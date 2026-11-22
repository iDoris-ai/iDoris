//! 假的 oMLX 引擎（IDORIS-CORE S5 的测试夹具）：一个监听 loopback 端口、认私有 key 的 HTTP 服务，外面套一个 shell 包装，
//! 作为 `omlx-cli` 交给 iDoris 拉起。**绝不加载任何模型、不启动任何推理进程**：它只回答 oMLX 的三个状态接口。
//!
//! 引擎的环境是清洗过的（只有 `PATH`、`HOME` 等白名单变量加 `OMLX_API_KEY`、`OMLX_BASE_PATH`），所以行为旋钮都在生成脚本时写死。
//! 每次启动记下 argv 与环境变量名（`launch-<序号>.json`），每个请求记下路径与有没有带对的 key（`requests.log`），
//! 测试据此断言「参数每次都传」「环境不泄露」「没有 key 的请求被拒」。
//!
//! 需要 `/usr/bin/python3`（macOS 与常见的 Linux 都有；只用标准库）。生成的脚本写完立刻交给 `Command::spawn` 时，
//! Linux 上可能 `ETXTBSY`（别的线程 fork 时写端还没关）；iDoris 的管理器对此重试，测试里直接 `spawn` 的地方用 [`spawn_retrying`]。
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::child_guard::{ChildGuard, SpawnGuarded};

/// 假引擎的行为。
#[derive(Clone, Debug, Default)]
pub struct Behavior {
    /// 启动之后先睡这么久才开始监听（模拟冷启动慢）。
    pub listen_delay_secs: f64,
    /// 永不监听（进程活着，端口上没有它）。
    pub never_listen: bool,
    /// 启动之后这么久自己退出（退出码 3，模拟引擎崩溃）；0 = 不退出。
    pub exit_after_secs: f64,
    /// 一启动就以这个退出码退出，不监听（模拟引擎起不来）；`None` = 正常。
    pub exit_immediately: Option<i32>,
    /// 收到 SIGTERM 时先写一个标记文件（`sigterm-<pid>`，证明走的是优雅停止，不是 SIGKILL），再继续服务这么久才退出（0 = 立刻
    /// 退出）；`None` = 不接管 SIGTERM（默认的终止行为）。
    pub trap_sigterm_secs: Option<f64>,
    /// 有状态的模型接口（IDORIS-CORE S6）：目录里一个模型 `fake-model`，`POST /v1/models/fake-model/load|unload` 改变**这个进程**
    /// 里的已加载集合，`/v1/models/status`、`/api/status` 如实报告，`POST /v1/chat/completions` 只在它已加载时才 200（真的 oMLX
    /// 会按需加载；这里故意不，这样账本过期的聊天会真的失败）。进程换了，已加载集合就空了。带 key 的 POST 也记进 `requests.log`
    /// （路径前加 `POST:`）。`false` = 只有 S5 的三个状态接口。
    pub stateful_models: bool,
    /// 第 `n` 次（从 0 起）及以后拉起的进程，`GET /api/status` 回 500（引擎起来了、`/v1/models` 也答，但 Supervisor 采样它的
    /// 驻留失败：模拟引擎换代之后账本复位采样不到新进程）。`None` = 从不。
    pub api_status_fails_from_launch: Option<usize>,
    /// 一个最小的 Admin API（IDORIS-CORE S6，Codex 终审 H4）：`POST /admin/api/login`（key 对就发一个**这个进程独有**的会话 cookie）与
    /// `GET /admin/api/activity`（要有效的 cookie，否则 401）。每个 Admin 请求记进 `admin.log`：`<pid> <方法> <路径> <none|ok|bad>`（带的
    /// cookie 是不是这个进程签发的），测试据此断言「旧进程的会话 cookie 没有发给新进程」。`false` = 没有 `/admin/api/*`（404）。
    pub admin_api: bool,
}

/// 引擎可以「收下请求、记进 `requests.log`，然后不回答」的几类请求（IDORIS-CORE S6，[`FakeOmlx::hold`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// `POST /v1/chat/completions`：一个在途的聊天。
    Chat,
    /// `POST /v1/models/<id>/load`：一个已经发给引擎、还在加载的模型。
    Load,
    /// `GET /api/status`：Supervisor 的复位采样（引擎起来之后对账本重建的那一次读）。
    Status,
    /// `GET /v1/models/status`：模型目录（聊天、预热在加载之前读的那一步）。
    List,
    /// `GET /v1/models`：公共的模型列表对这个引擎的读取（管理器自己的探测也读这个路径：用例要把监督的节拍拉到小时级）。
    Models,
    /// `POST /v1/embeddings`：一个经代理（resident 的伴生卡、后台预热）转发、还在途的请求（IDORIS-CORE S6，Codex 终审 M1）。
    Embeddings,
    /// 流式的 `POST /v1/chat/completions`：响应头与第一块已经发出，之后的字节被扣着（一个仍在输出的流式响应体）。
    Stream,
}

