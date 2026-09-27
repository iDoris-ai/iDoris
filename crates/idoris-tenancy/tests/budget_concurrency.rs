//! Concurrent `reserve` must never over-spend a scope's budget — the exact
//! bug class the blog write-up on LiteLLM #32614 describes (a non-atomic
//! check-then-deduct lets N concurrent callers all pass the balance check
//! before any commits its deduction). Two variants:
//! - many threads, each with its **own** SQLite connection to the same
//!   file (so this stresses SQLite's own locking, not just a Rust mutex),
//!   synchronized to start with a `Barrier` so the race is real;
//! - many separate **OS processes** sharing the same file, via the
//!   `budget_mp_worker` binary, synchronized with a "go file" (spawn all
//!   first, blocked; create the file; they race) — proves atomicity across
//!   connections that don't even share an address space.
//!
//! `LIMIT_MINOR / PER_RESERVE_MINOR` is chosen so the outcome is exactly
//! deterministic regardless of scheduling: with a 1000 limit and 100 per
//! reserve, **exactly 10** of the 16 concurrent attempts can ever succeed
//! (10 * 100 = 1000, an 11th would need 1100) — always exactly 10/6, never
//! just "some succeed, some don't".

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Price};

const THREADS: i64 = 16;
const PER_RESERVE_MINOR: i64 = 100;
const LIMIT_MINOR: i64 = 1_000;
const EXPECTED_SUCCESSES: i64 = LIMIT_MINOR / PER_RESERVE_MINOR; // 10
const EXPECTED_REJECTIONS: i64 = THREADS - EXPECTED_SUCCESSES; // 6

/// Temp SQLite path that deletes the database and its `-wal`/`-shm`
/// sidecars on drop — including when the test panics.
struct TempDb(std::path::PathBuf);

impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

fn temp_db_path(tag: &str) -> TempDb {
    TempDb(std::env::temp_dir().join(format!(
        "idoris-tenancy-budget-concurrency-{tag}-{}-{}.sqlite3",
        std::process::id(),
        uuid::Uuid::new_v4()
    )))
}

#[test]
fn concurrent_reserve_never_overspends_single_process() {
    let path = temp_db_path("threads");
    let scope = BudgetScope::new("acme-co", "key-threads", "openai", "gpt-5");
    {
        let ledger = BudgetLedger::open(&path).expect("open");
        ledger
            .configure(&scope, LIMIT_MINOR, "Asia/Bangkok")
            .expect("configure");
    }

    // All threads block on this barrier immediately before calling
    // `reserve`, so they arrive at the atomic check-and-deduct together
    // instead of being staggered by however long thread spawn/setup took.
    let barrier = Arc::new(Barrier::new(THREADS as usize));

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let path = path.0.clone();
            let scope = scope.clone();
            let barrier = Arc::clone(&barrier);
            // Each thread opens its own connection to the shared file, so
            // the reserve/settle atomicity guarantee under test comes from
            // SQLite's own locking (`BEGIN IMMEDIATE` + `busy_timeout`),
            // not from an in-process Rust mutex trivially serializing
            // everything.
            thread::spawn(move || {
                let ledger = BudgetLedger::open(&path).expect("open in thread");
                barrier.wait();
                ledger.reserve(&scope, Price::Known(PER_RESERVE_MINOR))
            })
        })
        .collect();

    let mut succeeded: i64 = 0;
    let mut rejected: i64 = 0;
    for h in handles {
        match h.join().expect("worker thread panicked") {
            Ok(_) => succeeded += 1,
            Err(BudgetError::Exceeded { .. }) => rejected += 1,
            // Strict: anything other than a clean success or a structured
            // budget rejection is a bug (e.g. a spurious SQLITE_BUSY that
            // should have been absorbed by `busy_timeout`), not a result
            // this test should silently lump in with "rejected".
            Err(other) => panic!("unexpected reserve error: {other}"),
        }
    }

    assert_eq!(
        succeeded, EXPECTED_SUCCESSES,
        "expected exactly {EXPECTED_SUCCESSES} successful reservations"
    );
    assert_eq!(
        rejected, EXPECTED_REJECTIONS,
        "expected exactly {EXPECTED_REJECTIONS} budget-exceeded rejections"
    );
}

#[test]
fn concurrent_reserve_never_overspends_multi_process() {
    let path = temp_db_path("processes");
    let scope = BudgetScope::new("acme-co", "key-mp", "openai", "gpt-5");
    {
        let ledger = BudgetLedger::open(&path).expect("open");
        ledger
            .configure(&scope, LIMIT_MINOR, "Asia/Bangkok")
            .expect("configure");
    }

    let go_file = temp_db_path("go-file");
    let worker = env!("CARGO_BIN_EXE_budget_mp_worker");

    // Spawn every worker first — each blocks inside the binary until
    // `go_file` exists — then create the file, releasing them all together.
    let mut children: Vec<_> = (0..THREADS)
        .map(|_| {
            let args = [
                path.0.to_str().expect("utf8 path").to_string(),
                scope.tenant_id.clone(),
                scope.key_id.clone(),
                scope.provider_id.clone(),
                scope.model_id.clone(),
                PER_RESERVE_MINOR.to_string(),
                go_file.0.to_str().expect("utf8 go path").to_string(),
            ];
            Command::new(worker)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn worker")
        })
        .collect();

    std::fs::write(&go_file, b"go").expect("write go file");

    // A panic partway through the loop below (timeout, non-zero exit,
    // unexpected stdout) must not leave the not-yet-processed children
    // running as orphans — this guard kills them on unwind only (`Drop`
    // still runs during unwind; `panicking()` skips it on the happy path).
    struct KillRemainingOnDrop<'a>(&'a mut Vec<std::process::Child>);
    impl Drop for KillRemainingOnDrop<'_> {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                return;
            }
            for child in self.0.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut succeeded: i64 = 0;
    let mut rejected: i64 = 0;
    let per_child_deadline = Duration::from_secs(15);
    while let Some(mut child) = children.pop() {
        let _guard = KillRemainingOnDrop(&mut children);
        let start = Instant::now();
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => break status,
                None if start.elapsed() >= per_child_deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("worker process timed out after {per_child_deadline:?} and was killed");
                }
                None => thread::sleep(Duration::from_millis(5)),
            }
        };

        let mut stdout = String::new();
        if let Some(mut out) = child.stdout.take() {
            use std::io::Read as _;
            let _ = out.read_to_string(&mut stdout);
        }
        let mut stderr = String::new();
        if let Some(mut err) = child.stderr.take() {
            use std::io::Read as _;
            let _ = err.read_to_string(&mut stderr);
        }

        assert!(
            status.success(),
            "worker exited non-zero ({status}): {stderr}"
        );
        match stdout.trim() {
            s if s.starts_with("RESERVED") => succeeded += 1,
            "EXCEEDED" => rejected += 1,
            other => panic!("unexpected worker stdout: {other:?} (stderr: {stderr})"),
        }
    }

    assert_eq!(
        succeeded, EXPECTED_SUCCESSES,
        "expected exactly {EXPECTED_SUCCESSES} successful reservations across processes"
    );
    assert_eq!(
        rejected, EXPECTED_REJECTIONS,
        "expected exactly {EXPECTED_REJECTIONS} cross-process budget-exceeded rejections"
    );
}
