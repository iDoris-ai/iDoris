#!/usr/bin/env python3
"""Stdlib tests for the Socket Mode listener. No Slack calls."""

from __future__ import annotations

import json
import os
import queue
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

import listen


def policy(**overrides) -> listen.Policy:
    base = dict(
        channel_ids={"C1"},
        channel_names={"C1": "pr"},
        self_ids={"UME", "UBOT"},
        bot_id="UBOT",
        mentions_only=False,
        allow_users=set(),
    )
    base.update(overrides)
    return listen.Policy(**base)


class FilterTest(unittest.TestCase):
    def test_keeps_a_colleague_message(self) -> None:
        record = listen.accept_event(
            {"type": "message", "channel": "C1", "user": "UOTHER", "text": "在吗", "ts": "1.2"},
            "Ev1",
            policy(),
            listen.Seen(),
        )
        self.assertIsNotNone(record)
        assert record is not None
        self.assertEqual(record["channel_name"], "pr")
        self.assertEqual(record["text"], "在吗")
        self.assertNotIn("token", record)

    def test_long_text_is_stored_whole(self) -> None:
        text = "长" * 9000
        record = listen.accept_event(
            {"type": "message", "channel": "C1", "user": "UOTHER", "text": text, "ts": "9.9"},
            "EvLong",
            policy(),
            listen.Seen(),
        )
        self.assertIsNotNone(record)
        assert record is not None
        self.assertEqual(record["text"], text)

    def test_drops_self_bot_other_channel_and_subtype(self) -> None:
        seen = listen.Seen()
        current = policy()
        self.assertIsNone(
            listen.accept_event(
                {"type": "message", "channel": "C1", "user": "UME", "text": "我", "ts": "2"},
                "Ev",
                current,
                seen,
            )
        )
        self.assertIsNone(
            listen.accept_event(
                {"type": "message", "channel": "C1", "user": "UOTHER", "text": "bot", "ts": "3", "bot_id": "B1"},
                "Ev",
                current,
                seen,
            )
        )
        self.assertIsNone(
            listen.accept_event(
                {"type": "message", "channel": "C9", "user": "UOTHER", "text": "别处", "ts": "4"},
                "Ev",
                current,
                seen,
            )
        )
        self.assertIsNone(
            listen.accept_event(
                {
                    "type": "message",
                    "subtype": "message_changed",
                    "channel": "C1",
                    "user": "UOTHER",
                    "text": "改",
                    "ts": "5",
                },
                "Ev",
                current,
                seen,
            )
        )

    def test_dedups_mention_and_message_with_the_same_ts(self) -> None:
        seen = listen.Seen()
        current = policy(mentions_only=True)
        first = listen.accept_event(
            {"type": "app_mention", "channel": "C1", "user": "UOTHER", "text": "<@UBOT> hi", "ts": "9.9"},
            "EvA",
            current,
            seen,
        )
        second = listen.accept_event(
            {"type": "message", "channel": "C1", "user": "UOTHER", "text": "<@UBOT> hi", "ts": "9.9"},
            "EvB",
            current,
            seen,
        )
        self.assertIsNotNone(first)
        self.assertIsNone(second)

    def test_mentions_only_drops_plain_text(self) -> None:
        record = listen.accept_event(
            {"type": "message", "channel": "C1", "user": "UOTHER", "text": "普通消息", "ts": "8"},
            "Ev",
            policy(mentions_only=True),
            listen.Seen(),
        )
        self.assertIsNone(record)

    def test_allow_users(self) -> None:
        current = policy(allow_users={"UKEEP"})
        self.assertIsNone(
            listen.accept_event(
                {"type": "message", "channel": "C1", "user": "UOTHER", "text": "不在名单", "ts": "7"},
                "Ev",
                current,
                listen.Seen(),
            )
        )
        self.assertIsNotNone(
            listen.accept_event(
                {"type": "message", "channel": "C1", "user": "UKEEP", "text": "在名单", "ts": "7.1"},
                "Ev",
                current,
                listen.Seen(),
            )
        )


