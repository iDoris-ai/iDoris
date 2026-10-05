use std::collections::BTreeMap;

use idoris_policy::Role;

use crate::catalog::{Catalog, CatalogRole};
use crate::memory::{DEFAULT_OVERHEAD_GB, KvQuant, kv_cache_gb, weights_gb};
use crate::roles::role_candidates;

#[derive(Debug, Clone, PartialEq)]
pub struct ModelEstimate {
    pub catalog_id: String,
    pub quant: String,
    pub ctx: u64,
    pub kv_quant: KvQuant,
    pub weights_gb: f64,
    pub kv_gb: f64,
    pub overhead_gb: f64,
    pub footprint_gb: f64,
    pub min_ram_gb: f64,
    pub roles: Vec<String>,
    pub capabilities: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelApiError {
    UnknownModel(String),
    UnknownQuant { model: String, quant: String },
    InvalidContext,
    InvalidRole,
    NoCandidates(String),
    InvalidEstimate,
}

impl std::fmt::Display for ModelApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownModel(id) => write!(f, "unknown catalog model {id:?}"),
            Self::UnknownQuant { model, quant } => {
                write!(f, "unknown quant {quant:?} for catalog model {model:?}")
            }
            Self::InvalidContext => f.write_str("ctx must be > 0"),
            Self::InvalidRole => f.write_str("auto is not a catalog role"),
            Self::NoCandidates(role) => write!(f, "no catalog candidates for role {role}"),
            Self::InvalidEstimate => f.write_str("model estimate must be finite and > 0"),
        }
    }
}

impl std::error::Error for ModelApiError {}

pub fn estimate_model(
    catalog: &Catalog,
    catalog_id: &str,
    quant_label: &str,
    ctx: u64,
    kv_quant: KvQuant,
) -> Result<ModelEstimate, ModelApiError> {
    if ctx == 0 {
        return Err(ModelApiError::InvalidContext);
    }
    let model = catalog
        .catalog
        .iter()
        .find(|model| model.id == catalog_id)
        .ok_or_else(|| ModelApiError::UnknownModel(catalog_id.to_string()))?;
    let quant = model
        .quant_options
        .iter()
        .find(|quant| quant.label == quant_label)
        .ok_or_else(|| ModelApiError::UnknownQuant {
            model: catalog_id.to_string(),
            quant: quant_label.to_string(),
        })?;
    let weights = weights_gb(model.params_total_b, &quant.as_quant_spec())
        .map_err(|_| ModelApiError::InvalidEstimate)?;
    let kv = kv_cache_gb(model.arch, ctx, kv_quant);
    let footprint = weights + kv + DEFAULT_OVERHEAD_GB;
    if !weights.is_finite()
        || !kv.is_finite()
        || !footprint.is_finite()
        || weights <= 0.0
        || footprint <= 0.0
    {
        return Err(ModelApiError::InvalidEstimate);
    }
    Ok(ModelEstimate {
        catalog_id: model.id.clone(),
        quant: quant.label.clone(),
        ctx,
        kv_quant,
        weights_gb: weights,
        kv_gb: kv,
        overhead_gb: DEFAULT_OVERHEAD_GB,
        footprint_gb: footprint,
        min_ram_gb: model.min_ram_gb,
        roles: model
            .roles
            .iter()
            .map(|role| role.role().as_str().to_string())
            .collect(),
        capabilities: model.capability.clone().unwrap_or_default(),
    })
}

pub fn candidates_for_role(
    catalog: &Catalog,
    role: Role,
    available_ram_gb: Option<f64>,
) -> Result<Vec<String>, ModelApiError> {
    let catalog_role = CatalogRole::new(role).map_err(|_| ModelApiError::InvalidRole)?;
    let candidates = role_candidates(catalog, catalog_role, None, available_ram_gb);
    if candidates.is_empty() {
        Err(ModelApiError::NoCandidates(role.as_str().to_string()))
    } else {
        Ok(candidates)
    }
}
