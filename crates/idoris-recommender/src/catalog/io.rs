use std::fs;
use std::path::Path;

use serde_yaml::Value;

use super::{Catalog, CatalogError, parse_catalog};

/// Load a catalog from exactly the caller-supplied path.
///
/// This boundary never searches a repository/default location: callers own
/// config-root resolution. Bytes must be UTF-8, then the same YAML parser and
/// semantic validator used by in-memory catalog parsing are applied.
pub fn load_catalog(path: impl AsRef<Path>) -> Result<Catalog, CatalogError> {
    let path = path.as_ref();
    let display = path.display().to_string();
    let bytes = fs::read(path).map_err(|error| {
        CatalogError::new(format!("failed to read catalog: {error}"), display.clone())
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        CatalogError::new(
            format!("catalog is not valid UTF-8: {error}"),
            display.clone(),
        )
    })?;
    let value: Value = serde_yaml::from_str(text).map_err(|error| {
        CatalogError::new(format!("catalog YAML is invalid: {error}"), display.clone())
    })?;
    parse_catalog(&value)
}