impl Hold {
    fn file(self) -> &'static str {
        match self {
            Self::Chat => "hold-chat",
            Self::Load => "hold-load",
            Self::Status => "hold-status",
            Self::List => "hold-list",
            Self::Models => "hold-models",
            Self::Embeddings => "hold-embeddings",
            Self::Stream => "hold-stream",
        }
    }
}

/// 一条 Admin 请求的记录（[`FakeOmlx::admin_requests`]）：`cookie` 是 `none`（没带）、`ok`（是这个进程签发的会话）、`bad`（别的进程签发的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminRequest {
    pub pid: u64,
    pub method: String,
    pub path: String,
    pub cookie: String,
}

/// 一个生成好的假引擎。
pub struct FakeOmlx {
    /// 交给 iDoris 的「`omlx-cli`」（shell 包装，`exec` 成 python，所以只有一个进程）。
    pub executable: PathBuf,
    /// 启动记录与请求记录所在的目录。
    pub record_dir: PathBuf,
    script: PathBuf,
}

impl FakeOmlx {
    /// 在 `dir` 里生成 `omlx-cli` 与 `fake_omlx.py`，记录写在 `dir/records`。
    pub fn create(dir: &Path, behavior: &Behavior) -> Self {
        let record_dir = dir.join("records");
        std::fs::create_dir_all(&record_dir).unwrap();
        let script = dir.join("fake_omlx.py");
        std::fs::write(&script, python_source(behavior, &record_dir)).unwrap();
        let executable = dir.join("omlx-cli");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nexec /usr/bin/python3 -B '{}' \"$@\"\n",
                script.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            executable,
            record_dir,
            script,
        }
    }

    /// 换掉之后新启动的引擎的行为（已经在跑的不受影响）：原子地换 python 脚本（先写临时文件再改名），
    /// 所以正好有一次启动在这时 `exec` 它，也只会读到旧的或新的完整脚本。包装脚本不动。
    pub fn set_behavior(&self, behavior: &Behavior) {
        let temporary = self.script.with_extension("py.tmp");
        std::fs::write(&temporary, python_source(behavior, &self.record_dir)).unwrap();
        std::fs::rename(&temporary, &self.script).unwrap();
    }

    /// 从现在起引擎对每个 GET 都回这个状态码（进程活着、端口在听，但「不好用」：503 = 忙，401 = 不认我们的 key，
    /// 200 = 答的不是模型列表），直到 [`Self::answer_normally`]。运行期切换，不用重启引擎。
    pub fn force_status(&self, code: u16) {
        std::fs::write(self.record_dir.join("force-status"), code.to_string()).unwrap();
    }

    /// 取消 [`Self::force_status`]。
    pub fn answer_normally(&self) {
        let _ = std::fs::remove_file(self.record_dir.join("force-status"));
    }

    /// 从现在起，引擎对 `what` 这一类请求**先收下并记进 `requests.log`，然后不回答**，直到 [`Self::release`]（运行期切换，对所有
    /// 进程生效，包括之后拉起的）。测试靠 `requests.log` 里这条请求出现，知道它「已经在引擎里、还没做完」。
    pub fn hold(&self, what: Hold) {
        std::fs::write(self.record_dir.join(what.file()), "1").unwrap();
    }

    /// 让引擎（此刻在跑的、之后拉起的）把它监听的端口放掉、但**不退出**：进程活着，端口空了，别的东西可以绑上来（IDORIS-CORE S6，
    /// Codex 审计 H3：进程活着不等于端口还是它的）。已经接受的连接不受影响。
    pub fn release_port(&self) {
        std::fs::write(self.record_dir.join("release-port"), "1").unwrap();
    }

    /// 放开 [`Self::hold`] 的那一类请求：被扣着的请求这就回答。
    pub fn release(&self, what: Hold) {
        let _ = std::fs::remove_file(self.record_dir.join(what.file()));
    }

    /// `requests.log` 里路径是 `path` 的请求有几条（`POST:` 前缀的是 POST）。
    pub fn requests_of(&self, path: &str) -> usize {
        self.requests().iter().filter(|(p, _)| p == path).count()
    }

    /// 已经启动过几次（`launch-*.json` 的个数）。
    pub fn launches(&self) -> usize {
        self.launch_files().len()
    }

    fn launch_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.record_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with("launch-") && name.ends_with(".json")
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// 第 `index` 次（从 0 起）启动的记录：`{"argv": [...], "env": [名字...], "env_values": {...}, "pid": N}`。
    pub fn launch(&self, index: usize) -> serde_json::Value {
        let files = self.launch_files();
        let text = std::fs::read_to_string(&files[index]).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// 全部启动记录，按启动顺序。
    pub fn all_launches(&self) -> Vec<serde_json::Value> {
        (0..self.launches())
            .map(|index| self.launch(index))
            .collect()
    }

    /// 第 `pid` 号引擎进程收到过 SIGTERM（它的 `trap_sigterm_secs` 写下的标记）。
    pub fn got_sigterm(&self, pid: u64) -> bool {
        self.record_dir.join(format!("sigterm-{pid}")).exists()
    }

    /// 有状态的引擎（[`Behavior::stateful_models`]）收到过几次 `POST /v1/models/<id>/load`。
    pub fn model_loads(&self) -> usize {
        self.requests()
            .iter()
            .filter(|(path, _)| path.starts_with("POST:/v1/models/") && path.ends_with("/load"))
            .count()
    }

    /// Admin 请求记录（[`Behavior::admin_api`]）：哪个进程收到了什么、带的 cookie 是不是它自己签发的。
    pub fn admin_requests(&self) -> Vec<AdminRequest> {
        std::fs::read_to_string(self.record_dir.join("admin.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let mut parts = line.split(' ');
                Some(AdminRequest {
                    pid: parts.next()?.parse().ok()?,
                    method: parts.next()?.to_owned(),
                    path: parts.next()?.to_owned(),
                    cookie: parts.next()?.to_owned(),
                })
            })
            .collect()
    }

    /// 请求记录：每行 `<路径> <none|ok|bad>`（有没有带 `Authorization`，带的是不是引擎的私有 key）。
    pub fn requests(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.record_dir.join("requests.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (path, auth) = line.split_once(' ')?;
                Some((path.to_owned(), auth.to_owned()))
            })
            .collect()
    }
}

