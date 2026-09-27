/// Ties a Rust type to the schema file it is generated from, and to the
/// exact set of top-level JSON keys it (de)serializes.
///
/// `tests/contract_drift.rs` reads the live schema file and asserts its
/// `properties`/`required` key sets equal [`PROPERTIES`]/[`REQUIRED`] below —
/// which are declared right next to each struct on purpose. If someone edits
/// `packages/contracts/schema/*.schema.json` (adds/removes/renames a
/// property, or changes what's required) without updating the matching
/// struct *and* this list, `cargo test` fails with `DRIFT: <file>`. This is
/// the Rust-side equivalent of `pnpm check:contract-drift` — the TS side
/// catches drift by re-running codegen and diffing committed output; we
/// don't generate code, so we pin the shape declaratively instead.
///
/// [`PROPERTIES`]: SchemaShape::PROPERTIES
/// [`REQUIRED`]: SchemaShape::REQUIRED
pub trait SchemaShape {
    /// File name under `packages/contracts/schema/` (the real source of truth).
    const SCHEMA_FILE: &'static str;
    /// Every top-level key in the schema's `properties`.
    const PROPERTIES: &'static [&'static str];
    /// The schema's top-level `required` array (a subset of `PROPERTIES`).
    const REQUIRED: &'static [&'static str];
}
