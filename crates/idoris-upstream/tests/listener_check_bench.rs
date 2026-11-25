//! Micro benchmark of the per-request listener re-check (IDORIS-CORE S6; requirement of the main session: the re-check adds p99 < 5 ms and
//! starts no child process on the request path). **Ignored by default**; run it with
//!
//! ```text
//! cargo test -p idoris-upstream --release --test listener_check_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! What is timed is the whole [`authorize_omlx_request`] call of an endpoint the engine manager governs (registry lookup, in-flight
//! registration, the guard's re-check against a **real** listening process, key resolution): the cost every request to the engine pays
//! before it is sent. The baseline is the same call for an endpoint governed *without* a guard, so the difference is the re-check.
//! For reference it also times the previous re-check (`lsof` via [`ProcessInspector::listens_on`]) and the fallback with the native probe
//! switched off (the short reuse window).
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use idoris_upstream::engine::{
    ExternalListenerGuard, ListenerIdentity, ListenerPolicy, ListenerVerifier, MANAGED_DIAL,
    OwnedListenerGuard, ProcessInspector, SystemInspector,
};
use idoris_upstream::omlx::key::{
    EndpointGuard, EndpointKey, authorize_omlx_request, forget_endpoint, govern_endpoint,
    govern_endpoint_guarded,
};

const LISTENER: &str = r#"
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.bind(("127.0.0.1", 0))
s.listen(16)
print("listening %d" % s.getsockname()[1], flush=True)
sys.stdin.read()
"#;

struct Engine {
    child: Child,
    port: u16,
}