/// 命令行里带着 `dir`（这个测试自己的临时目录）与假引擎脚本名的进程，一律 SIGKILL：iDoris 被 SIGKILL 时来不及停的引擎、
/// 或者还没来得及登记 pid 的，不会在测试结束后留下来占着端口。目录是每个测试独有的，所以不会碰到别的测试或别的东西。
pub fn kill_fake_engines_under(dir: &Path) {
    let needle = dir.to_string_lossy().into_owned();
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,command="])
        .output()
    else {
        return;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.contains("fake_omlx.py")
            && line.contains(&needle)
            && let Some(pid) = line.split_whitespace().next()
        {
            let _ = Command::new("kill")
                .args(["-KILL", pid])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// 起一个普通子进程，对 `ETXTBSY` 重试（刚写好的脚本，Linux 上偶发）。返回的是被守卫的子进程（`spawn_guarded`）：测试失败、
/// panic 展开时也会被杀掉并收尸，不会留下占着端口的假引擎。
pub fn spawn_retrying(command: &mut Command) -> std::io::Result<ChildGuard> {
    let mut attempt = 0;
    loop {
        match command.spawn_guarded() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 100 =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

fn python_source(behavior: &Behavior, record_dir: &Path) -> String {
    format!(
        r#"import http.server, json, os, signal, sys, threading, time

RECORDS = {records}
LISTEN_DELAY = {delay}
NEVER_LISTEN = {never}
EXIT_AFTER = {exit_after}
EXIT_IMMEDIATELY = {exit_now}
TRAP_SIGTERM = {trap}
STATEFUL = {stateful}
STATUS_FAILS_FROM = {status_fails_from}
ADMIN_API = {admin_api}
MODEL = "fake-model"
SIZE = 1000000000
LOADED = set()

def arg(name):
    args = sys.argv[1:]
    return args[args.index(name) + 1] if name in args else None

def hold(name):
    # The test asked the engine to take a request and not answer it (until it removes the file).
    path = os.path.join(RECORDS, name)
    while os.path.exists(path):
        time.sleep(0.02)

# One record per launch: what the engine was started with. Numbered by the files already there.
os.makedirs(RECORDS, exist_ok=True)
index = len([n for n in os.listdir(RECORDS) if n.startswith("launch-")])
tmp = os.path.join(RECORDS, "tmp-%04d-%d" % (index, os.getpid()))
with open(tmp, "w") as f:
    json.dump({{
        "argv": sys.argv[1:],
        "env": sorted(os.environ.keys()),
        "env_values": {{k: os.environ[k] for k in ("OMLX_BASE_PATH", "HOME", "PATH") if k in os.environ}},
        "has_api_key": "OMLX_API_KEY" in os.environ,
        "api_key": os.environ.get("OMLX_API_KEY"),
        "env_all": dict(os.environ),
        "time": time.time(),
        "pid": os.getpid(),
        "pgid": os.getpgrp(),
        "cwd": os.getcwd(),
    }}, f)
os.rename(tmp, os.path.join(RECORDS, "launch-%04d.json" % index))

if EXIT_IMMEDIATELY is not None:
    sys.exit(EXIT_IMMEDIATELY)

KEY = os.environ.get("OMLX_API_KEY")
LOCK = threading.Lock()
# The admin session this process issues: another process never accepts it.
SESSION = "fake-session-%d-%s" % (os.getpid(), os.urandom(4).hex())

if TRAP_SIGTERM is not None:
    def on_term(signum, frame):
        with open(os.path.join(RECORDS, "sigterm-%d" % os.getpid()), "w") as marker:
            marker.write("1")
        if TRAP_SIGTERM <= 0:
            os._exit(0)
        threading.Timer(TRAP_SIGTERM, lambda: os._exit(0)).start()
    signal.signal(signal.SIGTERM, on_term)

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def cookie_state(self):
        cookie = self.headers.get("Cookie")
        if cookie is None:
            return "none"
        return "ok" if ("omlx_admin_session=" + SESSION) in cookie else "bad"

    def admin_log(self, method, state):
        with LOCK:
            with open(os.path.join(RECORDS, "admin.log"), "a") as f:
                f.write("%d %s %s %s\n" % (os.getpid(), method, self.path, state))

    def login(self, payload):
        self.admin_log("POST", "none")
        if KEY is not None and payload.get("api_key") != KEY:
            return self.reply(401, {{"error": "bad key"}})
        data = json.dumps({{"success": True}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Set-Cookie", "omlx_admin_session=" + SESSION + "; Path=/; HttpOnly")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def admin(self, method):
        state = self.cookie_state()
        self.admin_log(method, state)
        if state != "ok":
            return self.reply(401, {{"error": "not logged in"}})
        if self.path == "/admin/api/activity":
            return self.reply(200, {{"active_models": {{"memory_pressure": {{"enabled": False}}}}}})
        return self.reply(404, {{"error": "not found"}})

    def do_GET(self):
        if ADMIN_API and self.path.startswith("/admin/api/"):
            return self.admin("GET")
        header = self.headers.get("Authorization")
        if header is None:
            auth = "none"
        elif KEY is not None and header == "Bearer " + KEY:
            auth = "ok"
        else:
            auth = "bad"
        with LOCK:
            with open(os.path.join(RECORDS, "requests.log"), "a") as f:
                f.write("%s %s\n" % (self.path, auth))
        forced = os.path.join(RECORDS, "force-status")
        if os.path.exists(forced):
            with open(forced) as f:
                code = int(f.read().strip() or "503")
            return self.reply(code, {{"error": {{"message": "forced by the test"}}}})
        if KEY is not None and auth != "ok":
            return self.reply(401, {{"error": {{"message": "invalid api key"}}}})
        if self.path == "/v1/models":
            hold("hold-models")
            return self.reply(200, {{"object": "list", "data": [{{"id": "fake-model", "object": "model"}}]}})
        if self.path == "/v1/models/status":
            hold("hold-list")
            if STATEFUL:
                return self.reply(200, {{"models": [{{"id": MODEL, "estimated_size": SIZE, "loaded": MODEL in LOADED, "pinned": False}}]}})
            return self.reply(200, {{"models": [{{"id": "fake-model", "estimated_size": 1000000000}}]}})
        if self.path == "/api/status":
            hold("hold-status")
            if STATUS_FAILS_FROM is not None and index >= STATUS_FAILS_FROM:
                return self.reply(500, {{"error": "status unavailable"}})
            if STATEFUL:
                return self.reply(200, {{"loaded_models": sorted(LOADED), "model_memory_used": SIZE * len(LOADED), "model_memory_max": 17179869184, "pressure": "ok"}})
            return self.reply(200, {{"loaded_models": [], "model_memory_used": 0, "model_memory_max": 17179869184, "pressure": "ok"}})
        return self.reply(404, {{"error": "not found"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            payload = json.loads(raw) if raw else {{}}
        except ValueError:
            payload = {{}}
        if ADMIN_API and self.path == "/admin/api/login":
            return self.login(payload)
        if not STATEFUL:
            return self.reply(404, {{"error": "not found"}})
        header = self.headers.get("Authorization")
        if header is None:
            auth = "none"
        elif KEY is not None and header == "Bearer " + KEY:
            auth = "ok"
        else:
            auth = "bad"
        with LOCK:
            with open(os.path.join(RECORDS, "requests.log"), "a") as f:
                f.write("POST:%s %s\n" % (self.path, auth))
        if KEY is not None and auth != "ok":
            return self.reply(401, {{"error": {{"message": "invalid api key"}}}})
        parts = self.path.strip("/").split("/")
        if len(parts) == 4 and parts[0] == "v1" and parts[1] == "models" and parts[3] in ("load", "unload"):
            if parts[2] != MODEL:
                return self.reply(404, {{"error": {{"message": "unknown model"}}}})
            if parts[3] == "load":
                hold("hold-load")
                LOADED.add(MODEL)
            else:
                LOADED.discard(MODEL)
            return self.reply(200, {{}})
        if self.path == "/v1/embeddings":
            hold("hold-embeddings")
            return self.reply(200, {{"object": "list", "model": "embed-model", "data": [{{"object": "embedding", "index": 0, "embedding": [0.0]}}]}})
        if self.path == "/v1/chat/completions" and isinstance(payload, dict) and payload.get("stream") is True:
            # A streamed answer: the headers and the first chunk go out at once, the rest is held until the test lets go.
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(b'data: {{"choices":[{{"delta":{{"content":"hel"}}}}]}}\n\n')
            self.wfile.flush()
            hold("hold-stream")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            self.close_connection = True
            return
        if self.path == "/v1/chat/completions":
            hold("hold-chat")
            if MODEL in LOADED:
                return self.reply(200, {{"model": MODEL, "choices": [{{"message": {{"content": "hello from the fake engine"}}}}]}})
            return self.reply(404, {{"error": {{"message": "model not loaded"}}}})
        return self.reply(404, {{"error": "not found"}})

if EXIT_AFTER > 0:
    threading.Timer(EXIT_AFTER, lambda: os._exit(3)).start()

if NEVER_LISTEN:
    while True:
        time.sleep(3600)

if LISTEN_DELAY > 0:
    time.sleep(LISTEN_DELAY)

host = arg("--host") or "127.0.0.1"
port = int(arg("--port"))
server = http.server.ThreadingHTTPServer((host, port), Handler)
server.daemon_threads = True

def release_port_on_request():
    # The test asked the engine to give the port back **without exiting**: the process lives on, the port is free.
    flag = os.path.join(RECORDS, "release-port")
    while not os.path.exists(flag):
        time.sleep(0.02)
    server.shutdown()
    server.server_close()

threading.Thread(target=release_port_on_request, daemon=True).start()
server.serve_forever()
# serve_forever returned: the port was released. The process stays alive (and not listening) until it is killed.
while True:
    time.sleep(3600)
"#,
        // A JSON string is a valid Python string literal.
        records = serde_json::to_string(&record_dir.display().to_string()).unwrap(),
        delay = behavior.listen_delay_secs,
        never = if behavior.never_listen {
            "True"
        } else {
            "False"
        },
        exit_after = behavior.exit_after_secs,
        exit_now = match behavior.exit_immediately {
            Some(code) => code.to_string(),
            None => "None".to_owned(),
        },
        trap = match behavior.trap_sigterm_secs {
            Some(seconds) => format!("{seconds:?}"),
            None => "None".to_owned(),
        },
        stateful = if behavior.stateful_models {
            "True"
        } else {
            "False"
        },
        status_fails_from = match behavior.api_status_fails_from_launch {
            Some(launch) => launch.to_string(),
            None => "None".to_owned(),
        },
        admin_api = if behavior.admin_api { "True" } else { "False" },
    )
}
