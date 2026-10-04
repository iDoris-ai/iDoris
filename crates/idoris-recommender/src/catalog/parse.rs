use std::collections::{BTreeMap, BTreeSet};

use serde_yaml::{Mapping, Value};

use super::{
    Catalog, CatalogError, CatalogExcluded, CatalogModel, CatalogQuant, CatalogRole, LoadHint,
};
use crate::memory::ModelArch;

pub fn parse_catalog(raw: &Value) -> Result<Catalog, CatalogError> {
    let root = object(raw, "$")?;
    let version = number(field(root, "version"), "$.version")?;
    let models = field(root, "catalog")
        .and_then(Value::as_sequence)
        .ok_or_else(|| CatalogError::new("catalog must be an array", "$.catalog"))?;

    let mut catalog = Vec::with_capacity(models.len());
    let mut ids = BTreeSet::new();
    for (index, value) in models.iter().enumerate() {
        let path = format!("$.catalog[{index}]");
        let model = parse_model(value, &path)?;
        if !ids.insert(model.id.clone()) {
            return Err(CatalogError::new(
                format!("duplicate model id: {}", model.id),
                "$.catalog",
            ));
        }
        catalog.push(model);
    }

    let excluded = match field(root, "excluded").and_then(Value::as_sequence) {
        None => None,
        Some(items) => Some(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| parse_excluded(item, &format!("$.excluded[{index}]")))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };

    Ok(Catalog {
        version,
        catalog,
        excluded,
    })
}

fn parse_model(value: &Value, path: &str) -> Result<CatalogModel, CatalogError> {
    let model = object(value, path)?;
    let id = string(field(model, "id"), &format!("{path}.id"))?;
    let params_total_b = number(
        field(model, "params_total_b"),
        &format!("{path}.params_total_b"),
    )?;
    let params_active_b = optional_number(
        field(model, "params_active_b"),
        &format!("{path}.params_active_b"),
    )?;
    let min_ram_gb = number(field(model, "min_ram_gb"), &format!("{path}.min_ram_gb"))?;
    let arch = parse_arch(field(model, "arch"), &format!("{path}.arch"))?;

    let quant_raw = field(model, "quant_options")
        .and_then(Value::as_sequence)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            CatalogError::new(
                "quant_options must not be empty",
                format!("{path}.quant_options"),
            )
        })?;
    let quant_options = quant_raw
        .iter()
        .enumerate()
        .map(|(index, item)| parse_quant(item, &format!("{path}.quant_options[{index}]")))
        .collect::<Result<Vec<_>, _>>()?;

    let roles_raw = field(model, "roles")
        .ok_or_else(|| CatalogError::new("roles is required", format!("{path}.roles")))?
        .as_sequence()
        .ok_or_else(|| CatalogError::new("roles must be an array", format!("{path}.roles")))?;
    let mut roles = Vec::with_capacity(roles_raw.len());
    let mut seen_roles = BTreeSet::new();
    for (index, value) in roles_raw.iter().enumerate() {
        let raw = string(Some(value), &format!("{path}.roles[{index}]"))?;
        let role = CatalogRole::parse(&raw)
            .map_err(|err| CatalogError::new(err.message, format!("{path}.roles[{index}]")))?;
        // Router model-role parsing intentionally trims the whole model name;
        // catalog roles do not. Keep the shared enum while requiring the raw
        // catalog spelling to equal its canonical role string exactly.
        if role.role().as_str() != raw {
            return Err(CatalogError::new(
                format!("unknown catalog role {raw:?}"),
                format!("{path}.roles[{index}]"),
            ));
        }
        if !seen_roles.insert(role.role().as_str()) {
            return Err(CatalogError::new(
                format!("duplicate role {raw:?}"),
                format!("{path}.roles"),
            ));
        }
        roles.push(role);
    }

    let load_hint = match field(model, "load_hint") {
        None => None,
        Some(Value::String(value)) if value == "on_demand" => Some(LoadHint::OnDemand),
        Some(value) => {
            return Err(CatalogError::new(
                format!("unknown load_hint {value:?}"),
                format!("{path}.load_hint"),
            ));
        }
    };

    let modality = match field(model, "modality") {
        None => None,
        Some(value) => {
            let items = value.as_sequence().ok_or_else(|| {
                CatalogError::new("modality must be an array", format!("{path}.modality"))
            })?;
            Some(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| string(Some(item), &format!("{path}.modality[{index}]")))
                    .collect::<Result<Vec<_>, _>>()?,
            )
        }
    };

    Ok(CatalogModel {
        id,
        family: optional_string(field(model, "family")),
        params_total_b,
        params_active_b,
        arch,
        modality,
        roles,
        load_hint,
        capability: parse_capability(field(model, "capability"), &format!("{path}.capability"))?,
        quant_options,
        license: optional_string(field(model, "license")),
        min_ram_gb,
        status: optional_string(field(model, "status")),
        note: optional_string(field(model, "note")),
        // TS declares scenarios in the type but parseCatalog intentionally drops it.
        scenarios: None,
    })
}

