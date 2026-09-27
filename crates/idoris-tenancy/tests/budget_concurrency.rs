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
//!
//! The multi-process variant needs the `budget_mp_worker` binary, which is
//! gated behind the `test-bins` feature (Opus Tier-2 acceptance L6 — it has
//! no reason to ship in a default build/publish of this crate). This makes
//! it *opt-in at the Cargo-feature level*, not *skippable as a CI gate*: the
//! `rust` job in `.github/workflows/ci.yml` always passes
//! `--features idoris-tenancy/test-bins`, so it still runs on every push/PR
//! (M4) — a contributor running plain `cargo test` locally without that flag
//! just doesn't build the worker binary or this one test (running it
//! without the feature fails at runtime with "No such file or directory"
//! rather than being skipped at compile time, which is why the test itself
//! — not just the `[[bin]]` target — must be `#[cfg]`-gated).

#![cfg_attr(not(feature = "test-bins"), allow(dead_code, unused_imports))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

#[cfg(feature = "test-bins")]
use std::process::{Command, Stdio};
#[cfg(feature = "test-bins")]
use std::time::Instant;

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

#[cfg(feature = "test-bins")]
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

/// M4: concurrent `settle` and `release` racing on the *same* reservation id
/// must never both succeed — SQLite's own locking (not an in-process mutex:
/// two independently-opened ledgers race here) serializes the two
/// transactions, and whichever runs second sees the reservation already
/// finalized by the first and is rejected with `ReservationNotActive`.
#[test]
fn concurrent_settle_and_release_on_same_reservation_exactly_one_wins() {
    let path = temp_db_path("settle-release-race");
    let scope = BudgetScope::new("acme-co", "key-race", "openai", "gpt-5");
    let id = {
        let ledger = BudgetLedger::open(&path).expect("open");
        ledger.configure(&scope, 1_000, "UTC").expect("configure");
        ledger.reserve(&scope, Price::Known(100)).expect("reserve")
    };

    let barrier = Arc::new(Barrier::new(2));

    let settle_handle = {
        let path = path.0.clone();
        let tenant_id = scope.tenant_id.clone();
        let id = id.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            let ledger = BudgetLedger::open(&path).expect("open in settle thread");
            barrier.wait();
            ledger.settle(&tenant_id, &id, 100)
        })
    };
    let release_handle = {
        let path = path.0.clone();
        let tenant_id = scope.tenant_id.clone();
        let id = id.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            let ledger = BudgetLedger::open(&path).expect("open in release thread");
            barrier.wait();
            ledger.release(&tenant_id, &id)
        })
    };

    let settle_result = settle_handle.join().expect("settle thread panicked");
    let release_result = release_handle.join().expect("release thread panicked");

    let settle_won = settle_result.is_ok();
    let release_won = release_result.is_ok();
    assert!(
        settle_won ^ release_won,
        "exactly one of settle/release must win: settle={settle_result:?}, release={release_result:?}"
    );
    if !settle_won {
        assert!(matches!(
            settle_result,
            Err(BudgetError::ReservationNotActive { .. })
        ));
    }
    if !release_won {
        assert!(matches!(
            release_result,
            Err(BudgetError::ReservationNotActive { .. })
        ));
    }
}

