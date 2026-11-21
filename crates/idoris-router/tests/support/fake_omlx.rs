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
use std::process::{Child, Command};
use std::time::Duration;

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

/// 起一个普通子进程，对 `ETXTBSY` 重试（刚写好的脚本，Linux 上偶发）。
pub fn spawn_retrying(command: &mut Command) -> std::io::Result<Child> {
    let mut attempt = 0;
    loop {
        match command.spawn() {
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
        r#"import http.server, json, os, sys, threading, time

RECORDS = {records}
LISTEN_DELAY = {delay}
NEVER_LISTEN = {never}
EXIT_AFTER = {exit_after}
EXIT_IMMEDIATELY = {exit_now}

def arg(name):
    args = sys.argv[1:]
    return args[args.index(name) + 1] if name in args else None

# One record per launch: what the engine was started with. Numbered by the files already there.
os.makedirs(RECORDS, exist_ok=True)
index = len([n for n in os.listdir(RECORDS) if n.startswith("launch-")])
with open(os.path.join(RECORDS, "launch-%04d.json" % index), "w") as f:
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

if EXIT_IMMEDIATELY is not None:
    sys.exit(EXIT_IMMEDIATELY)

KEY = os.environ.get("OMLX_API_KEY")
LOCK = threading.Lock()

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

    def do_GET(self):
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
        if KEY is not None and auth != "ok":
            return self.reply(401, {{"error": {{"message": "invalid api key"}}}})
        if self.path == "/v1/models":
            return self.reply(200, {{"object": "list", "data": [{{"id": "fake-model", "object": "model"}}]}})
        if self.path == "/v1/models/status":
            return self.reply(200, {{"models": [{{"id": "fake-model", "estimated_size": 1000000000}}]}})
        if self.path == "/api/status":
            return self.reply(200, {{"loaded_models": [], "model_memory_used": 0, "model_memory_max": 17179869184, "pressure": "ok"}})
        return self.reply(404, {{"error": "not found"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(length)
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
server.serve_forever()
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
    )
}