impl Engine {
    fn start() -> Self {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-B", "-c", LISTENER])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        lines.read_line(&mut first).unwrap();
        let port = first
            .trim()
            .strip_prefix("listening ")
            .unwrap()
            .parse()
            .unwrap();
        // The pipe's read end stays open in the process; the child lives until it is killed.
        std::mem::forget(lines);
        Self { child, port }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Removes the endpoint's registration (and generation) when the benchmark ends.
struct Governed(String);

impl Drop for Governed {
    fn drop(&mut self) {
        forget_endpoint(&self.0);
    }
}

struct Stats {
    n: usize,
    p50: f64,
    p90: f64,
    p99: f64,
    p999: f64,
    max: f64,
}

/// Microseconds.
fn stats(mut nanos: Vec<u64>) -> Stats {
    nanos.sort_unstable();
    let at =
        |q: f64| nanos[((nanos.len() as f64 * q) as usize).min(nanos.len() - 1)] as f64 / 1_000.0;
    Stats {
        n: nanos.len(),
        p50: at(0.50),
        p90: at(0.90),
        p99: at(0.99),
        p999: at(0.999),
        max: *nanos.last().unwrap() as f64 / 1_000.0,
    }
}

fn row(label: &str, s: &Stats) {
    eprintln!(
        "| {label:<58} | {:>6} | {:>9.1} | {:>9.1} | {:>9.1} | {:>9.1} | {:>10.1} |",
        s.n, s.p50, s.p90, s.p99, s.p999, s.max
    );
}

async fn time_authorize(url: &str, n: usize) -> Stats {
    for _ in 0..200 {
        drop(authorize_omlx_request(url, None).await.unwrap());
    }
    let mut nanos = Vec::with_capacity(n);
    for _ in 0..n {
        let started = Instant::now();
        let authorization = authorize_omlx_request(url, None).await.unwrap();
        nanos.push(started.elapsed().as_nanos() as u64);
        drop(authorization);
    }
    stats(nanos)
}

/// One request every `gap` for `duration` (a steady trickle, so a reuse window is crossed many times), timing each.
async fn time_authorize_over(url: &str, duration: Duration, gap: Duration) -> Stats {
    let mut nanos = Vec::new();
    let end = Instant::now() + duration;
    while Instant::now() < end {
        let started = Instant::now();
        let authorization = authorize_omlx_request(url, None).await.unwrap();
        nanos.push(started.elapsed().as_nanos() as u64);
        drop(authorization);
        tokio::time::sleep(gap).await;
    }
    stats(nanos)
}

async fn time_lsof(inspector: &SystemInspector, pid: u32, port: u16, n: usize) -> Stats {
    let mut nanos = Vec::with_capacity(n);
    for _ in 0..n {
        let started = Instant::now();
        assert_eq!(inspector.listens_on(pid, port).await, Ok(true));
        nanos.push(started.elapsed().as_nanos() as u64);
    }
    stats(nanos)
}

/// Busy threads on every core until dropped (a loaded machine).
struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let threads = (0..cores)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 1_u64;
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..10_000 {
                            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        }
                        std::hint::black_box(x);
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[tokio::test]
#[ignore = "micro benchmark; see the module docs"]
async fn per_request_recheck_p50_p99_idle_and_under_load() {
    let engine = Engine::start();
    let pid = engine.child.id();
    let port = engine.port;

    // The endpoint with the native guard, the same endpoint's baseline (no guard) is a second port.
    let inspector = Arc::new(SystemInspector::new());
    let guard: Arc<dyn EndpointGuard> = Arc::new(OwnedListenerGuard::new(
        inspector.clone(),
        pid,
        std::net::SocketAddr::new(MANAGED_DIAL, port),
        inspector.start_time(pid).await.unwrap(),
        inspector.generation(pid).await.ok(),
    ));
    let url = format!("http://127.0.0.1:{port}");
    let _governed = Governed(url.clone());
    govern_endpoint_guarded(&url, EndpointKey::Private("bench-key".into()), Some(guard));

    let baseline_url = "http://127.0.0.1:1".to_owned();
    let _baseline = Governed(baseline_url.clone());
    govern_endpoint(&baseline_url, EndpointKey::Private("bench-key".into()));

    // The fallback: a second engine whose guard has the native probe switched off (what another unix, or a process the probe cannot read,
    // gets): the short reuse window of the `lsof` question.
    let fallback_engine = Engine::start();
    let fallback_url = format!("http://127.0.0.1:{}", fallback_engine.port);
    let _fallback_governed = Governed(fallback_url.clone());
    let fallback_guard: Arc<dyn EndpointGuard> = Arc::new(OwnedListenerGuard::new(
        Arc::new(SystemInspector::new().with_native_probe(false)),
        fallback_engine.child.id(),
        std::net::SocketAddr::new(MANAGED_DIAL, fallback_engine.port),
        inspector
            .start_time(fallback_engine.child.id())
            .await
            .unwrap(),
        inspector.generation(fallback_engine.child.id()).await.ok(),
    ));
    govern_endpoint_guarded(
        &fallback_url,
        EndpointKey::Private("bench-key".into()),
        Some(fallback_guard),
    );

    // An external engine (the key resolved from `$HOME/.omlx/settings.json`, so run this with `HOME` pointing at a scratch directory that has
    // one): the guard of the listener the verifier accepted, its executable identity re-read on every request.
    let external_engine = Engine::start();
    let external_pid = external_engine.child.id();
    let external_url = format!("http://127.0.0.1:{}", external_engine.port);
    let _external_governed = Governed(external_url.clone());
    let external_guard: Arc<dyn EndpointGuard> = Arc::new(ExternalListenerGuard::new(
        ListenerVerifier::new(inspector.clone(), ListenerPolicy::new("BENCH")),
        std::net::SocketAddr::new(MANAGED_DIAL, external_engine.port),
        vec![ListenerIdentity {
            pid: external_pid,
            started_at: inspector.start_time(external_pid).await.unwrap(),
            generation: inspector.listener_generation(external_pid).unwrap(),
            executable: inspector.executable_identity(external_pid).unwrap(),
        }],
    ));
    govern_endpoint_guarded(
        &external_url,
        EndpointKey::External {
            settings_key_verified: true,
        },
        Some(external_guard),
    );

    eprintln!();
    eprintln!(
        "per-request re-check, microseconds ({} build, {} cores)",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
    );
    eprintln!(
        "| {:<58} | {:>6} | {:>9} | {:>9} | {:>9} | {:>9} | {:>10} |",
        "scenario", "n", "p50", "p90", "p99", "p99.9", "max"
    );
    eprintln!(
        "|{:-<60}|{:-<8}|{:-<11}|{:-<11}|{:-<11}|{:-<11}|{:-<12}|",
        "", "", "", "", "", "", ""
    );

    let mut native_p99s = Vec::new();
    let mut external_p99s: Vec<(&str, f64)> = Vec::new();
    for (label, loaded) in [
        ("idle machine", false),
        ("under load (a busy thread per core)", true),
    ] {
        let _load = loaded.then(Load::start);
        let baseline = time_authorize(&baseline_url, 20_000).await;
        row(&format!("authorize, no guard [{label}]"), &baseline);
        let native = time_authorize(&url, 20_000).await;
        row(&format!("authorize, native re-check [{label}]"), &native);
        native_p99s.push((label, native.p99, baseline.p99));
        let external = time_authorize(&external_url, 20_000).await;
        row(
            &format!("authorize, EXTERNAL native re-check [{label}]"),
            &external,
        );
        external_p99s.push((label, external.p99));
        let lsof = time_lsof(&inspector, pid, port, if loaded { 20 } else { 30 }).await;
        row(
            &format!("previous re-check, `lsof` per request [{label}]"),
            &lsof,
        );
        let fallback = time_authorize_over(
            &fallback_url,
            Duration::from_secs(3),
            Duration::from_micros(500),
        )
        .await;
        row(
            &format!("fallback (no native), a request / 0.5 ms [{label}]"),
            &fallback,
        );
    }
    eprintln!();

    for (label, p99, baseline_p99) in native_p99s {
        assert!(
            p99 < 5_000.0,
            "p99 of the native re-check is {p99:.1} us ({label}; baseline {baseline_p99:.1} us), the requirement is below 5 ms"
        );
    }

    // The external endpoint resolves its key from `settings.json` on every request, answered from the stat-keyed cache while the file is
    // unchanged: the same 5 ms bound as the owned engine, idle and loaded.
    for (label, p99) in external_p99s {
        assert!(
            p99 < 5_000.0,
            "p99 of the external native re-check is {p99:.1} us ({label}), the requirement is below 5 ms"
        );
    }
}
