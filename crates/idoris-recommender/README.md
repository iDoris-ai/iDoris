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

## Public Rust boundary

`idoris-recommender` is now the maintained implementation. The TypeScript
package remains a reference/PoC used to lock parity fixtures; new product
behavior belongs in Rust.

Typical B1 consumption is explicit and side-effect free:

```rust
let catalog = idoris_recommender::catalog::load_catalog("config/catalog.yaml")?;
let rec = idoris_recommender::recommend::recommend(&facts, &catalog, None, None)?;
let estimate = idoris_recommender::model::estimate_model(
    &catalog,
    "ornith-1.0-9b",
    "q6_k",
    rec.policy.context_target,
    rec.policy.kv_quant,
)?;
```

The crate never loads a model, changes sysctl state, reads provider
credentials, or talks to oMLX. B1 owns backend binding, live queue state and
Supervisor admission; A-machine acceptance owns real memory calibration.

## Parity and allowed differences

Shared fixtures pin the TS reference baseline at
`49a66e86e65e9ddf26f3bf3c9d68041df49d8981`
(`testdata/recommender/{ram,edges}.json`). Valid-input recommendation output,
ordering, warnings/tradeoff text, and numeric results remain parity-sensitive.

Allowed Rust differences are intentionally narrow:

- Rust may return structured/path-aware errors instead of reproducing JS
  exception class names or stack text.
- strict loading-facing APIs reject zero/non-finite context or invalid model
  estimates fail-closed even when the pure TS recommender had no equivalent
  loader boundary.
- real OS sampling stays behind `HostProbe`; injected facts are not evidence
  that a physical host was measured.

## Real-hardware calibration template

Before changing defaults or claiming calibrated memory accuracy, record:

| Field | Required evidence |
|---|---|
| repo/catalog commit | exact SHA + catalog SHA-256 |
| host | RAM, chip, OS, GPU facts and sampling source |
| runtime | oMLX/backend version and actual backend model id |
| model binding | provider id, catalog id, quant, actual runtime id |
| memory | baseline, post-load, prefill/long-generation peak, post-unload |
| context | ctx target + KV quant |
| result | predicted footprint vs observed footprint, with explanation |

Current status: fixture parity, injected HostFacts, catalog parsing, formulas,
recommendation and B1 consumer contracts are tested. Real M1 Max model
footprint calibration and live B1 task15/task33 backend consumption remain
A-machine acceptance work and must not be described as already verified.
