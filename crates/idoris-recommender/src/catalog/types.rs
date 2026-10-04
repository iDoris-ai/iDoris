use std::collections::BTreeMap;

use idoris_policy::{Role, parse_model_role};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::memory::{ModelArch, QuantSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogRole(Role);

impl CatalogRole {
    pub fn new(role: Role) -> Result<Self, CatalogError> {
        if role.is_catalog_role() {
            Ok(Self(role))
        } else {
            Err(CatalogError::new("auto is not a catalog role", "$.roles"))
        }
    }

    pub const fn role(self) -> Role {
        self.0
    }

    pub fn parse(value: &str) -> Result<Self, CatalogError> {
        let model = format!("idoris/{value}");
        let role = parse_model_role(&model)
            .map_err(|_| CatalogError::new(format!("unknown catalog role {value:?}"), "$.roles"))?
            .ok_or_else(|| {
                CatalogError::new(format!("unknown catalog role {value:?}"), "$.roles")
            })?;
        Self::new(role)
    }
}

impl Serialize for CatalogRole {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for CatalogRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogQuant {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bpp: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights_gb: Option<f64>,
    pub quality: f64,
}

impl CatalogQuant {
    pub fn as_quant_spec(&self) -> QuantSpec {
        QuantSpec {
            label: Some(self.label.clone()),
            bpp: self.bpp,
            weights_gb: self.weights_gb,
            quality: self.quality,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadHint {
    OnDemand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub params_total_b: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_active_b: Option<f64>,
    pub arch: ModelArch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modality: Option<Vec<String>>,
    pub roles: Vec<CatalogRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_hint: Option<LoadHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<BTreeMap<String, f64>>,
    pub quant_options: Vec<CatalogQuant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    pub min_ram_gb: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenarios: Option<BTreeMap<String, f64>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogExcluded {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub version: f64,
    pub catalog: Vec<CatalogModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded: Option<Vec<CatalogExcluded>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError {
    pub message: String,
    pub path: String,
}

impl CatalogError {
    pub fn new(message: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: path.into(),
        }
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for CatalogError {}
