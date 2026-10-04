pub mod diagnostics;
pub mod forced;
pub mod quant;
pub mod resident;
pub mod temp;
pub mod types;

pub use diagnostics::{recommend, recommend_from_file};
pub use forced::apply_core_override;
pub use quant::{lowest_footprint_pick, pick_quant, quant_by_label};
pub use resident::{blocked_choices, budget_breakdown, choose_resident};
pub use temp::temp_admission;
pub use types::{
    BlockedChoice, BudgetBreakdown, CoreOverride, ForcedResult, HEADROOM_GB, PartialPolicy,
    QuantPick, RecommendError, Recommendation, RecommenderPolicy, ResidentChoice, ResidentError,
    SysctlRecommendation, TEMP_SLOT_RESERVE_GB, TempChoice, TempResult, TempStatus,
};
