//! 本机 chat 模型推荐（接口规范 §2.9，wire 1.2.0）。纯函数：不读文件系统、不读环境变量、不联网。
//!
//! 档位表、回落顺序与 `reason` 代码逐字取自 §2.9；「放得下」用 [`crate::memory`] 与
//! [`crate::recommend::budget_breakdown`] 的现有公式，不另写一套。
//!
//! 实现口径（规范没写到的地方，取保守一侧）：
//! - 内存估算 = `footprint_gb(权重 + 8k 上下文 fp16 KV 上界 + 1 GB 开销)`，与规范里 27B「约 23.8 GB」的算法一致。
//!   权重体积：8B 4.62 GB、27B 20.68 GB、35B-A3B 24.694 GB，三个值都是 2026-11-20 在本机实测的 oMLX 目录大小
//!   （8B 4_623_791_053、27B 20_681_935_545、35B-A3B 24_693_954_499 字节），取到兆字节精度；35B-A3B 向上取整
//!   （偏保守）。目录估算（35B 曾为 22.13 GB）少算约 2.5 GB，故改用实测值，未安装时 `size_bytes` 也是预计下载体积。
//! - `fits` = 估算内存不超过预算，且真实物理 RAM 不低于目录里的 `min_ram_gb`（§2.4 的 min_ram 门）。
//!   预算：设置了额度时，额度要**先扣 1 GB 余量**（[`HEADROOM_BYTES`]，与加载准入同一个常量：估算落在
//!   (额度 − 1 GB, 额度] 的模型加载时会 503 `headroom_confirmation_required`，不能报 `fits: true`）；
//!   没有额度时按调用方给的可用 GPU 内存（[`probed_gpu_available_bytes`]，加载准入对它没有余量要求）。
//! - `alternatives[].reason` 取该档位行自己的代码。
//! - 第一行（`[0, 16 GiB)`）放不下时 `model_id` 为 `null`，`reason` 仍是该行的 `tier_below_16gib`；其余行
//!   全部放不下才是 `insufficient_memory`。

use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, OmlxBinding};
use crate::local_models::LocalModel;
use crate::memory::{KvQuant, ModelArch, QuantSpec, footprint_gb};
use crate::probe::{FactsSource, HostFacts};
use crate::recommend::{RecommenderPolicy, budget_breakdown};

pub const GIB: u64 = 1_073_741_824;

/// 设置了额度时，加载准入在额度之内保留的余量（十进制 1 GB）：估算内存超过「额度 − 余量」的加载需要调用方用
/// `X-iDoris-Headroom-Breach: confirmed` 确认。router 的加载准入与这里的 `fits` 共用这一个常量。
pub const HEADROOM_BYTES: u64 = 1_000_000_000;
const BYTES_PER_GB: f64 = 1_000_000_000.0;

/// 估算内存用的上下文长度与 KV 精度（规范的实测口径：8k 上下文、全注意力 fp16 KV 上界）。
pub const ESTIMATE_CTX: u64 = 8192;
pub const ESTIMATE_KV_QUANT: KvQuant = KvQuant::Fp16;

pub const MODEL_8B: &str = "Qwen3-8B-4bit";
pub const MODEL_27B: &str = "Qwen3.8-27B-OptiQ-4bit";
pub const MODEL_35B: &str = "Qwen3.6-35B-A3B-OptiQ-4bit";

/// 回落顺序（§2.9）：从档位推荐的下一个起找第一个放得下的。
pub const FALLBACK_ORDER: [&str; 3] = [MODEL_35B, MODEL_27B, MODEL_8B];

pub const REASON_TIER_BELOW_16: &str = "tier_below_16gib";
pub const REASON_TIER_16_48: &str = "tier_16_48gib";
pub const REASON_TIER_48_64: &str = "tier_48_64gib";
pub const REASON_TIER_64_PLUS: &str = "tier_64gib_plus";
pub const REASON_FALLBACK: &str = "fallback_does_not_fit";
pub const REASON_INSUFFICIENT: &str = "insufficient_memory";

struct Tier {
    /// 左闭右开，单位 GiB。
    from_gib: u64,
    to_gib: Option<u64>,
    primary: &'static str,
    alternatives: &'static [&'static str],
    reason: &'static str,
}

