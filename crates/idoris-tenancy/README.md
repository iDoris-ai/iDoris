# idoris-tenancy

Ported from **`packages/tenancy`** (TS, kept as the reference implementation;
see the root `README.md`). `store`/`billing` are still R1 skeletons; `budget`
is implemented (R2-C) — see below for exactly what it does and does not
guarantee relative to the TS side.

## Scope (mirrors `packages/tenancy/src/*`)

| Rust module | TS source | Status | Responsibility |
|---|---|---|---|
| `store` | `src/store.ts` | skeleton | `TenantStore`, `TenantScopeError` — every read/write is tenant-scoped; missing tenant context is a hard error, never a fallback to the full table. |
| `budget` | `src/budget.ts` | **implemented (R2-C)** | SQLite-backed budget ledger: atomic two-phase `reserve`/`settle`/`release`, scoped to `(tenant, key, provider, model)`, bucketed per billing period. See below. |
| `billing` | `src/billing.ts` (T2.6.1) | skeleton | Monthly usage aggregation. Month boundaries (`period=YYYY-MM`) are resolved in the tenant's own `billing_timezone` (from `TenantContext`, see `idoris-contracts::tenant`) — never the server's local timezone, never a caller-supplied override. Records store UTC epoch millis; timezone conversion happens only at aggregation time. |

## `budget`: SQLite-backed budget ledger (R2-C)

This is a **new design informed by** `packages/tenancy/src/budget.ts`, not a
line-for-line port — the TS side's `checkBudget`/`charge`/`BudgetDecision`
operate on a `TenantContext` passed in by the caller and a single tenant-wide
budget; this crate owns its own SQLite-persisted state, scoped down to
`(tenant, key, provider, model)`, and splits "may this call spend" from
"charge it" into two atomic operations. Treat the TS module as prior art for
the *rules* (reject over-budget, never silently downgrade, price-unknown is
not free), not as a behavioral contract this crate must byte-for-byte match.

### API

- `BudgetScope::new(tenant_id, key_id, provider_id, model_id)` — the account
  a reservation/spend is tracked against.
- `BudgetLedger::open(path)` / `open_with(path, clock, ttl_ms)` — open
  (creating if needed) a ledger backed by a SQLite file. `ttl_ms` must be
  `> 0` (see below); `path` is trusted verbatim, never source it from
  tenant/request input.
- `configure(scope, limit_minor, billing_timezone)` — set/update a scope's
  period limit and IANA time zone. Rejects blank scope fields and
  unrecognized time zones.
- `balance(scope) -> i64` — read-only remaining balance for the current
  period; a consistent snapshot (one transaction), but can be stale the
  instant it's returned under concurrent writers. For observability/tests,
  not for deciding whether a spend can proceed.
- `reserve(scope, Price) -> Result<ReservationId, BudgetError>` — atomically
  checks balance and inserts a reservation inside one `BEGIN IMMEDIATE`
  transaction (`tests/budget_concurrency.rs` proves this holds across both
  threads and separate OS processes sharing the file). `Price` is
  `Known(i64)` or `Unknown`; `Unknown` always returns `PriceUnknown`, never
  treated as free.
- `settle(reservation_id, actual_cost_minor) -> Result<SettleReceipt, _>` and
  `release(reservation_id) -> Result<(), _>` are the two ways to finalize a
  reservation: `settle` for calls that completed (adds the real cost to
  settled spend, refunds any unused portion), `release` for calls that
  failed or fell back elsewhere (refunds the full reservation, no charge).
- Reservations expire **lazily**: each carries an `expires_at_ms`
  (`DEFAULT_RESERVATION_TTL_MS` by default, overridable via `open_with`),
  and `balance`/`reserve` simply exclude expired rows from their sums — there
  is no background thread sweeping the table. `sweep_expired()` is an
  explicit, optional call that flips expired rows' status (for table hygiene
  and so tests can observe the transition); skipping it never causes
  over-spending.
- Exceeding the balance returns `BudgetError::Exceeded { balance_minor,
  estimated_cost_minor, topup_hint }`. `to_402_body()` turns that into a
  `Budget402Body` carrying those three fields plus `reason_code:
  "budget_exceeded"` — **this is not** contract-tenancy §4's full HTTP
  envelope (`{"error": {"type", "message", "tenant_id", "limit_minor",
  "spent_minor"}}`); this crate only sees a `BudgetScope`, not the wider
  `TenantContext`, so a router integration is expected to fold these four
  fields into its own response, not serialize `Budget402Body` as the literal
  HTTP body.
- `estimate_tokens(text, model_family)` (and the `TokenEstimator` trait for
  swapping in a real tokenizer later): a conservative, CJK-aware estimate for
  sizing a `reserve` call before real usage is known. Every CJK codepoint
  counts as >= 1 token (never the flawed `chars/4` heuristic, which
  under-counts CJK text); everything else is priced at `bytes/3`; the total
  is multiplied by a 1.2 safety factor. `model_family` is accepted but
  currently ignored — one conservative estimate for every family.
- `billing_period_key(now_ms, billing_timezone)`: the `YYYY-MM` period a UTC
  instant falls into under an explicit IANA time zone, using `chrono-tz`'s
  bundled tzdata. This tracks the TS side's `Intl.DateTimeFormat`-based
  rules **only as of the tzdata snapshot `chrono-tz` ships with** (currently
  IANA 2025b, pinned via the root `Cargo.toml`) — it is not a live-updating
  tzdb, so a rule change IANA publishes after that snapshot won't be
  reflected until `chrono-tz` is bumped. Do not assume permanent, guaranteed
  parity with whatever tzdata the TS runtime's host happens to have.

### Known limitations

- `rusqlite`'s bundled SQLite (3.53.2 as of this writing) has a since-fixed
  (3.53.3+) issue where a crafted hot-journal can make SQLite delete an
  arbitrary file the process can write — no `libsqlite3-sys` release
  bundling the fix exists on crates.io yet. See the root `Cargo.toml`'s
  `rusqlite` dependency comment for the full risk note and the deployment
  constraint (data directory permissions) it depends on, which this repo
  does not yet enforce.
- No automated dependency-freshness gate exists (CI only runs `cargo deny
  check licenses bans`); bumping `rusqlite`/`libsqlite3-sys` when a fix
  ships currently depends on a human noticing.

### Tests

`src/budget/*.rs` (`#[cfg(test)]`, unit-level) plus `tests/budget_ttl.rs`,
`tests/budget_period_boundary.rs` and `tests/budget_concurrency.rs`
(integration-level, including the multi-process concurrency test via the
`budget_mp_worker` helper binary under `src/bin/`). All tests clean up their
temporary SQLite files (and `-wal`/`-shm` sidecars) via an RAII guard, so a
panicking test doesn't leak files into the OS temp directory.

## Explicitly not done here

`store`/`billing` stay unimplemented skeletons; this crate is not wired into
`crates/idoris-router` yet. Depends on `idoris-contracts` for
`TenantContext` and its `is_iana_time_zone` allow-list.
