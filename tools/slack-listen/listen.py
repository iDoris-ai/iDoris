#!/usr/bin/env python3
"""Local Socket Mode listener. It does not call a model and does not post to Slack.

Secrets stay in ~/.config/grok-slack/env. This process writes events to a local
spool and, when configured, runs one executable with the event JSON on stdin.
"""

from __future__ import annotations

import base64
import fcntl
import hashlib
import json
import os
import queue
import re
import signal
import socket
import ssl
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_FRAME = 1_000_000
ENV_PATH = Path(os.environ.get("SLACK_ENV_FILE", str(Path.home() / ".config" / "grok-slack" / "env")))
DEFAULT_SPOOL = Path.home() / ".local" / "state" / "slack-listen"
SLACK_API = "https://slack.com/api/"

stop = threading.Event()
current_sock: dict[str, socket.socket | None] = {"sock": None}


class Disconnect(RuntimeError):
    pass


class Seen:
    def __init__(self, cap: int = 1000) -> None:
        self.cap = cap
        self.items: dict[str, None] = {}

    def has(self, key: str) -> bool:
        return not key or key in self.items

    def add(self, key: str) -> bool:
        if self.has(key):
            return False
        self.items[key] = None
        while len(self.items) > self.cap:
            self.items.pop(next(iter(self.items)))
        return True


class Policy:
    def __init__(
        self,
        channel_ids: set[str],
        channel_names: dict[str, str],
        self_ids: set[str],
        bot_id: str,
        mentions_only: bool,
        allow_users: set[str],
    ) -> None:
        self.channel_ids = channel_ids
        self.channel_names = channel_names
        self.self_ids = self_ids
        self.bot_id = bot_id
        self.mentions_only = mentions_only
        self.allow_users = allow_users


class Config:
    def __init__(
        self,
        channels: list[str],
        users: list[str],
        mentions_only: bool,
        trigger: str,
        trigger_timeout: float,
        spool_dir: Path,
    ) -> None:
        self.channels = channels
        self.users = users
        self.mentions_only = mentions_only
        self.trigger = trigger
        self.trigger_timeout = trigger_timeout
        self.spool_dir = spool_dir


def log(text: str) -> None:
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S")
    print(f"{stamp} {text}", flush=True)


def redact(text: str) -> str:
    text = re.sub(r"wss://\S+", "wss://…", text)
    text = re.sub(r"xapp-[A-Za-z0-9-]+", "xapp-…", text)
    text = re.sub(r"xox[baprs]-[A-Za-z0-9-]+", "xox-…", text)
    return text[:300]


def safe_error(exc: BaseException) -> str:
    return redact(str(exc))


def load_env(path: Path = ENV_PATH) -> dict[str, str]:
    if not path.exists():
        return {}
    values: dict[str, str] = {}
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip().strip("'").strip('"')
    return values


def load_config() -> Config:
    path = Path(
        os.environ.get("SLACK_LISTEN_CONFIG", str(Path.home() / ".config" / "grok-slack" / "listen.json"))
    )
    data: dict = {}
    if path.exists():
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            raise RuntimeError(f"配置必须是对象：{path}")
    channels = data.get("channels") or ["pr"]
    users = data.get("users") or []
    if not isinstance(channels, list) or not all(isinstance(item, str) and item.strip() for item in channels):
        raise RuntimeError("channels 必须是非空字符串数组")
    if not isinstance(users, list) or not all(isinstance(item, str) for item in users):
        raise RuntimeError("users 必须是字符串数组")
    trigger = str(data.get("trigger") or "")
    timeout = float(data.get("trigger_timeout_seconds") or 30)
    timeout = min(3600.0, max(1.0, timeout))
    spool = str(data.get("spool_dir") or "")
    spool_dir = Path(spool).expanduser() if spool else DEFAULT_SPOOL
    return Config(
        channels=[item.strip() for item in channels],
        users=[item.strip() for item in users if item.strip()],
        mentions_only=bool(data.get("mentions_only") or False),
        trigger=trigger,
        trigger_timeout=timeout,
        spool_dir=spool_dir,
    )


