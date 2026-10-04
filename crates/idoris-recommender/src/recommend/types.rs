use idoris_contracts::common::Capability;

use crate::memory::{KvQuant, WiredMode};

#[derive(Debug, Clone, PartialEq)]
pub struct RecommenderPolicy {
    pub wired_mode: WiredMode,
    pub context_target: u64,
    pub kv_quant: KvQuant,
    pub temp_slots: u32,
    pub quality_threshold: f64,
    pub needed_capabilities: Vec<Capability>,
}

impl Default for RecommenderPolicy {
    fn default() -> Self {
        Self {
            wired_mode: WiredMode::Conservative,
            context_target: 32_768,
            kv_quant: KvQuant::Q8,
            temp_slots: 1,
            quality_threshold: 0.98,
            needed_capabilities: vec![Capability::Vision, Capability::Coding],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartialPolicy {
    pub wired_mode: Option<WiredMode>,
    pub context_target: Option<u64>,
    pub kv_quant: Option<KvQuant>,
    pub temp_slots: Option<u32>,
    pub quality_threshold: Option<f64>,
    pub needed_capabilities: Option<Vec<Capability>>,
}

impl RecommenderPolicy {
    pub fn merged(partial: Option<&PartialPolicy>) -> Self {
        let defaults = Self::default();
        let Some(partial) = partial else {
            return defaults;
        };
        Self {
            wired_mode: partial.wired_mode.unwrap_or(defaults.wired_mode),
            context_target: partial.context_target.unwrap_or(defaults.context_target),
            kv_quant: partial.kv_quant.unwrap_or(defaults.kv_quant),
            temp_slots: partial.temp_slots.unwrap_or(defaults.temp_slots),
            quality_threshold: partial
                .quality_threshold
                .unwrap_or(defaults.quality_threshold),
            needed_capabilities: partial
                .needed_capabilities
                .clone()
                .unwrap_or(defaults.needed_capabilities),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuantPick {
    pub label: String,
    pub quality: f64,
    pub weights_gb: f64,
    pub kv_gb: f64,
    pub footprint_gb: f64,
}

pub const TEMP_SLOT_RESERVE_GB: f64 = 3.5;
pub const HEADROOM_GB: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetBreakdown {
    pub reserve_gb: f64,
    pub usable_gb: f64,
    pub temp_reserve_gb: f64,
    pub resident_budget_gb: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResidentChoice {
    pub id: String,
    pub ctx: u64,
    pub score: f64,
    pub label: String,
    pub quality: f64,
    pub weights_gb: f64,
    pub kv_gb: f64,
    pub footprint_gb: f64,
}

impl ResidentChoice {
    pub fn from_pick(id: String, ctx: u64, score: f64, pick: QuantPick) -> Self {
        Self {
            id,
            ctx,
            score,
            label: pick.label,
            quality: pick.quality,
            weights_gb: pick.weights_gb,
            kv_gb: pick.kv_gb,
            footprint_gb: pick.footprint_gb,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockedChoice {
    pub id: String,
    pub min_ram_gb: f64,
    pub estimated_memory_gb: f64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidentError {
    EmptyQuantOptions { id: String },
}

impl std::fmt::Display for ResidentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyQuantOptions { id } => {
                write!(f, "catalog model {id:?} has no quant options")
            }
        }
    }
}

impl std::error::Error for ResidentError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreOverride {
    pub id: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForcedResult {
    pub resident: Option<ResidentChoice>,
    pub override_choice: Option<CoreOverride>,
    pub warnings: Vec<String>,
}