fn parse_arch(value: Option<&Value>, path: &str) -> Result<ModelArch, CatalogError> {
    let arch = value
        .and_then(Value::as_mapping)
        .ok_or_else(|| CatalogError::new("arch must be an object", path))?;
    Ok(ModelArch {
        n_layers: number(field(arch, "n_layers"), &format!("{path}.n_layers"))?,
        n_kv_heads: number(field(arch, "n_kv_heads"), &format!("{path}.n_kv_heads"))?,
        head_dim: number(field(arch, "head_dim"), &format!("{path}.head_dim"))?,
    })
}

fn parse_quant(value: &Value, path: &str) -> Result<CatalogQuant, CatalogError> {
    let quant = object(value, path)?;
    let label = string(field(quant, "label"), &format!("{path}.label"))?;
    let quality = number(field(quant, "quality"), &format!("{path}.quality"))?;
    let bpp = optional_number(field(quant, "bpp"), &format!("{path}.bpp"))?;
    let weights_gb = optional_number(field(quant, "weights_gb"), &format!("{path}.weights_gb"))?;
    if bpp.is_none() && weights_gb.is_none() {
        return Err(CatalogError::new(
            "quant option requires bpp or weights_gb",
            path,
        ));
    }
    Ok(CatalogQuant {
        label,
        bpp,
        weights_gb,
        quality,
    })
}

fn parse_capability(
    value: Option<&Value>,
    path: &str,
) -> Result<Option<BTreeMap<String, f64>>, CatalogError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let map = value
        .as_mapping()
        .ok_or_else(|| CatalogError::new("capability must be an object", path))?;
    let mut out = BTreeMap::new();
    for (key, raw) in map {
        let key = key
            .as_str()
            .ok_or_else(|| CatalogError::new("capability keys must be strings", path))?;
        out.insert(key.to_owned(), number(Some(raw), &format!("{path}.{key}"))?);
    }
    Ok(Some(out))
}

fn parse_excluded(value: &Value, path: &str) -> Result<CatalogExcluded, CatalogError> {
    let item = object(value, path)?;
    Ok(CatalogExcluded {
        id: string(field(item, "id"), &format!("{path}.id"))?,
        reason: string(field(item, "reason"), &format!("{path}.reason"))?,
    })
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Mapping, CatalogError> {
    value
        .as_mapping()
        .ok_or_else(|| CatalogError::new("must be an object", path))
}

fn field<'a>(map: &'a Mapping, name: &str) -> Option<&'a Value> {
    map.get(Value::String(name.to_owned()))
}

fn string(value: Option<&Value>, path: &str) -> Result<String, CatalogError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CatalogError::new("must be a non-empty string", path))
}

fn number(value: Option<&Value>, path: &str) -> Result<f64, CatalogError> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| CatalogError::new("must be a finite number", path))
}

fn optional_number(value: Option<&Value>, path: &str) -> Result<Option<f64>, CatalogError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => number(Some(value), path).map(Some),
    }
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}
