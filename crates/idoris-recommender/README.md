# idoris-recommender

Maintained Rust recommender. Calls without an external allocation preserve the
**`packages/recommender`** TS reference fixtures.

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

With no external allocation, a forced model may appear as both resident and
blocked. Consumers preserve both legacy rows. With an allocation, a forced model
cannot bypass total memory or the real host `min_ram_gb` gate.

## Scope (mirrors `packages/recommender/src/*`)

| Rust module | TS source | Responsibility |
|---|---|---|
| `probe` | `src/probe.ts` | `HostFacts` + adapters over `node:os`/`system_profiler`. Recommendation logic depends only on `HostFacts`, never touches the OS directly, so it stays testable with injected facts. |
| `memory` | `src/memory.ts` (T2.1.1, docs/07 §1–§3, Apple budget correction in docs/13 §3.1) | `footprint_gb = params_total_b * bpp(quant) + kv_gb(ctx) + overhead_gb`. Unit conventions are load-bearing and easy to get wrong: weights and KV estimates are decimal GB (1 GB = 1e9 B); RAM budgets follow docs/13's table when no external allocation is supplied. Do not "simplify" the units without re-reading the TS header comment. |
| `recommend` | `src/recommend.ts` (T2.1.2/T2.1.3, docs/07 §5.3, threshold correction in docs/13 §9.2) | `min_ram_gb` gates real host RAM. The legacy unbudgeted forced override can bypass it with a warning. Resident pick maximizes `capability.reasoning * quant.quality` subject to fitting the resident budget; on-demand candidates are admitted one at a time; `IDORIS_CORE_MODEL` forces a yield with a warning, never a silent override. |

## Public Rust boundary

`idoris-recommender` is now the maintained implementation. The TypeScript
package remains a reference/PoC used to lock parity fixtures; new product
behavior belongs in Rust.

Typical B1 consumption is explicit and side-effect free:

```rust
let catalog = idoris_recommender::catalog::load_catalog("config/catalog.yaml")?;
let rec = idoris_recommender::recommend::recommend(
    &facts, &catalog, None, None, Some(12_000_000_000),
)?;
let estimate = idoris_recommender::model::estimate_model(
    &catalog,
    "ornith-1.0-9b",
    "q6_k",
    rec.policy.context_target,
    rec.policy.kv_quant,
)?;
```

Both `recommend(hardware, catalog, partial_policy, forced, allocated_budget_bytes)`
and `recommend_from_file(path, hardware, partial_policy, forced, allocated_budget_bytes)`
accept `allocated_budget_bytes: Option<u64>`. Existing callers pass `None` for the
legacy host-derived budget. This is a direct signature migration.

### Allocated total and admission

`Some(bytes)` replaces host-derived usable memory with the allocated total. It
does not apply a wired fraction or subtract OS reserve again. `reserve_gb` is
zero in this mode. Resident selection still reserves `3.5GB * temp_slots` and
`1GB` of headroom within that total. On-demand candidates are evaluated one at
a time against total minus the resident footprint, without cumulative deduction
across alternatives.

| Candidate footprint | Allocated admission |
|---|---|
| Fits remaining | `Ready` |
| Exceeds remaining but fits total, including equality | `RequiresEviction` |
| Even the smallest quant exceeds total | In `Recommendation.blocked`, absent from resident and temp |
| Real host RAM is below `min_ram_gb` | In `Recommendation.blocked`, regardless of allocation size |

`Some(0)` blocks every positive footprint. Quant selection still prefers the
highest capability model and then quality; it does not switch to a weaker model
when the highest capability candidate is blocked. The existing lowest-footprint
quant fallback can relax the quality threshold. In allocated mode its status
uses the actual footprint comparison with remaining memory.

A forced quant must fit allocated total and real host eligibility. A rejected
known override has `override_choice.active = false`, emits a `BLOCKED` warning,
and preserves the automatic resident. A fitting override may spend the soft
resident reserve but cannot exceed total. Unknown model overrides retain their
existing automatic fallback.

The router's `LiveCapabilitiesProvider::with_allocated_budget_bytes(Option<u64>)`
passes the allocation into each snapshot. Blocked models map to HTTP
`admission_status: "blocked"`. The maintained `idoris serve` binary accepts the
optional `IDORIS_ALLOCATED_BUDGET_BYTES` unsigned integer environment variable;
unset means `None`, and malformed or overflowing values reject startup. This
configures recommendations and capability reporting; the same `Option<u64>` also
sets the capacity ledger for lifecycle (Supervisor) load admission. It does not
hard-limit external resident services or actual RSS, and there is no dynamic
budget API or live Agent24 allocation negotiation protocol.