def slack_form(method: str, token: str, form: dict[str, str]) -> dict:
    payload = urllib.parse.urlencode(form).encode("utf-8")
    request = urllib.request.Request(
        SLACK_API + method,
        data=payload,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/x-www-form-urlencoded",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            body = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")[:180]
        raise RuntimeError(f"Slack HTTP {exc.code}: {detail}") from exc
    except urllib.error.URLError as exc:
        raise RuntimeError(f"连不上 slack.com：{exc.reason}") from exc
    if not body.get("ok"):
        raise RuntimeError(f"Slack {method} 失败：{body.get('error', 'unknown')}")
    return body


def slack_pages(method: str, token: str, form: dict[str, str], key: str) -> list:
    items: list = []
    cursor = ""
    for _ in range(50):
        body = dict(form)
        if cursor:
            body["cursor"] = cursor
        result = slack_form(method, token, body)
        items.extend(result.get(key) or [])
        cursor = (result.get("response_metadata") or {}).get("next_cursor") or ""
        if not cursor:
            return items
    raise RuntimeError(f"Slack {method} 超过 50 页，停止使用残缺结果")


def looks_like_id(value: str, prefix: str) -> bool:
    return value.startswith(prefix) and value[1:].isalnum()


def resolve_channels(token: str, wanted: list[str]) -> tuple[set[str], dict[str, str]]:
    names = [item for item in wanted if not looks_like_id(item, "C") and not looks_like_id(item, "G")]
    rows = []
    if names:
        rows = slack_pages(
            "conversations.list",
            token,
            {"types": "public_channel,private_channel", "exclude_archived": "true", "limit": "200"},
            "channels",
        )
    by_name = {(row.get("name") or "").lower(): row for row in rows}
    ids: set[str] = set()
    labels: dict[str, str] = {}
    for item in wanted:
        if looks_like_id(item, "C") or looks_like_id(item, "G"):
            ids.add(item)
            labels.setdefault(item, item)
            continue
        row = by_name.get(item.lower().lstrip("#"))
        if not row:
            raise RuntimeError(f"找不到频道 {item}。确认账号在这个频道里。")
        ids.add(row["id"])
        labels[row["id"]] = row.get("name") or item
    return ids, labels


def resolve_users(token: str, wanted: list[str]) -> set[str]:
    if not wanted:
        return set()
    rows = slack_pages("users.list", token, {"limit": "200"}, "members")
    found: set[str] = set()
    for item in wanted:
        if looks_like_id(item, "U"):
            found.add(item)
            continue
        needle = item.lstrip("@").lower()
        hits = []
        for member in rows:
            if member.get("deleted"):
                continue
            profile = member.get("profile") or {}
            names = {
                (member.get("name") or "").lower(),
                (member.get("real_name") or "").lower(),
                (profile.get("display_name") or "").lower(),
                (profile.get("real_name") or "").lower(),
            }
            if needle in names:
                hits.append(member["id"])
        if len(hits) != 1:
            raise RuntimeError(f"找不到唯一的用户 {item}")
        found.add(hits[0])
    return found


def resolve_policy(env: dict[str, str], config: Config) -> Policy:
    user_token = env["SLACK_USER_TOKEN"]
    bot_token = env["SLACK_BOT_TOKEN"]
    me = slack_form("auth.test", user_token, {})
    bot = slack_form("auth.test", bot_token, {})
    self_ids = {item for item in (me.get("user_id"), bot.get("user_id")) if item}
    channel_ids, channel_names = resolve_channels(user_token, config.channels)
    allow_users = resolve_users(user_token, config.users)
    if config.trigger and not os.access(config.trigger, os.X_OK):
        raise RuntimeError(f"触发命令不可执行：{config.trigger}")
    return Policy(
        channel_ids=channel_ids,
        channel_names=channel_names,
        self_ids=self_ids,
        bot_id=str(bot.get("user_id") or ""),
        mentions_only=config.mentions_only,
        allow_users=allow_users,
    )