/// L4: SQLite reporting `SQLITE_BUSY` even after the configured
/// `busy_timeout` elapses must surface as `BudgetError::Busy`, not a generic
/// `Storage` error, so a caller can tell "contended, maybe retry" apart from
/// "something is actually broken". A raw connection holds an exclusive write
/// transaction open for the whole test, so the ledger's own `BEGIN
/// IMMEDIATE` inside `configure` is guaranteed to time out waiting for it —
/// a short (50ms) `busy_timeout` keeps the test fast.
#[test]
fn busy_timeout_surfaces_as_busy_not_a_generic_storage_error() {
    let path = temp_db_path("busy-timeout");
    // Open (running migrations) *before* the blocker below takes its lock,
    // so this doesn't also need to win a race against the blocker itself.
    let ledger = BudgetLedger::open_with_busy_timeout(
        &path,
        Arc::new(idoris_tenancy::budget::SystemClock),
        idoris_tenancy::budget::DEFAULT_RESERVATION_TTL_MS,
        Duration::from_millis(50),
    )
    .expect("open with short busy_timeout");

    let blocker = rusqlite::Connection::open(&path).expect("open raw blocker connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold a write transaction open");

    let scope = BudgetScope::new("acme-co", "key-busy", "openai", "gpt-5");
    let result = ledger.configure(&scope, 100, "UTC");

    // Release the blocker regardless of the assertion outcome below, so a
    // failing assertion doesn't leave a stray lock behind.
    let _ = blocker.execute_batch("ROLLBACK;");
    drop(blocker);

    assert!(
        matches!(result, Err(BudgetError::Busy)),
        "expected Busy, got {result:?}"
    );
}

/// B-8: the same deterministic busy-timeout proof as above, but against
/// `reserve` specifically (the method the split-transaction concern is
/// actually about) rather than `configure`.
#[test]
fn reserve_returns_busy_under_short_busy_timeout_when_lock_is_held() {
    let path = temp_db_path("busy-timeout-reserve");
    let scope = BudgetScope::new("acme-co", "key-busy-reserve", "openai", "gpt-5");
    let ledger = BudgetLedger::open_with_busy_timeout(
        &path,
        Arc::new(idoris_tenancy::budget::SystemClock),
        idoris_tenancy::budget::DEFAULT_RESERVATION_TTL_MS,
        Duration::from_millis(50),
    )
    .expect("open with short busy_timeout");
    ledger.configure(&scope, 1_000, "UTC").expect("configure");

    let blocker = rusqlite::Connection::open(&path).expect("open raw blocker connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold a write transaction open");

    let result = ledger.reserve(&scope, Price::Known(100));

    let _ = blocker.execute_batch("ROLLBACK;");
    drop(blocker);

    assert!(
        matches!(result, Err(BudgetError::Busy)),
        "expected Busy, got {result:?}"
    );
}

/// B-8: the tenant-level limit must be enforced atomically across *both*
/// multiple sub-scopes *and* multiple OS processes at once — not just one
/// sub-scope (the existing multi-process test above) or one process
/// (`tenant_level_limit_aggregates_across_sub_scopes` in `ledger.rs`'s own
/// unit tests). `SUB_SCOPES` different `key_id`s, each individually
/// configured with a limit far above what the shared tenant limit actually
/// allows, so only the tenant-level check is ever the binding constraint.
#[cfg(feature = "test-bins")]
#[test]
fn tenant_limit_is_enforced_across_multiple_sub_scopes_and_processes() {
    const SUB_SCOPES: i64 = 4;

    let path = temp_db_path("tenant-multi-subscope-processes");
    let scopes: Vec<BudgetScope> = (0..SUB_SCOPES)
        .map(|i| BudgetScope::new("acme-co", format!("key-{i}"), "openai", "gpt-5"))
        .collect();
    {
        let ledger = BudgetLedger::open(&path).expect("open");
        for scope in &scopes {
            // Deliberately far above the tenant limit below, so a sub-scope
            // never independently blocks a reserve on its own.
            ledger
                .configure(scope, 10_000, "UTC")
                .expect("configure sub-scope");
        }
        ledger
            .configure_tenant(
                "acme-co",
                LIMIT_MINOR,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .expect("configure_tenant");
    }

    let go_file = temp_db_path("tenant-multi-subscope-go");
    let worker = env!("CARGO_BIN_EXE_budget_mp_worker");

    let mut children: Vec<_> = (0..THREADS)
        .map(|i| {
            let scope = &scopes[(i % SUB_SCOPES) as usize];
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
        "expected exactly {EXPECTED_SUCCESSES} successes shared across {SUB_SCOPES} sub-scopes"
    );
    assert_eq!(
        rejected, EXPECTED_REJECTIONS,
        "expected exactly {EXPECTED_REJECTIONS} tenant-level rejections"
    );
}
