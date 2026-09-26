# Local LLM-as-a-Judge for Local LLMs (2B–35B, Apple Silicon) — Evidence Review
*Compiled 2026-09-19. All claims fetched from primary sources unless marked SECONDARY. Two arXiv IDs initially surfaced by search (2410.06158, 2504.00013) were wrong papers and were discarded.*

## Headline verdict

**Same-size-class judging is viable, but only for the narrow job of *ranking* (pairwise/pointwise rubric) — not for absolute quality guarantees.** Fine-tuned specialists in the 1.7B–13B range reach human correlation comparable to much larger general judges (Prometheus-2-7B Pearson 0.666 vs GPT-4-1106 on Vicuna Bench; GLIDER-3B beats GPT-4o Pearson on FLASK; a 1.7B probe judge beats an 8B generative judge). **But** every bias documented in frontier judges reappears, often worse, in small judges (position bias up to 25 pp; central-tendency compression; 51.5–69.1% agreement). Self-preference is real but partially a confound. Multi-judge panels are the *least* reliable mitigation at small scale — nine frontier judges carried ~2 effective votes. Recommend: rubric + reference + swapped-order pairwise, 1–3 disjoint fine-tuned judges, calibrated against ~100–300 hand-labeled items, with reported judge–human agreement and n_eff.

## 1. Bias evidence (each with a concrete study)