class RedactTest(unittest.TestCase):
    def test_uncommitted_event_can_be_accepted_again(self) -> None:
        seen = listen.Seen()
        event = {"type": "message", "channel": "C1", "user": "UOTHER", "text": "再试", "ts": "3.3"}
        current = policy()
        first = listen.accept_event(event, "Ev", current, seen, remember=False)
        second = listen.accept_event(event, "Ev", current, seen, remember=False)
        self.assertIsNotNone(first)
        self.assertIsNotNone(second)
        seen.add("C1:3.3")
        self.assertIsNone(listen.accept_event(event, "Ev", current, seen, remember=False))

    def test_trigger_stderr_is_redacted(self) -> None:
        proc = mock.Mock()
        proc.pid = 42
        proc.returncode = 1
        proc.communicate.return_value = (b"", b"leaked xoxb-1-SECRET and wss://example.slack.com/link/?ticket=SECRET")
        with mock.patch.object(listen.subprocess, "Popen", return_value=proc):
            with mock.patch.object(listen, "log") as logged:
                listen.run_trigger("/bin/echo", {"text": "hi"}, 1)
        line = logged.call_args[0][0]
        self.assertNotIn("SECRET", line)
        self.assertIn("xox-…", line)

    def test_safe_error_hides_tokens_and_socket_url(self) -> None:
        text = listen.safe_error(
            RuntimeError("bad wss://wss-primary.slack.com/link/?ticket=secret xapp-1-abc xoxb-1-secret")
        )
        self.assertNotIn("secret", text)
        self.assertNotIn("xoxb-1", text)
        self.assertIn("wss://…", text)


class SpoolTest(unittest.TestCase):
    def test_spool_is_private_and_has_no_token_field(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp) / "state"
            listen.append_spool(
                directory,
                {"channel": "C1", "user": "UOTHER", "text": "你好", "ts": "1"},
            )
            path = directory / "events.jsonl"
            self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)
            saved = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(saved["text"], "你好")
            self.assertNotIn("token", saved)

    def test_short_write_rolls_back_before_retry(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp) / "state"
            listen.append_spool(directory, {"text": "first"})
            path = directory / "events.jsonl"
            before = path.read_bytes()
            real_write = os.write
            calls = {"n": 0}

            def short_then_fail(fd: int, data: bytes) -> int:
                calls["n"] += 1
                if calls["n"] == 1:
                    return real_write(fd, bytes(data[:1]))
                raise OSError(28, "No space left on device")

            with mock.patch.object(listen.os, "write", side_effect=short_then_fail):
                with self.assertRaises(OSError):
                    listen.append_spool(directory, {"text": "second-record"})
            self.assertEqual(path.read_bytes(), before)
            listen.append_spool(directory, {"text": "second-record"})
            lines = path.read_text(encoding="utf-8").splitlines()
            self.assertEqual([json.loads(line)["text"] for line in lines], ["first", "second-record"])


class ShutdownTest(unittest.TestCase):
    def tearDown(self) -> None:
        listen.stop.clear()
        proc = listen.current_trigger["proc"]
        listen.current_trigger["proc"] = None
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait(timeout=3)

    def test_shutdown_reaps_current_trigger_and_skips_the_queue(self) -> None:
        listen.stop.clear()
        with tempfile.TemporaryDirectory() as tmp:
            script = Path(tmp) / "sleep.sh"
            script.write_text("#!/bin/sh\nread line\nsleep 30\n", encoding="utf-8")
            script.chmod(0o700)
            events: queue.Queue = queue.Queue()
            events.put({"text": "run"})
            events.put({"text": "skip"})
            config = listen.Config(["pr"], [], False, str(script), 30, Path(tmp))
            worker = threading.Thread(target=listen.trigger_worker, args=(events, config))
            worker.start()
            deadline = time.time() + 3
            while listen.current_trigger["proc"] is None and time.time() < deadline:
                time.sleep(0.02)
            proc = listen.current_trigger["proc"]
            self.assertIsNotNone(proc)
            assert proc is not None
            listen.stop.set()
            listen.stop_trigger_worker(worker, 1)
            self.assertFalse(worker.is_alive())
            self.assertIsNone(listen.current_trigger["proc"])
            self.assertIsNotNone(proc.poll())
            self.assertEqual(events.qsize(), 1)


