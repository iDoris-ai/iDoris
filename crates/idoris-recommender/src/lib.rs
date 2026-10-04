//! Skeleton crate — see `README.md` in this directory for scope and the TS
//! package it corresponds to. R1 only stakes out the module layout; no
//! business logic is ported here yet.

/// Hardware probing (`packages/recommender/src/probe.ts`): `HostFacts` and
/// its adapters (`os::totalmem`, CPU model string, GPU core count via
/// `system_profiler` on macOS). Decisions only ever read `HostFacts` — real
/// probing is an adapter, so recommendation logic can be tested against
/// injected facts without depending on the test machine's hardware.
pub mod probe;

/// Memory footprint formulas (`packages/recommender/src/memory.ts`, T2.1.1):
/// `footprint(GB) = params_total_b * bpp(quant) + KV(ctx) + overhead`. Unit
/// conventions (decimal GB for weights, MiB for KV, Apple RAM budget table)
/// must be preserved exactly — see the TS source's header comment before
/// touching any of this.
pub mod memory;

/// Typed model-catalog surface. Parsing/validation lives in follow-up task09.
pub mod catalog;

/// Catalog role candidate selection, sharing the policy crate's metadata gate.
pub mod roles;

/// Public catalog-model estimate / role-candidate boundary for B1 consumers.
pub mod model;

/// `HardwareAwareModelRecommender` (`packages/recommender/src/recommend.ts`,
/// T2.1.2/T2.1.3): picks a resident + on-demand model combination given
/// `HostFacts` + a model catalog + policy, with hard `min_ram_gb` gating and
/// `IDORIS_CORE_MODEL` override support.
pub mod recommend;