def accept_event(
    event: dict, event_id: str, policy: Policy, seen: Seen, *, remember: bool = True
) -> dict | None:
    kind = event.get("type")
    if kind not in ("message", "app_mention"):
        return None
    if event.get("subtype"):
        return None
    if event.get("bot_id"):
        return None
    user = str(event.get("user") or "")
    channel = str(event.get("channel") or "")
    if not user or user in policy.self_ids or user == "USLACKBOT":
        return None
    if channel not in policy.channel_ids:
        return None
    if policy.allow_users and user not in policy.allow_users:
        return None
    text = str(event.get("text") or "")
    if policy.mentions_only and kind != "app_mention" and f"<@{policy.bot_id}>" not in text:
        return None
    ts = str(event.get("ts") or event.get("event_ts") or "")
    key = f"{channel}:{ts}" if ts else event_id
    if seen.has(key):
        return None
    if remember:
        seen.add(key)
    return {
        "type": kind,
        "channel": channel,
        "channel_name": policy.channel_names.get(channel, ""),
        "user": user,
        "text": text[:8000],
        "ts": ts,
        "thread_ts": str(event.get("thread_ts") or ""),
        "event_id": event_id,
        "received_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def ensure_dir(directory: Path) -> None:
    directory.mkdir(parents=True, mode=0o700, exist_ok=True)
    os.chmod(directory, 0o700)


def append_spool(directory: Path, record: dict) -> None:
    ensure_dir(directory)
    path = directory / "events.jsonl"
    if path.exists() and path.stat().st_size > 1_000_000:
        rotated = directory / "events.jsonl.1"
        if rotated.exists():
            rotated.unlink()
        path.replace(rotated)
    line = (json.dumps(record, ensure_ascii=False) + "\n").encode("utf-8")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        view = memoryview(line)
        while view:
            written = os.write(fd, view)
            if written <= 0:
                raise OSError("事件记录写入失败")
            view = view[written:]
        os.fsync(fd)
    finally:
        os.close(fd)


def write_status(directory: Path, **fields: str) -> None:
    ensure_dir(directory)
    path = directory / "status.json"
    current: dict = {}
    if path.exists():
        try:
            current = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            current = {}
    current.update(fields)
    tmp = directory / "status.json.tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        json.dump(current, handle, ensure_ascii=False)
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)