| Finding | Study | Metric | Verified? |
|---|---|---|---|
| Position bias is severe; order alone flips verdicts | [MT-Bench 2306.05685](https://arxiv.org/abs/2306.05685) | Swap consistency: Claude-v1 23.8%, GPT-3.5 46.2%, GPT-4 65.0%; GPT-4 favors first in 30% | VERIFIED |
| Small judges (1.7B–7B) have judge-specific position bias | [govllm 2605.24737](https://arxiv.org/abs/2605.24737) | Agreement drops up to **25 pp** across original/reversed/permuted order | VERIFIED |
| Renaming assistants changes the bias | MT-Bench | Claude-v1 consistency 23.8% → 56.2% under "rename" prompt | VERIFIED |
| Verbosity/length bias: padding wins | MT-Bench | "Repetitive list" attack failure: GPT-4 8.7%, **GPT-3.5 91.3%**, Claude-v1 91.3% | VERIFIED |
| Length/complexity sensitivity + leniency | [Judging the Judges 2406.12624](https://arxiv.org/abs/2406.12624) | 13 judges; sensitivity to prompt complexity/length; leniency; scores differ up to 5 pts | VERIFIED |
| Self-preference exists; self-*recognition* drives it | [Panickssery 2404.13076](https://arxiv.org/abs/2404.13076) | Linear correlation between self-recognition accuracy and self-preference after fine-tuning | VERIFIED |
| Mechanism is familiarity/low perplexity, not identity | [Wataoka 2410.21819](https://arxiv.org/abs/2410.21819) | Judges over-rate low-perplexity text vs humans, self-generated or not | VERIFIED |
| Self-preference is **partly an artifact** | [Are LLM Evaluators Really Narcissists? 2601.22548](https://arxiv.org/abs/2601.22548) (ICML 2026) | Only **51%** of prior self-preference examples survive an evaluator-quality null (89.6% of probability mass) | VERIFIED |
| MT-Bench's own self-enhancement result was inconclusive | MT-Bench | GPT-4 +10%, Claude-v1 +25% self win-rate; GPT-3.5 none → "cannot determine" | VERIFIED |
| Sycophancy: judges cave to stated user views | [SycEval 2502.08177](https://arxiv.org/abs/2502.08177) | 58.19% sycophantic; regressive (wrong) 14.66%; persistence 78.5% | VERIFIED |
| Sycophancy on subjective, not objective, items | [Ranaldi 2311.09410](https://arxiv.org/abs/2311.09410) | Suggestibility on opinion/fact-contradicting queries; resists on math | VERIFIED |
| Score clustering: central-tendency / endpoint compression | [Central Tendency Bias 2605.16386](https://arxiv.org/abs/2605.16386) | Systematic compression to scale middle; over-predict low end, under-predict high end; **few-shot full-range exemplars do NOT fix it** | VERIFIED |
| Censored/bounded scales manufacture effects | [Censored Rating Scale 2608.27309](https://arxiv.org/abs/2608.27309) | A zero-preference construction reproduced 79–85% of a "significant" interaction from floor effects | VERIFIED |
| Low discriminative power on hard pairs | [JudgeBench 2410.12784](https://arxiv.org/abs/2410.12784) | GPT-4o "just slightly better than random guessing" on knowledge/reasoning/math/code pairs | VERIFIED |
| Format/distractor sensitivity | [Pairwise or Pointwise? 2504.14716](https://arxiv.org/abs/2504.14716) | Distractor features flip pairwise preferences in **35%** of cases | VERIFIED |

## 2. Agreement with humans: open-weight vs frontier

| Judge | Human/GPT-4 agreement | Metric | Verified? |
|---|---|---|---|
| GPT-4 (frontier reference) | 85% vs experts; human–human 81% | % agreement (no-tie) | VERIFIED |
| Prometheus-13B (2023) | 0.897 vs human; GPT-4 0.882; ChatGPT 0.392 | Pearson, 45 rubrics | VERIFIED |
| **Prometheus-2-7B** | 0.666 (Vicuna), 0.548 (MT-Bench), 0.617 (FLASK-GPT-4), **0.545 vs humans (FLASK)**, 0.882 (Feedback Bench); 8x7B 0.685/0.665 | Pearson | VERIFIED |
| **GLIDER-3B** | Higher Pearson than GPT-4o on FLASK; **91.3%** human agreement | Pearson + % agreement | VERIFIED |
| **JudgeLM-7/13/33B** | >90% agreement with teacher judge, "surpasses human-to-human" | % agreement | VERIFIED |
| CompassJudger-1 | First open all-in-one judge; JudgerBench released (no headline metric in abstract) | — | VERIFIED |
| Skywork-Reward-V2 (0.6B–8B) | SOTA on 7 RM benchmarks; **outperforms generative reward models**; resistant to stylistic bias | RewardBench-style accuracy | VERIFIED |
| Self-Taught Evaluators (Llama-3-70B) | RewardBench 75.4 → **88.3** (88.7 majority vote); beats GPT-4 judge; matches labeled-data RMs | RewardBench | VERIFIED |
| JudgeLRM (RL-trained) | 3B/4B **exceeds GPT-4**; 7B/8B/14B beat DeepSeek-R1 by >2% F1 | F1 | VERIFIED |
| 13-judge spread (open+closed) | Only the best/largest align reasonably; all far behind inter-human; **ranking signal still OK for smaller models** | Cohen's kappa / alignment | VERIFIED |

**Metric caveat (VERIFIED):** 2406.12624 explicitly warns that judges with high *percent agreement* can assign vastly different scores — always report kappa/Spearman alongside raw agreement.

## 3. Is same-size-class judging viable?

**Yes, for ranking — with fine-tuning, not prompting.**
- [PoLL 2404.18796](https://arxiv.org/abs/2404.18796): a panel of *smaller, disjoint-family* models **outperforms a single large judge**, with less intra-model bias, >7× cheaper.
- [Small LMs as Judges 2608.30005](https://arxiv.org/abs/2608.30005): a **Qwen3-1.7B probe judge** gives the best criterion-level agreement, beating both 8B generative and logprob judges; as a GRPO reward model it trains 0.232→**0.643** vs 0.594 for an 8B generative judge, at **10.7× less judge time**.
- GLIDER-3B and JudgeLRM-3B confirm capability is bought by *specialized training*, not parameter count.

**No / caveats.**
- [Nine Judges 2605.29800](https://arxiv.org/abs/2605.29800): 9 frontier judges, 7 families → **~2 effective votes** (Kish n_eff); panel accuracy 8–22 pp below independent-voting ideal; **best single judge matches or beats the full panel**; smarter aggregation closes ≤11%.
- 2406.12624: only the best/largest judges align with humans.
- 2601.22548: much apparent self-preference disappears once you control for evaluator quality/uncertainty — so *measuring* self-preference in your own pipeline requires its null model, not a raw score gap.

## 4–5. Mitigations, and pairwise vs absolute

| Mitigation | Effectiveness | Evidence | Verified? |
|---|---|---|---|
| Swap order / randomize position | **High, cheap; mandatory** | MT-Bench conservative swap (tie on inconsistency); FairEval Balanced Position Calibration | VERIFIED |
| Reference answer in prompt | **High for verifiable tasks** | MT-Bench math failure 14/20 → **3/20** | VERIFIED |
| Chain-of-thought / reasoning-first | **Moderate** | MT-Bench failure 14/20 → 6/20 (~50% cut), still misled by context; [PRePair 2406.12319](https://arxiv.org/abs/2406.12319) pointwise-reasoning-in-pairwise beats both pure modes; Self-Taught Evaluators trains reasoning traces | VERIFIED |
| Explicit rubric with per-score descriptions | **High** | Prometheus / Prometheus-2 gains require rubric + reference; pairwise rubric variant also supported | VERIFIED |
| Multi-judge ensemble / jury | **Weak–negative at scale; only helps if families are truly disjoint** | PoLL positive; Nine Judges strongly negative (correlated errors, best single ≥ panel) | VERIFIED |
| Calibrate on small human gold set | **High value per unit cost** | [FairEval 2305.17926](https://arxiv.org/abs/2305.17926) Human-in-the-Loop via balanced-position-diversity entropy; [TEE 2604.11581](https://arxiv.org/abs/2604.11581) small pilot recovers honest CIs, +7.9 pp human agreement | VERIFIED |
| Few-shot / anchoring to examples | **Mixed — helps consistency, not bias** | MT-Bench consistency 65% → 77.5% (4× cost); but 2605.16386: full-range few-shot did **not** remove central tendency | VERIFIED |
| Logprob-based scoring | **Not a reliable replacement** | 2608.30005: Yes/No logprob margins *lose* to probe judges; [GEM 2026 code eval](https://aclanthology.org/2026.gem-main.55/) finds discrimination–ranking dissociation — logprob ranks canonical vs mutated code better, explicit judges capture semantic correctness better. Complementary, not interchangeable. | VERIFIED |
| Absolute vs pairwise | **Split verdict** | 2504.14716: absolute more robust to distractors (9% vs 35% flips); 2406.12319: pairwise *amplifies* superficial-attribute bias. But MT-Bench: pairwise has the higher ceiling (97% GPT-4 self-consistency) while single-answer grading is more scalable and has a "stable internal rubric". Best practice: **pointwise reasoning, pairwise decision** (PRePair). | VERIFIED |
| Bradley-Terry / Elo aggregation | Standard for pairwise → ranking | [Chatbot Arena 2403.04132](https://arxiv.org/abs/2403.04132): pairwise votes + statistical ranking over 240K votes. BT's simplicity/robustness on small uneven datasets is argued in SLAM (ACL 2025) — | Arena: VERIFIED; SLAM: SECONDARY |

## 6. Calibration procedure for a local judge (concrete)

1. **Build a gold set:** 100–300 items sampled from *your* traffic, covering easy→hard and full score range (avoid range restriction — 2605.16386). Label with 2 humans; keep the human–human agreement ceiling as your target.
2. **Fix the instrument:** pin model revision + quantization, temperature, prompt, and seed; log all of them (2604.11581 shows unpinned design choices create 40–60% understated SEs).
3. **Measure agreement, not vibes:** report % agreement **and** Cohen's kappa / Spearman / Kendall vs the gold set; split by slice (task, length, difficulty) to expose position, verbosity, and central-tendency bias.
4. **Ablate the prompt:** zero-shot vs rubric vs rubric+reference vs few-shot; measure each on the gold set; keep only what helps.
5. **Run bias audits in your own pipeline:** swap-order consistency rate; length-correlation of scores; self-preference against the 2601.22548 evaluator-quality null.
6. **Pilot, then project:** use a small pilot to compute total evaluation error and pick the design change with the best precision-per-cost (2604.11581).
7. **Monitor drift:** rerun the gold set on every model/quant/prompt change; treat judge–human agreement as a monitored metric with a re-calibration trigger.

## 7. Very small models (2B–7B): when they fail catastrophically

- **Prompted (untrained) small models as generative judges fail outright.** Prometheus-2 shows Llama-2-Chat-7B at Pearson **0.036** on MT-Bench, and untrained models often cannot emit a parseable verdict at all (the paper had to loop until parseable).
- **Absolute Likert scoring collapses.** Central-tendency compression concentrates at the scale middle and is worst at clinically/task-critical extremes; few-shot does not fix it (2605.16386). Bounded scales also let floor effects fabricate "significant" findings (2608.27309).
- **Position bias is largest exactly where it matters** — up to 25 pp agreement swing on 1.7B–7B regulatory judges (2605.24737), which also documents three structural failure modes.
- **Hard reasoning pairs are near-chance even for frontier judges** (JudgeBench); a 2B judge should not be asked to verify math/code correctness without a reference or an execution check.
- **Failure is avoidable in one configuration:** a *fine-tuned pointwise rubric* judge — 1.7B probe judge beats an 8B generative judge at criterion level (2608.30005), and ≥90% teacher agreement is attainable at 7B (JudgeLM). **Rule of thumb: below 7B, never use free-form generative absolute scoring; use a fine-tuned head on per-criterion yes/no with a reference.**

## Source URLs

arXiv abstracts/results fetched: [2306.05685](https://arxiv.org/abs/2306.05685) (MT-Bench) · [2305.17926](https://arxiv.org/abs/2305.17926) (FairEval) · [2310.08491](https://arxiv.org/abs/2310.08491) (Prometheus) · [2310.17631](https://arxiv.org/abs/2310.17631) (JudgeLM) · [2403.04132](https://arxiv.org/abs/2403.04132) (Chatbot Arena) · [2403.13787](https://arxiv.org/abs/2403.13787) (RewardBench) · [2404.13076](https://arxiv.org/abs/2404.13076) · [2404.18796](https://arxiv.org/abs/2404.18796) (PoLL) · [2405.01535](https://arxiv.org/abs/2405.01535) (Prometheus 2) · [2406.12319](https://arxiv.org/abs/2406.12319) (PRePair) · [2406.12624](https://arxiv.org/abs/2406.12624) · [2408.02666](https://arxiv.org/abs/2408.02666) · [2410.12784](https://arxiv.org/abs/2410.12784) (JudgeBench) · [2410.16256](https://arxiv.org/abs/2410.16256) (CompassJudger-1) · [2410.21819](https://arxiv.org/abs/2410.21819) · [2411.15594](https://arxiv.org/abs/2411.15594) · [2411.16594](https://arxiv.org/abs/2411.16594) · [2412.14140](https://arxiv.org/abs/2412.14140) (GLIDER) · [2502.08177](https://arxiv.org/abs/2502.08177) (SycEval) · [2504.00050](https://arxiv.org/abs/2504.00050) (JudgeLRM) · [2504.14716](https://arxiv.org/abs/2504.14716) · [2507.01352](https://arxiv.org/abs/2507.01352) (Skywork-Reward-V2) · [2601.22548](https://arxiv.org/abs/2601.22548) · [2604.11581](https://arxiv.org/abs/2604.11581) (TEE) · [2605.16386](https://arxiv.org/abs/2605.16386) · [2605.24737](https://arxiv.org/abs/2605.24737) (govllm) · [2605.29800](https://arxiv.org/abs/2605.29800) (Nine Judges) · [2608.27309](https://arxiv.org/abs/2608.27309) · [2608.30005](https://arxiv.org/abs/2608.30005) (Small LMs as Judges) · [GEM 2026 logprob code eval](https://aclanthology.org/2026.gem-main.55/)
