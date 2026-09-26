# Selecting Local LLMs Against Real User Scenarios (2026-09-19)

**Headline recommendation.** Don't pick models off a leaderboard. Build a small versioned scenario suite per role and run it through **Inspect AI** (UK AISI) — or **promptfoo** for YAML+CI ergonomics — with **deterministic checks first and one calibrated rubric judge second**. From public benchmarks run exactly two: **BFCL v4 non-live** and **τ²-bench `airline`/`retail`**. Spend the saved effort on scenario authoring and the human feedback loop; that is where role selection is actually decided.

## 1. Frameworks — offline against a local OpenAI-compatible endpoint

| Framework | Ver / ★ / License | Offline-local | Scenarios | Judge | Regression |
|---|---|---|---|---|---|
| [lm-eval-harness](https://github.com/EleutherAI/lm-evaluation-harness) ✅ | 0.4.13 · 14.0k · MIT | ✅ `local-chat-completions` | ✅ YAML | ⚠️ custom metric | ❌ |
| [lighteval](https://github.com/huggingface/lighteval) ✅ | 0.13.0 · 2.5k · MIT | ✅ litellm/tgi/vllm | ✅ custom task | ⚠️ custom metric | ⚠️ sample logs |
| [Inspect AI](https://github.com/UKGovernmentBEIS/inspect_ai) ✅ | 0.3.266 · 2.8k · MIT | ✅ Ollama/vLLM/llama-cpp/OpenAI-compat | ✅ | ✅ `model_graded_qa` | ✅ [eval-sets](https://inspect.aisi.org.uk/eval-sets.html), [Inspect Flow](https://meridianlabs-ai.github.io/inspect_flow/) |
| [promptfoo](https://github.com/promptfoo/promptfoo) ✅ | 0.123.1 · 25.3k · MIT | ✅ documented offline mode | ✅ YAML | ✅ `llm-rubric`, `g-eval` | ✅ baseline diff, CI |
| [OpenAI Evals](https://github.com/openai/evals) ✅ | stale (2026-04) · 19.5k · MIT | ❌ OpenAI API | ⚠️ | ⚠️ | ❌ |
| [deepeval](https://github.com/confident-ai/deepeval) ✅ | 4.2.3 · 18.3k · Apache-2.0 | ✅ Ollama/LM Studio | ✅ | ✅ G-Eval | ✅ pytest + SQLite |
| [ragas](https://github.com/vibrantlabsai/ragas) ✅ | 0.4.3 · 15.8k · Apache-2.0 | ⚠️ wrapper-dependent | ⚠️ RAG-only | ✅ | ❌ |
| [MLflow](https://mlflow.org/docs/latest/genai/eval-monitor/) ✅ | 3.16.1 · 28.0k · Apache-2.0 | ✅ local server | ✅ `@scorer` | ✅ built-in + `Guidelines()` | ✅ [CI/CD](https://mlflow.org/docs/latest/genai/eval-monitor/regression-testing/) |
| [Giskard](https://github.com/Giskard-AI/giskard-oss) ✅ | 3.0.0 · 5.8k · Apache-2.0 | ✅ local evals | ✅ | ✅ | ⚠️ `run_count`, CI |

**2026 entrants.** [CrucibleBench](https://zenodo.org/records/21386663) ✅ shows rankings swing on classifier-dependent score components; it proposes **ranking-stability checks under judge-dimension removal**. [JADE](https://icml.cc/virtual/2026/poster/63884) ✅ adds claim-level **evidence-dependency gating**; [Petri 2.0](https://alignment.anthropic.com/2026/petri-v2/) ✅ adds eval-awareness mitigations. ⚠️ OpenAI is acquiring promptfoo (page exists; fetch 403).

## 2. Agentic benchmarks — what a Mac can run

| Benchmark | Infra | Mac? | Runtime | Judge |
|---|---|---|---|---|
| **BFCL v4** non-live single-turn ✅ | pip, OpenAI-compat | ✅ easy | mins–1h | ❌ AST/exec |
| **τ²/τ³-bench** airline/retail ✅ | uv py3.12, no Docker | ✅ | 2–6h | ✅ user-sim |
| **ToolSandbox** ✅ | conda py3.9, arm64 native | ✅ | 1–3h | ✅ OpenAI sim |
| **MCP-Atlas** keyless subset ✅ | Docker ≥10GB | ✅ | 3–8h | ✅ Gemini 3.1 Pro |
| SWE-bench Verified ✅ 5.0.1 | Docker per task | ✅ heavy | 10–40h | ❌ tests |
| Terminal-Bench ✅ v4.0.0 | Docker/Modal | ❌ | days | ❌ tests |
| Claw-Eval ✅ v1.1.0 | uv + sandbox | ⚠️ partial | 2–6h ×3 | ✅ cloud only |
| GAIA ✅ | — | ❌ **repo 404** | — | ❌ |

Run order: BFCL non-live → τ²-bench airline/retail → MCP-Atlas keyless → ToolSandbox. The latter three need a cloud simulator/judge.

## 3. LLM-as-judge on local models

**Verdict: same-size-class judging is viable for ranking, not absolute quality.** Fine-tuned 1.7B–13B specialists rival far larger judges (Prometheus-2-7B Pearson 0.666 vs GPT-4-1106; GLIDER-3B beats GPT-4o on FLASK; a 1.7B probe beats an 8B generative judge) — capability comes from training, not size. But biases recur: position bias to 25pp at 1.7B–7B; central-tendency compression few-shot does **not** fix; 35% preference flips from distractors; [sycophancy 58%](https://arxiv.org/abs/2502.08177). Two 2026 corrections: self-preference is **partly an artifact** — only 51% of reported cases survive an evaluator-quality null ([2601.22548](https://arxiv.org/abs/2601.22548)) — and **panels are the *least* reliable mitigation**: nine judges across seven families carry ~2 effective votes ([2605.29800](https://arxiv.org/abs/2605.29800)).

Best mitigations: swapped-order + rubric + reference (high); reasoning-first (moderate). Panels, few-shot anchoring and logprob scoring all underperform.

Absolute scoring is more robust to distractors (9% vs 35% flips); pairwise has the higher ceiling. Use **pointwise reasoning + pairwise decision**, calibrate on 100–300 hand-labeled items from your own traffic, and report agreement *and* kappa/Spearman. **Below 7B never use free-form absolute scoring**.

## 4. Scenario and rubric construction

**Personas → requirements.** Derive roles PersonaHub-style, then freeze each as a versioned spec (task types, input style, hard constraints, must-not-do). Style is itself a test dimension — [persona-augmented benchmarking](https://aclanthology.org/2025.emnlp-main.1155/) shows surface form alone shifts scores.

**Synthetic data, validated.** Generate with a model from a **different family than any candidate** (generator heterogeneity lifts human-benchmark correlation 0.655→0.833). Guard [degeneration](https://arxiv.org/abs/2412.02980) with the reference-free **Vendi Score**, MinHash+cosine dedup, difficulty spread, and a human spot-check.

**User simulators.** Verify the simulator before trusting any agent score: [τ²-bench](https://arxiv.org/abs/2506.07982) constrains by environment state (16%/6% critical error vs 40%/12%). But [Lost in Simulation](https://aclanthology.org/2026.acl-long.2192/) finds success varying up to 9pp across simulators with demographic miscalibration, and [non-collaborative sims](https://arxiv.org/abs/2509.23124) show agent-friendly simulators **overestimate** capability by up to 29%.

**Rubrics.** Follow [Rubrics as Rewards](https://arxiv.org/abs/2507.17746): 7–20 **self-contained binary criteria** weighted Essential/Important/Optional/Pitfall, grounded in reference answers (reference-free synthesis scores worse, 0.320 vs 0.359). Rubrics that **pass for the wrong reason**: [The Silent Judge](https://arxiv.org/abs/2509.26072) shows provenance/recency cues shift verdicts up to 30% while judges acknowledge the cue **zero** percent of the time. Defend with cue-swap invariance tests, banning hygiene-only criteria, requiring criteria to cite the artifact span satisfying them, and ≥2 negative controls per role. RaR explicitly **did not test reward hacking** — don't cite it as hack-resistant.

## 5. Closing the feedback loop

Sample traces → open coding → axial coding to saturation → failure taxonomy → evaluators → promote confirmed failures into a versioned dataset that gates CI.

- Annotate ≥30 traces yourself; continue to ~100 or until failure modes stop changing. Log only the **first** failure. Re-run every 2–4 weeks ([error analysis](https://hamel.dev/blog/posts/evals-faq/why-is-error-analysis-so-important-in-llm-evals-and-how-is-it-performed.html)).
- Typically 3 modes ≈ 60%+ of problems — build those evaluators first.
- LLM judges need **100–200 labeled examples per failure mode** (train/dev/test 10–20/40–45/40–45, each with 30–50 pass and 30–50 fail) ([sample size](https://hamel.dev/blog/posts/evals-faq/how-many-examples-do-i-need-for-an-eval.html)).
- Trace→test is native: [LangSmith](https://docs.langchain.com/langsmith/manage-datasets-in-application), [Langfuse](https://langfuse.com/resources/engineering/user-feedback-to-evaluation-datasets), [Braintrust](https://www.braintrust.dev/docs/guides/datasets), Inspect's [`samples_df()`](https://inspect.aisi.org.uk/dataframe.html.md). Pin an immutable dataset version per CI gate; retire cases that always pass.
- Use **one domain expert** as annotator. With ≥2: label independently, measure agreement, discuss only disagreements, patch the rubric. κ .61–.80 "substantial", though [McHugh](https://www.ncbi.nlm.nih.gov/pmc/articles/PMC3900052/) argues .41+ is too lenient; Krippendorff α ≥.8 reliable.
- Contamination is real ([TS-Guessing](https://arxiv.org/abs/2311.09783)); **form alone moves ranks up to 8 positions** ([2402.01781](https://arxiv.org/abs/2402.01781)).

## 6. Statistical rigor

- **Items, not samples.** Resampling K answers/item removes only conditional variance (K=2 cuts 1/3, ceiling 2/3). Buy items ([Miller/Anthropic](https://arxiv.org/abs/2411.00640)).
- **~1,000 items** to detect 3pp at 80% power. n=200 binary items → SE 3.5pp, unpaired CI ±6.9pp: **sub-7pp gaps are unresolvable unpaired.** Compare **paired per-item differences**, never headline totals, and cluster-adjust (clustered SE up to 3× naive).
- **Temperature 0 is not deterministic** — float non-associativity and missing batch invariance; 75.8/51.0/47.6% of code tasks gave zero identical outputs across repeats ([2308.02828](https://arxiv.org/abs/2308.02828), [Thinking Machines](https://thinkingmachines.ai/blog/defeating-nondeterminism-in-llm-inference/)). Don't lower temperature to buy precision.
- **Infra confounds as much as models** — 6pp swing across infra on Terminal-Bench 2.0 (p<0.01) plus time-of-day drift ([Anthropic](https://www.anthropic.com/engineering/infrastructure-noise)). Pin hardware/concurrency; run a control model as canary.
- **9 models = 36 pairwise tests.** Control FDR (Benjamini–Hochberg 5%) with paired tests; pre-register model set and primary metric. [NIST AI 800-3](https://nvlpubs.nist.gov/nistpubs/ai/NIST.AI.800-3.pdf) recommends GLMMs, ICC and Kish effective n.
- **Winner rule:** paired CI excludes 0, effect ≥ pre-declared MDE, survives FDR across all comparisons, reproduces across ≥2 seeds and ≥2 infra configs, configs pinned.

## 7. Minimal viable stack

1. **Runner:** Inspect AI; add promptfoo only for YAML diffs in CI.
2. **Public benchmarks (2):** BFCL v4 non-live + τ²-bench airline/retail, quarterly, as a sanity floor — not the decider.
3. **Scenarios:** 30–60 per role × 5 roles, generated by an excluded model family, validated with Vendi + dedup + spot-check. Deterministic checks first; 15–25 reference-grounded rubric items per role; ≥2 negative controls.
4. **Judge:** one fine-tuned 3B–13B rubric judge, swapped-order pairwise, calibrated on 100–300 human-labeled items; re-audit on every swap.
5. **Loop:** versioned dataset, error analysis every 2–4 weeks, confirmed failures promoted to regressions, CI gated on the pinned version.
6. **Stats:** paired per-item tests, ≥1,000 items or accept a coarse MDE, BH-FDR, two seeds, two infra configs.

## What to skip

- **GAIA** (harness repo 404s), **Terminal-Bench full suite** (days of containers), **SWE-bench Verified** (disowned over contamination; 10–40h, ARM needs Buildx).
- **Claw-Eval / MCP-Atlas as primary signals** — cloud-only grading.
- **Leaderboard-driven selection** — form perturbations move ranks up to 8 positions.
- **OpenAI Evals and ragas** — stale (2026-04, 2026-02) and not scenario-shaped.
- **Generative absolute scoring below 7B** — use a fine-tuned per-criterion yes/no head.
- **Multi-judge panels** (~2 effective votes from 9), **few-shot anchoring** (doesn't fix central tendency), and **temperature-0 single-run comparisons** (you will declare a winner from noise).
- **Expecting frameworks to replace error analysis.**

## Sources

**Frameworks** [lm-eval](https://github.com/EleutherAI/lm-evaluation-harness) · [lighteval](https://github.com/huggingface/lighteval) · [Inspect providers](https://inspect.aisi.org.uk/providers.html) · [promptfoo FAQ](https://www.promptfoo.dev/docs/faq/) · [promptfoo assertions](https://www.promptfoo.dev/docs/configuration/expected-outputs/) · [deepeval](https://deepeval.com/integrations/models/ollama) · [MLflow](https://mlflow.org/docs/latest/genai/eval-monitor/) · [Giskard](https://docs.giskard.ai/hub/sdk/guides/evaluations) · [CrucibleBench](https://zenodo.org/records/21386663) · [JADE](https://icml.cc/virtual/2026/poster/63884) · [Petri 2.0](https://alignment.anthropic.com/2026/petri-v2/)

**Benchmarks** [BFCL](https://github.com/ShishirPatil/gorilla) · [τ²-bench](https://github.com/sierra-research/tau2-bench) · [ToolSandbox](https://github.com/apple/ToolSandbox) · [MCP-Atlas](https://labs.scale.com/leaderboard/mcp_atlas) · [SWE-bench](https://github.com/SWE-bench/SWE-bench) · [Terminal-Bench](https://github.com/harbor-framework/terminal-bench) · [Claw-Eval](https://github.com/claw-eval/claw-eval)

**Judge** [MT-Bench](https://arxiv.org/abs/2306.05685) · [PoLL](https://arxiv.org/abs/2404.18796) · [self-preference](https://arxiv.org/abs/2404.13076) · [null](https://arxiv.org/abs/2601.22548) · [panels](https://arxiv.org/abs/2605.29800) · [central tendency](https://arxiv.org/abs/2605.16386) · [JudgeBench](https://arxiv.org/abs/2410.12784) · [PRePair](https://arxiv.org/abs/2406.12319) · [GLIDER](https://arxiv.org/abs/2412.14140)

**Scenarios/rubrics** [PersonaHub](https://arxiv.org/abs/2406.20094) · [QDC](https://arxiv.org/abs/2412.02980) · [Vendi](https://arxiv.org/abs/2210.02410) · [RaR](https://arxiv.org/abs/2507.17746) · [Silent Judge](https://arxiv.org/abs/2509.26072) · [OpenAI agent evals](https://developers.openai.com/blog/eval-skills.md)

**Loop/stats** [field guide](https://hamel.dev/blog/posts/field-guide/) · [OpenAI best practices](https://developers.openai.com/api/docs/guides/evaluation-best-practices) · [Miller](https://arxiv.org/abs/2411.00640) · [EleutherAI](https://arxiv.org/abs/2405.14782) · [infra noise](https://www.anthropic.com/engineering/infrastructure-noise) · [NIST AI 800-3](https://nvlpubs.nist.gov/nistpubs/ai/NIST.AI.800-3.pdf)

*Legend: ✅ verified against a fetched primary page · ⚠️ secondary/partial. All recommended items are ✅.*
