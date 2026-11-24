//! Maintained catalog recommender. See `README.md` for allocated budget and
//! legacy TypeScript parity contracts.

/// Hardware probing (`packages/recommender/src/probe.ts`): `HostFacts` and
/// its adapters (`os::totalmem`, CPU model string, GPU core count via
/// `system_profiler` on macOS). Decisions only ever read `HostFacts` — real
/// probing is an adapter, so recommendation logic can be tested against
/// injected facts without depending on the test machine's hardware.
pub mod probe;

/// Memory footprint formulas (`packages/recommender/src/memory.ts`, T2.1.1):
/// `footprint(GB) = params_total_b * bpp(quant) + KV(ctx) + overhead`. Unit
/// conventions (decimal GB for weights and KV, Apple RAM budget table)
/// must be preserved exactly — see the TS source's header comment before
/// touching any of this.
pub mod memory;

/// Typed model-catalog surface. Parsing/validation lives in follow-up task09.
pub mod catalog;

/// Catalog role candidate selection, sharing the policy crate's metadata gate.
pub mod roles;

/// Public catalog-model estimate / role-candidate boundary for B1 consumers.
pub mod model;

/// 本机 chat 模型推荐（接口规范 §2.9，wire 1.2.0）：档位表 + 回落 + `fits`，纯函数。
pub mod chat_recommendation;

/// 本地模型列表（接口规范 §2.9）：只读扫描 oMLX 模型目录，按 catalog → `config.json` → `unknown` 定 `kind`。
pub mod local_models;

/// 并发请求共用一次进行中的计算（不缓存结果）：本地模型目录的扫描用它，避免请求并发时重复遍历磁盘。
pub mod single_flight;

/// `HardwareAwareModelRecommender` (`packages/recommender/src/recommend.ts`,
/// T2.1.2/T2.1.3): picks a resident + on-demand model combination given
/// `HostFacts` + a model catalog + policy, with hard `min_ram_gb` gating and
/// `IDORIS_CORE_MODEL` override support.
pub mod recommend;
