use idoris_policy::{Role, is_role_metadata_eligible};

use crate::catalog::{Catalog, CatalogModel, CatalogRole};

/// Return eligible model ids in catalog declaration order.
///
/// `installed_model_ids=None` means "installation state unknown / do not
/// filter". `Some(&[])` means the caller knows that nothing is installed and
/// therefore returns no candidates. Load hints are deliberately not part of
/// role eligibility: roles and loading strategy are independent axes.
pub fn role_candidates(
    catalog: &Catalog,
    role: CatalogRole,
    installed_model_ids: Option<&[String]>,
    available_ram_gb: Option<f64>,
) -> Vec<String> {
    catalog
        .catalog
        .iter()
        .filter(|model| model_is_eligible(model, role.role(), available_ram_gb))
        .filter(|model| {
            installed_model_ids.is_none_or(|installed| installed.iter().any(|id| id == &model.id))
        })
        .map(|model| model.id.clone())
        .collect()
}

fn model_is_eligible(model: &CatalogModel, role: Role, available_ram_gb: Option<f64>) -> bool {
    let roles = model
        .roles
        .iter()
        .map(|role| role.role())
        .collect::<Vec<_>>();
    is_role_metadata_eligible(
        model.status.as_deref() == Some("experiment"),
        model.min_ram_gb,
        &roles,
        role,
        available_ram_gb,
    )
}