const TIERS: [Tier; 4] = [
    Tier {
        from_gib: 0,
        to_gib: Some(16),
        primary: MODEL_8B,
        alternatives: &[],
        reason: REASON_TIER_BELOW_16,
    },
    Tier {
        from_gib: 16,
        to_gib: Some(48),
        primary: MODEL_8B,
        alternatives: &[],
        reason: REASON_TIER_16_48,
    },
    Tier {
        from_gib: 48,
        to_gib: Some(64),
        primary: MODEL_27B,
        alternatives: &[MODEL_8B],
        reason: REASON_TIER_48_64,
    },
    Tier {
        from_gib: 64,
        to_gib: None,
        primary: MODEL_27B,
        alternatives: &[MODEL_35B, MODEL_8B],
        reason: REASON_TIER_64_PLUS,
    },
];

/// 已知体积（十进制字节）；`None` 表示没有实测值，按目录最小量化估算。
fn known_size_bytes(omlx_id: &str) -> Option<u64> {
    match omlx_id {
        MODEL_8B => Some(4_620_000_000),
        MODEL_27B => Some(20_680_000_000),
        MODEL_35B => Some(24_694_000_000),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecommendationHost {
    pub ram_bytes: u64,
    pub allocated_budget_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecommendationAlternative {
    pub model_id: String,
    pub installed: bool,
    pub size_bytes: Option<u64>,
    pub est_memory_bytes: Option<u64>,
    pub fits: bool,
    pub reason: String,
}

/// `GET /v1/recommendation?role=chat` 与 `idoris recommend --role chat --json` 共用的响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatRecommendation {
    pub role: String,
    pub model_id: Option<String>,
    pub installed: bool,
    pub size_bytes: Option<u64>,
    pub est_memory_bytes: Option<u64>,
    pub fits: bool,
    pub reason: String,
    pub host: RecommendationHost,
    pub alternatives: Vec<RecommendationAlternative>,
}

#[derive(Debug, Clone, Copy)]
pub struct RecommendInput<'a> {
    /// 物理内存字节数（档位按它判断）。
    pub ram_bytes: u64,
    /// `IDORIS_ALLOCATED_BUDGET_BYTES` / `--allocated-budget-bytes`；设置了就按它判断「放得下」。
    pub allocated_budget_bytes: Option<u64>,
    /// iDoris 自探的可用 GPU 内存；没有额度时按它判断「放得下」。见 [`probed_gpu_available_bytes`]。
    pub gpu_available_bytes: u64,
    /// 已安装（完整）的本地模型，来自 [`crate::local_models::scan_local_models`]。
    pub installed: &'a [LocalModel],
    pub catalog: &'a Catalog,
    pub bindings: &'a [OmlxBinding],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecommendError {
    /// 目录里缺少档位表需要的 `omlx_id` 条目（目录文件被改坏或太旧）。
    CatalogMissing(String),
}

impl std::fmt::Display for RecommendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CatalogMissing(id) => {
                write!(f, "模型目录（catalog.yaml）里没有 omlx_id 为 {id} 的条目")
            }
        }
    }
}

impl std::error::Error for RecommendError {}

/// 没有额度时的「可用 GPU 内存」：与 `/capabilities` 同一公式（`apple_usable_gb`，保守档）。
pub fn probed_gpu_available_bytes(ram_bytes: u64) -> u64 {
    let host = host_facts(ram_bytes);
    let budget = budget_breakdown(&host, &RecommenderPolicy::default(), None);
    gb_to_bytes(budget.usable_gb)
}

fn host_facts(ram_bytes: u64) -> HostFacts {
    HostFacts {
        ram_gb: ram_bytes as f64 / GIB as f64,
        chip: crate::probe::UNKNOWN_CHIP.to_owned(),
        gpu_cores: None,
        os: "unknown".to_owned(),
        source: FactsSource::Probe,
    }
}

fn gb_to_bytes(gb: f64) -> u64 {
    (gb * BYTES_PER_GB).max(0.0).round() as u64
}

struct Assessed {
    est_memory_bytes: u64,
    fits: bool,
    size_bytes: Option<u64>,
    installed: bool,
}

