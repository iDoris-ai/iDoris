# idoris-recommender

Rust recommender port, kept behaviorally equivalent to **`packages/recommender`**
(TS, kept as the reference implementation; see the root `README.md`).

## B1 `/capabilities` consumer mapping

The recommender intentionally does not own HTTP or backend queue state. B1 can
map public `Recommendation` output onto the seven-field TS
`CapabilityEntry`:

| CapabilityEntry | Recommender source |
|---|---|
| `id` | resident/temp/blocked `id` |
| `capability` | resident=`reasoning`; temp=`capability`; blocked=highest catalog capability |
| `resident` | true only for the resident choice |
| `estimated_memory_gb` | **`footprint_gb`** (weights + KV + overhead), never weights alone |
| `ctx_limit` | resident `ctx`; otherwise recommendation policy `context_target` |
| `queue_depth` | **B1 runtime/backend status only**; B2 must not fabricate it |
| `admission_status` | resident=`ready`; temp status; blocked=`blocked` |

A forced model may intentionally appear both as resident and blocked; consumers
must preserve both rows rather than deduplicate away the warning/evidence.

## Scope (mirrors `packages/recommender/src/*`)

| Rust module | TS source | Responsibility |
|---|---|---|
| `probe` | `src/probe.ts` | `HostFacts` + adapters over `node:os`/`system_profiler`. Recommendation logic depends only on `HostFacts`, never touches the OS directly, so it stays testable with injected facts. |
| `memory` | `src/memory.ts` (T2.1.1, docs/07 §1–§3, Apple budget correction in docs/13 §3.1) | `footprint_gb = params_total_b * bpp(quant) + kv_gb(ctx) + overhead_gb`. Unit conventions are load-bearing and easy to get wrong: weights are decimal GB (1 GB = 1e9 B), KV is MiB (2^20 B), RAM budgets follow docs/13's table. Do not "simplify" the units without re-reading the TS header comment. |
| `recommend` | `src/recommend.ts` (T2.1.2/T2.1.3, docs/07 §5.3, threshold correction in docs/13 §9.2) | `min_ram_gb` is a hard gate (below it, a combination is `BLOCKED`, no exceptions). Resident pick maximizes `capability.reasoning * quant.quality` subject to fitting the resident budget; on-demand candidates are admitted one at a time; `IDORIS_CORE_MODEL` forces a yield with a warning, never a silent override. |

## Explicitly not done in R1

No business logic is ported — formulas, thresholds, and the recommender
algorithm all still live only in `packages/recommender`. Depends on
`idoris-contracts` for whatever contract types (e.g. component/provider
descriptors) a real implementation will eventually need.
