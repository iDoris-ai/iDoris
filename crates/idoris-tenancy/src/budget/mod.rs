//! SQLite-backed budget ledger (R2-C). See the crate-level doc comment in
//! `lib.rs` and this directory's `README.md` for how this relates to
//! `packages/tenancy/src/budget.ts` and contract-tenancy §4.
//!
//! This landed incrementally: foundation types first (this change), the
//! SQLite-backed `BudgetLedger` itself in follow-up changes on top.

mod clock;
mod error;
mod estimator;
mod ledger;
mod period;
mod scope;

pub use clock::{Clock, SystemClock};
pub use error::{BUDGET_EXCEEDED_REASON_CODE, Budget402Body, BudgetError};
pub use estimator::{ConservativeTokenEstimator, TokenEstimator, estimate_tokens};
pub use ledger::{
    BudgetLedger, DEFAULT_RESERVATION_TTL_MS, Price, ReservationId, SettleReceipt, SpendGate,
};
pub use period::billing_period_key;
pub use scope::BudgetScope;