fn assess(omlx_id: &str, input: &RecommendInput<'_>) -> Result<Assessed, RecommendError> {
    let missing = || RecommendError::CatalogMissing(omlx_id.to_owned());
    let binding = input
        .bindings
        .iter()
        .find(|binding| binding.omlx_id == omlx_id)
        .ok_or_else(missing)?;
    let model = input
        .catalog
        .catalog
        .iter()
        .find(|model| model.id == binding.catalog_id)
        .ok_or_else(missing)?;
    let weights_gb = match known_size_bytes(omlx_id) {
        Some(bytes) => bytes as f64 / BYTES_PER_GB,
        None => model
            .quant_options
            .iter()
            .filter_map(|quant| {
                crate::memory::weights_gb(model.params_total_b, &quant.as_quant_spec()).ok()
            })
            .fold(f64::INFINITY, f64::min),
    };
    if !weights_gb.is_finite() {
        return Err(missing());
    }
    let arch: ModelArch = model.arch;
    let spec = QuantSpec {
        label: None,
        bpp: None,
        weights_gb: Some(weights_gb),
        quality: 1.0,
    };
    let footprint = footprint_gb(
        model.params_total_b,
        &spec,
        arch,
        ESTIMATE_CTX,
        ESTIMATE_KV_QUANT,
        None,
    )
    .map_err(|_| missing())?;
    let est_memory_bytes = gb_to_bytes(footprint);
    // 额度要先扣余量（加载准入就是这么算的）；自探的 GPU 可用内存没有余量要求。
    let budget_bytes = match input.allocated_budget_bytes {
        Some(allocated) => allocated.saturating_sub(HEADROOM_BYTES),
        None => input.gpu_available_bytes,
    };
    let ram_gb = input.ram_bytes as f64 / GIB as f64;
    let fits = est_memory_bytes <= budget_bytes && ram_gb >= model.min_ram_gb;
    let local = input
        .installed
        .iter()
        .find(|local| local.omlx_id == omlx_id);
    Ok(Assessed {
        est_memory_bytes,
        fits,
        size_bytes: local
            .map(|local| local.size_bytes)
            .or_else(|| known_size_bytes(omlx_id)),
        installed: local.is_some(),
    })
}

/// 计算推荐。档位只看 `ram_bytes`；`fits` 看额度或可用 GPU 内存。
pub fn recommend_chat(input: &RecommendInput<'_>) -> Result<ChatRecommendation, RecommendError> {
    let tier = TIERS
        .iter()
        .find(|tier| {
            input.ram_bytes >= tier.from_gib * GIB
                && tier.to_gib.is_none_or(|to| input.ram_bytes < to * GIB)
        })
        .unwrap_or(&TIERS[3]);

    let primary = assess(tier.primary, input)?;
    let mut alternatives = Vec::with_capacity(tier.alternatives.len());
    for id in tier.alternatives {
        let item = assess(id, input)?;
        alternatives.push(RecommendationAlternative {
            model_id: (*id).to_owned(),
            installed: item.installed,
            size_bytes: item.size_bytes,
            est_memory_bytes: Some(item.est_memory_bytes),
            fits: item.fits,
            reason: tier.reason.to_owned(),
        });
    }
    let host = RecommendationHost {
        ram_bytes: input.ram_bytes,
        allocated_budget_bytes: input.allocated_budget_bytes,
    };
    let chosen = |model_id: &str, item: &Assessed, reason: &str| ChatRecommendation {
        role: "chat".to_owned(),
        model_id: Some(model_id.to_owned()),
        installed: item.installed,
        size_bytes: item.size_bytes,
        est_memory_bytes: Some(item.est_memory_bytes),
        fits: true,
        reason: reason.to_owned(),
        host: host.clone(),
        alternatives: alternatives.clone(),
    };
    let none = |reason: &str| ChatRecommendation {
        role: "chat".to_owned(),
        model_id: None,
        installed: false,
        size_bytes: None,
        est_memory_bytes: None,
        fits: false,
        reason: reason.to_owned(),
        host: host.clone(),
        alternatives: alternatives.clone(),
    };

    if primary.fits {
        return Ok(chosen(tier.primary, &primary, tier.reason));
    }
    if tier.reason == REASON_TIER_BELOW_16 {
        return Ok(none(tier.reason));
    }
    let start = FALLBACK_ORDER
        .iter()
        .position(|id| *id == tier.primary)
        .map_or(0, |index| index + 1);
    for id in &FALLBACK_ORDER[start..] {
        let item = assess(id, input)?;
        if item.fits {
            return Ok(chosen(id, &item, REASON_FALLBACK));
        }
    }
    Ok(none(REASON_INSUFFICIENT))
}
