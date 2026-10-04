use serde::{Deserialize, Serialize};

pub const DEFAULT_OVERHEAD_GB: f64 = 1.0;
const BYTES_PER_GB: f64 = 1_000_000_000.0;
const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;
const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuantEntry {
    pub bpp: f64,
    pub quality: f64,
}

pub const QUANT_TABLE: [(&str, QuantEntry); 7] = [
    (
        "fp16",
        QuantEntry {
            bpp: 2.0,
            quality: 1.0,
        },
    ),
    (
        "q8_0",
        QuantEntry {
            bpp: 1.0,
            quality: 0.998,
        },
    ),
    (
        "q6_k",
        QuantEntry {
            bpp: 0.82,
            quality: 0.995,
        },
    ),
    (
        "q5_k_m",
        QuantEntry {
            bpp: 0.69,
            quality: 0.99,
        },
    ),
    (
        "q4_k_m",
        QuantEntry {
            bpp: 0.55,
            quality: 0.98,
        },
    ),
    (
        "q3_k",
        QuantEntry {
            bpp: 0.43,
            quality: 0.95,
        },
    ),
    (
        "q2_k",
        QuantEntry {
            bpp: 0.30,
            quality: 0.85,
        },
    ),
];

pub fn lookup_quant(label: &str) -> Option<QuantEntry> {
    QUANT_TABLE
        .iter()
        .find_map(|(candidate, entry)| (*candidate == label).then_some(*entry))
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuantSpec {
    pub label: Option<String>,
    pub bpp: Option<f64>,
    pub weights_gb: Option<f64>,
    pub quality: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KvQuant {
    Fp16,
    Q8,
    Q4,
}

impl KvQuant {
    pub const fn bytes_per_element(self) -> f64 {
        match self {
            Self::Fp16 => 2.0,
            Self::Q8 => 1.0,
            Self::Q4 => 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WiredMode {
    Conservative,
    Moderate,
    Aggressive,
}

impl WiredMode {
    pub const fn fraction(self) -> f64 {
        match self {
            Self::Conservative => 0.66,
            Self::Moderate => 0.70,
            Self::Aggressive => 0.75,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelArch {
    pub n_layers: u32,
    pub n_kv_heads: u32,
    pub head_dim: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryError;

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("quant spec requires weights_gb or bpp")
    }
}

impl std::error::Error for MemoryError {}

pub fn bytes_to_gb(bytes: f64) -> f64 {
    bytes / BYTES_PER_GB
}

pub fn bytes_to_mib(bytes: f64) -> f64 {
    bytes / BYTES_PER_MIB
}

pub fn bytes_to_gib(bytes: f64) -> f64 {
    bytes / BYTES_PER_GIB
}

pub fn weight_bytes(params_total_b: f64, spec: &QuantSpec) -> Result<f64, MemoryError> {
    if let Some(weights_gb) = spec.weights_gb {
        return Ok(weights_gb * BYTES_PER_GB);
    }
    if let Some(bpp) = spec.bpp {
        return Ok(params_total_b * bpp * BYTES_PER_GB);
    }
    Err(MemoryError)
}

pub fn weights_gb(params_total_b: f64, spec: &QuantSpec) -> Result<f64, MemoryError> {
    weight_bytes(params_total_b, spec).map(bytes_to_gb)
}

pub fn kv_bytes(arch: ModelArch, ctx: u64, kv_quant: KvQuant) -> f64 {
    2.0 * f64::from(arch.n_layers)
        * f64::from(arch.n_kv_heads)
        * f64::from(arch.head_dim)
        * ctx as f64
        * kv_quant.bytes_per_element()
}

pub fn kv_cache_gb(arch: ModelArch, ctx: u64, kv_quant: KvQuant) -> f64 {
    bytes_to_gb(kv_bytes(arch, ctx, kv_quant))
}

pub fn footprint_gb(
    params_total_b: f64,
    quant: &QuantSpec,
    arch: ModelArch,
    ctx: u64,
    kv_quant: KvQuant,
    overhead_gb: Option<f64>,
) -> Result<f64, MemoryError> {
    Ok(weights_gb(params_total_b, quant)?
        + kv_cache_gb(arch, ctx, kv_quant)
        + overhead_gb.unwrap_or(DEFAULT_OVERHEAD_GB))
}

pub fn apple_reserve_gb(ram_gb: f64) -> f64 {
    (ram_gb * 0.30).clamp(3.0, 16.0)
}

pub fn apple_usable_gb(ram_gb: f64, mode: WiredMode) -> f64 {
    (ram_gb * mode.fraction()).min(ram_gb - apple_reserve_gb(ram_gb))
}

pub fn recommended_wired_limit_mb(usable_gb: f64) -> i64 {
    (usable_gb * 1024.0).round() as i64
}