def acquire_lock(directory: Path):
    ensure_dir(directory)
    fd = os.open(directory / "listen.lock", os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        return None
    return fd


def run_trigger(path: str, record: dict, timeout: float) -> None:
    proc = subprocess.Popen(
        [path],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    payload = json.dumps(record, ensure_ascii=False).encode("utf-8")
    try:
        _out, err = proc.communicate(payload, timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate()
        log(f"触发超时，已终止：{path}")
        return
    if proc.returncode != 0:
        detail = redact(err.decode("utf-8", errors="replace").strip().replace("\n", " "))
        log(f"触发退出 {proc.returncode}：{detail}")


def trigger_worker(events: queue.Queue, config: Config) -> None:
    while not stop.is_set() or not events.empty():
        try:
            record = events.get(timeout=0.5)
        except queue.Empty:
            continue
        try:
            run_trigger(config.trigger, record, config.trigger_timeout)
        except OSError as exc:
            log(f"触发没有跑起来：{safe_error(exc)}")
        finally:
            events.task_done()


def expected_accept(key: str) -> str:
    digest = hashlib.sha1((key + WS_GUID).encode("ascii")).digest()
    return base64.b64encode(digest).decode("ascii")


def encode_frame(opcode: int, payload: bytes, *, mask: bool) -> bytes:
    first = 0x80 | (opcode & 0x0F)
    length = len(payload)
    mask_bit = 0x80 if mask else 0
    if length < 126:
        header = bytes([first, mask_bit | length])
    elif length < 65536:
        header = bytes([first, mask_bit | 126]) + struct.pack("!H", length)
    else:
        header = bytes([first, mask_bit | 127]) + struct.pack("!Q", length)
    if not mask:
        return header + payload
    key = os.urandom(4)
    masked = bytes(byte ^ key[index % 4] for index, byte in enumerate(payload))
    return header + key + masked


class WebSocket:
    def __init__(self, sock: socket.socket, leftover: bytes = b"") -> None:
        self.sock = sock
        self.buf = leftover

    def read_exact(self, size: int) -> bytes:
        while len(self.buf) < size:
            chunk = self.sock.recv(max(4096, size - len(self.buf)))
            if not chunk:
                raise EOFError("连接已断开")
            self.buf += chunk
        out, self.buf = self.buf[:size], self.buf[size:]
        return out

    def read_frame(self) -> tuple[bool, int, bytes]:
        header = self.read_exact(2)
        fin = (header[0] & 0x80) != 0
        opcode = header[0] & 0x0F
        masked = (header[1] & 0x80) != 0
        length = header[1] & 0x7F
        if length == 126:
            length = struct.unpack("!H", self.read_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self.read_exact(8))[0]
        if length > MAX_FRAME:
            raise RuntimeError("帧太大")
        key = self.read_exact(4) if masked else b""
        payload = self.read_exact(length)
        if masked:
            payload = bytes(byte ^ key[index % 4] for index, byte in enumerate(payload))
        return fin, opcode, payload

    def send_frame(self, opcode: int, payload: bytes) -> None:
        self.sock.sendall(encode_frame(opcode, payload, mask=True))

    def send_text(self, text: str) -> None:
        self.send_frame(1, text.encode("utf-8"))

    def next_text(self) -> str:
        parts: list[bytes] = []
        while True:
            fin, opcode, payload = self.read_frame()
            if opcode == 9:
                self.send_frame(10, payload)
                continue
            if opcode == 10:
                continue
            if opcode == 8:
                try:
                    self.send_frame(8, payload[:125])
                except OSError:
                    pass
                raise EOFError("对端关闭")
            if opcode == 0:
                parts.append(payload)
            elif opcode == 1:
                parts = [payload]
            else:
                raise RuntimeError(f"不支持的帧 {opcode}")
            if fin:
                return b"".join(parts).decode("utf-8")

    def close(self) -> None:
        try:
            self.send_frame(8, b"")
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass


def open_websocket(url: str, timeout: float = 90) -> WebSocket:
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme not in ("ws", "wss") or not parsed.hostname:
        raise RuntimeError("Socket Mode 地址无效")
    port = parsed.port or (443 if parsed.scheme == "wss" else 80)
    path = parsed.path or "/"
    if parsed.query:
        path = f"{path}?{parsed.query}"
    raw = socket.create_connection((parsed.hostname, port), timeout=timeout)
    raw.settimeout(timeout)
    try:
        if parsed.scheme == "wss":
            raw = ssl.create_default_context().wrap_socket(raw, server_hostname=parsed.hostname)
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        host_header = parsed.hostname if port in (80, 443) else f"{parsed.hostname}:{port}"
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host_header}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        raw.sendall(request.encode("ascii"))
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = raw.recv(4096)
            if not chunk:
                raise EOFError("握手中断")
            data += chunk
            if len(data) > 65536:
                raise RuntimeError("握手响应过长")
        header, rest = data.split(b"\r\n\r\n", 1)
        status = header.split(b"\r\n", 1)[0]
        if b" 101 " not in status:
            raise RuntimeError(f"WebSocket 握手失败：{status.decode(errors='replace')[:120]}")
        if expected_accept(key).encode("ascii") not in header:
            raise RuntimeError("WebSocket 握手校验失败")
        return WebSocket(raw, rest)
    except Exception:
        raw.close()
        raise


def dispatch(message: dict, policy: Policy, seen: Seen, config: Config, events: queue.Queue | None) -> None:
    payload = message.get("payload") or {}
    event = payload.get("event") or {}
    if not isinstance(event, dict):
        return
    record = accept_event(event, str(payload.get("event_id") or ""), policy, seen, remember=False)
    if record is None:
        return
    append_spool(config.spool_dir, record)
    dedupe_key = f"{record['channel']}:{record['ts']}" if record["ts"] else record["event_id"]
    seen.add(dedupe_key)
    write_status(config.spool_dir, last_event_at=record["received_at"], last_event_ts=record["ts"])
    where = record["channel_name"] or record["channel"]
    log(f"收到 #{where} user={record['user']} ts={record['ts']} 字数={len(record['text'])}")
    if events is None:
        return
    try:
        events.put_nowait(record)
    except queue.Full:
        log("触发排队已满，这条只留在本地记录里")


def run_socket(
    url: str,
    policy: Policy,
    seen: Seen,
    config: Config,
    events: queue.Queue | None,
    health: dict | None = None,
) -> None:
    ws = open_websocket(url)
    current_sock["sock"] = ws.sock
    try:
        while not stop.is_set():
            try:
                text = ws.next_text()
            except TimeoutError:
                raise EOFError("空闲过久") from None
            except OSError:
                if stop.is_set():
                    return
                raise
            try:
                message = json.loads(text)
            except json.JSONDecodeError:
                log("收到无法解析的帧")
                continue
            if not isinstance(message, dict):
                continue
            kind = message.get("type")
            if kind == "hello":
                if health is not None:
                    health["hello"] = True
                log(f"Socket Mode 就绪，连接数 {message.get('num_connections')}")
                continue
            if kind == "disconnect":
                raise Disconnect(str(message.get("reason") or "disconnect"))
            if kind == "events_api":
                dispatch(message, policy, seen, config, events)
            envelope = message.get("envelope_id")
            if envelope:
                ws.send_text(json.dumps({"envelope_id": envelope}))
    finally:
        current_sock["sock"] = None
        ws.close()


def open_connection(app_token: str) -> str:
    body = slack_form("apps.connections.open", app_token, {})
    url = str(body.get("url") or "")
    if not url.startswith("wss://"):
        raise RuntimeError("Slack 没有返回 Socket Mode 地址")
    return url


def missing_tokens(env: dict[str, str]) -> list[str]:
    return [key for key in ("SLACK_APP_TOKEN", "SLACK_USER_TOKEN", "SLACK_BOT_TOKEN") if not env.get(key)]


def handle_signal(_signum: int, _frame) -> None:
    stop.set()
    sock = current_sock["sock"]
    if sock is not None:
        try:
            sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass


def main(argv: list[str]) -> int:
    check_only = "--check" in argv
    env = load_env()
    missing = missing_tokens(env)
    if missing:
        log("缺少 " + "、".join(missing) + "。监听没有启动。")
        if "SLACK_APP_TOKEN" in missing:
            log("在 Slack 应用里打开 Socket Mode，创建 App-Level Token，scope 选 connections:write，写入 SLACK_APP_TOKEN。不要把令牌贴进对话。")
        return 2
    if not env["SLACK_APP_TOKEN"].startswith("xapp-"):
        log("SLACK_APP_TOKEN 必须以 xapp- 开头。Bot 令牌不能开 Socket Mode。")
        return 2
    config = load_config()
    if check_only:
        policy = resolve_policy(env, config)
        names = ", ".join(sorted(set(policy.channel_names.values())))
        trigger = "有命令" if config.trigger else "只写本地记录"
        log(f"配置可用。频道 {names}。触发：{trigger}。")
        return 0
    lock = acquire_lock(config.spool_dir)
    if lock is None:
        log("已经有一个监听在跑。")
        return 0
    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)
    events: queue.Queue | None = None
    worker = None
    if config.trigger:
        events = queue.Queue(maxsize=20)
        worker = threading.Thread(target=trigger_worker, args=(events, config), daemon=True)
        worker.start()
    seen = Seen()
    health = {"hello": False}
    delay = 1
    log(f"监听进程启动。记录目录 {config.spool_dir}。不调用模型，不发 Slack。")
    while not stop.is_set():
        health["hello"] = False
        try:
            policy = resolve_policy(env, config)
            url = open_connection(env["SLACK_APP_TOKEN"])
            write_status(config.spool_dir, last_connect_at=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), last_error="")
            run_socket(url, policy, seen, config, events, health)
            delay = 1
        except Disconnect as exc:
            log(f"Slack 要求重连：{safe_error(exc)}")
            write_status(config.spool_dir, last_error=safe_error(exc))
            delay = 1 if health["hello"] else min(delay * 2, 60)
        except Exception as exc:
            log(f"连接中断：{safe_error(exc)}")
            write_status(config.spool_dir, last_error=safe_error(exc))
            delay = 1 if health["hello"] else min(delay * 2, 60)
        if stop.is_set() or stop.wait(delay):
            break
    if worker is not None:
        worker.join(timeout=config.trigger_timeout)
    log("监听进程退出。")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except KeyboardInterrupt:
        raise SystemExit(0)