class FrameTest(unittest.TestCase):
    def test_masked_and_plain_roundtrip(self) -> None:
        for mask in (False, True):
            raw = listen.encode_frame(1, "你好".encode(), mask=mask)
            ws = listen.WebSocket(_FakeSock(raw))
            self.assertEqual(ws.next_text(), "你好")

    def test_ping_is_answered_before_text(self) -> None:
        sock = _FakeSock(listen.encode_frame(9, b"xyz", mask=False) + listen.encode_frame(1, b"ok", mask=False))
        ws = listen.WebSocket(sock)
        self.assertEqual(ws.next_text(), "ok")
        _fin, opcode, payload = listen.WebSocket(_FakeSock(sock.sent)).read_frame()
        self.assertEqual(opcode, 10)
        self.assertEqual(payload, b"xyz")


class HandshakeTest(unittest.TestCase):
    def test_local_handshake_and_text_frame(self) -> None:
        server = socket.socket()
        server.bind(("127.0.0.1", 0))
        server.listen(1)
        port = server.getsockname()[1]

        def serve() -> None:
            conn, _addr = server.accept()
            data = b""
            while b"\r\n\r\n" not in data:
                data += conn.recv(4096)
            key = data.split(b"Sec-WebSocket-Key: ", 1)[1].split(b"\r\n", 1)[0].decode()
            accept = listen.expected_accept(key)
            conn.sendall(
                (
                    "HTTP/1.1 101 Switching Protocols\r\n"
                    "Upgrade: websocket\r\n"
                    "Connection: Upgrade\r\n"
                    f"Sec-WebSocket-Accept: {accept}\r\n"
                    "\r\n"
                ).encode()
                + listen.encode_frame(1, b'{"type":"hello","num_connections":1}', mask=False)
            )
            conn.close()

        thread = threading.Thread(target=serve)
        thread.start()
        ws = listen.open_websocket(f"ws://127.0.0.1:{port}/link", timeout=2)
        self.assertIn("hello", ws.next_text())
        ws.close()
        thread.join(3)
        server.close()


class ConfigTest(unittest.TestCase):
    def test_load_config_defaults_and_override(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "listen.json"
            path.write_text(
                json.dumps(
                    {
                        "channels": ["pr"],
                        "users": ["David"],
                        "mentions_only": True,
                        "trigger": "",
                        "trigger_timeout_seconds": 12,
                    }
                ),
                encoding="utf-8",
            )
            old = os.environ.get("SLACK_LISTEN_CONFIG")
            os.environ["SLACK_LISTEN_CONFIG"] = str(path)
            try:
                config = listen.load_config()
            finally:
                if old is None:
                    os.environ.pop("SLACK_LISTEN_CONFIG", None)
                else:
                    os.environ["SLACK_LISTEN_CONFIG"] = old
        self.assertEqual(config.channels, ["pr"])
        self.assertEqual(config.users, ["David"])
        self.assertTrue(config.mentions_only)
        self.assertEqual(config.trigger_timeout, 12)


class _FakeSock:
    def __init__(self, data: bytes) -> None:
        self.data = data
        self.sent = b""

    def recv(self, size: int) -> bytes:
        if not self.data:
            return b""
        out, self.data = self.data[:size], self.data[size:]
        return out

    def sendall(self, data: bytes) -> None:
        self.sent += data

    def close(self) -> None:
        return

    def shutdown(self, _how: int) -> None:
        return


if __name__ == "__main__":
    unittest.main()
