# idoris-recommender

Skeleton only — module layout staked out for a later task to fill in. Ported
from (and must stay behaviorally equivalent to) **`packages/recommender`**
(TS, kept as the reference implementation; see the root `README.md`).

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