### Units and footprint limits

`HostFacts.ram_gb` represents real physical memory as
`round(total_mem_bytes / 2^30)`, despite its historical name. OS sampling rounds
to the nearest whole GiB, preserving the existing host-probe contract. Injected
facts must describe the real host; the pure constructor continues to accept fractional fixture facts. `min_ram_gb` compares with those
facts, never with an allocation disguised as RAM.

Agent24 passes integer bytes at the boundary. iDoris converts the allocation
once to decimal GB (`bytes / 1_000_000_000`) in `budget_breakdown` and shares the
result across selection stages. It never converts the allocation through GiB.
Weights, KV, overhead, and reported footprints use decimal GB. KV is computed
from architecture, the policy's specified context, and KV quantization. The
estimate is `weights + KV(context_target) + 1GB` per-model runtime overhead.
Loading peaks, temporary loading buffers, and measured backend peaks are **not
modeled**. This is an estimated admission limit, not a claim that observed peak
RSS stays below the allocation. Estimates and GB comparisons use `f64`. Integer
byte inputs are exactly representable up to 2^53 bytes, but conversion to GB and
footprint arithmetic can still introduce rounding at ordinary model sizes
(including approximately one byte at an admission boundary). This is not a
byte-exact memory allocator; precision degrades further above 2^53 bytes.
Loading peaks require separate margin and backend calibration.
`recommended_sysctl` remains advice for the physical host, derived from its
legacy usable memory and wired mode, independently of the allocation.

The crate never loads a model, changes sysctl state, reads provider
credentials, or talks to oMLX. B1 owns backend binding, live queue state and
Supervisor admission; A-machine acceptance owns real memory calibration.

## Parity and allowed differences

Shared fixtures pin the TS reference baseline at
`49a66e86e65e9ddf26f3bf3c9d68041df49d8981`
(`testdata/recommender/{ram,edges}.json`). Valid-input recommendation output,
ordering, warnings/tradeoff text, and numeric results remain parity-sensitive
when `allocated_budget_bytes` is `None`. In particular, the existing
`temp-too-large-even-alone-requires-eviction` edge fixture labels a model above
legacy usable memory as `RequiresEviction`, and oversized forced overrides can
remain resident. These are explicit legacy exceptions. The allocated path
rejects these admissions. All shared fixtures remain unchanged.

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

## Degraded alternative (owner decision 2026-10-27)

When the best on-demand model for a capability does not fit the remaining
budget, the user must be told and offered two choices:

- (a) evict unused models, then load the best model. This is the existing
  `TempChoice` itself (`status = RequiresEviction`, `quant`).
- (b) do not evict and degrade to a smaller model that fits now. This is the new
  `TempChoice.degraded_alternative: Option<TempAlternative>` (`id`, `capability`,
  `quant`, `reason`).

`degraded_alternative` is `Some` only for `RequiresEviction`: the best-scoring
other eligible model (not experiment, `min_ram_gb` satisfied, not the resident)
for the same capability whose quant fits the remaining budget without eviction.
It is `None` for `Ready` or when nothing fits. In allocated mode, a best model
larger than the allocated total stays out of `temp` (it is in `blocked`); its
alternatives are on `BlockedChoice.degraded_alternatives`, one per capability
(each `TempAlternative` carries its `capability`). As with `Ready`, remaining
memory is not deducted cumulatively across capabilities, so alternatives for
different capabilities may not fit together. Existing warnings,
reasons and tradeoff text are unchanged, and the HTTP `/capabilities` shape is
unchanged (the router does not expose the alternative yet).

## Headroom breach option (owner decision)

The `HEADROOM_GB` (1GB) margin is kept by default. `Recommendation.headroom_breach_option:
Option<ResidentChoice>` is the best resident that would fit if that margin were
spent (resident budget + `HEADROOM_GB`, never above the allocated total or
usable memory). It is filled only when its score is strictly higher than the
automatic resident (or there is no automatic resident) and no forced override
is active. It never replaces `resident`, never changes temp admission, and does
not appear in warnings or tradeoff text.

The option takes effect only after the user confirms it each time; it must
never be applied automatically. The confirmation UI belongs to Agent24; how the
option is exposed over HTTP is deferred to a later phase to be agreed with
Agent24.
