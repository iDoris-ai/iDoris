# Voice V0 现状盘点 — 回复泰国 Starter Kit

> 对方 `starter-kit/PRODUCT-FORM-AND-ROADMAP.md` 的 **V0** 里程碑写着「**需读 iDoris 代码仓库**（不在本仓库）」，
> 是整条 Voice 线的硬阻塞，也是 Dev 入职第一周第一件事。这份是回复。
> 核查日期：2026-09-07 ｜ 核查范围：`~/Dev/auraai/*`、`~/Dev/tools/*`

> **定位（2026-09-07 jason 拍板）**：**AgentEar 是外部输入法组件，未来还包括独立的、可插入到任意 agent 的「说话器官」。**
> 所以「Voice 的基础在 AgentEar」这句要读准：**iDoris 与泰国业务都是它的消费者，不是拥有者**。三条含义：① 不能当私有实现去改，提需求不提 commit；② 正因为是外部组件，`NOTICE` 与再分发才是真义务而非形式；③ 它已有 TTS 选型 ADR（`0005-tts-selection.md`，状态「草稿，不可据此拍板」，卡在一条需要人耳判断的事情上），意味着这条线**将来不止是入口（听），还可能是出口（说）**。

## 0. 结论先行：假设成立，但你们找错了仓库

> **「Voice 已有基础」这个假设是真的 —— 但它不在 iDoris，也不在 Agent24，在 [`iDoris-ai/AgentEar`](https://github.com/iDoris-ai/AgentEar)（本地 `~/Dev/tools/AgentEar`）。**

- **iDoris 仓库**：24 个文件，**零代码**，只有规划文档 + 一份 U0 spike 日志。没有任何语音实现。
- **Agent24**：`agent24-worker` crate 只有 **Rust 侧的 wire 契约与 HTTP 客户端**（定义了 `POST /v1/transcribe`）。**Python 侧 ML worker 未实现**，`docs/specs/TASKS.md` 标 `D4b deferred（等消费者）`。
- **AgentEar**：**M1 完成、M2 完成**（v0.4.2），有发布版 `.app`，实测一次 7.4 秒录音**全程 7.8 秒**完成，空闲常驻内存 13 MB。

## 1. 逐条回答 V0 的四问

| V0 问 | 答 |
|:---|:---|
| **哪个 whisper 实现与模型尺寸** | **不是 whisper。** 主链路是 **SenseVoiceSmall q8 GGUF**（~234M 参数 / ~250MB），跑在 **FunASR llamacpp runtime**（`runtime-llamacpp-v0.1.9`，macos-arm64）+ `fsmn-vad.gguf` 做 VAD。经四模型实测横比选定（`docs/decisions/0001-asr-model-selection.md`），初版基于二手资料的选型结论已被实测推翻并作废。 |
| **泰语单独调过没** | **调过，而且是单独一条引擎。** SenseVoice **做不了泰语**——实测两次，它把泰语音频标成 `<\|en\|>` 并输出乱码；其语种标记集里**根本没有 `th`**。故泰语走**独立的 whisper.cpp 引擎 + 显式语言选择**（不是自动检测），决策见 `docs/decisions/0004-thai-asr-engine.md`，模型指纹在 ADR 里固定、由 app 按需下载。另有泰语语料归档与实测结论（commit `2013c51`）。 |
| **有没有可跑的 demo** | **有。** GitHub Releases 提供 `AgentEar-*-macos-arm64.zip`，拖进「应用程序」即用；按右 Command 录音、再按停止，几百毫秒后转写进剪贴板。另有 `agentear --diagnose` 环境自检、`--transcribe x.wav` 离线转写。 |
| **部署形态** | **macOS `.app`，Apple Silicon 限定（M1 及以上，macOS 11+）。** |

## 2. 三条会改变你们排期的事实

### 2.1 ⚠️ `faster-whisper 停更 9 个月` 这条风险**不适用**，但换了一条

你们 `verification-2026-09-06-voice-stack.md` 的结论是「faster-whisper 停更 9 个月而后端仍在发版」。**AgentEar 根本没用 faster-whisper**，用的是 FunASR/SenseVoice + llamacpp runtime。

**新风险只有一条**：主链路依赖 `modelscope/FunASR` 的 `runtime-llamacpp-*` 发布产物。对策不是只锁版本号 —— **要自留一份产物副本，版本号挡不住上游删 release**（这条对策由 iDoris-website 补充）。

> **🔧 更正（2026-09-07，由 iDoris-website 核出）**：本文早先版本还写了「SenseVoice-Small 官方标注即将停止维护」。**该说法不成立，已删除。**
> 我引的是 AgentEar `docs/asr-selection.md` §1，而那句已被同仓库的 `docs/decisions/0001-asr-model-selection.md` §3「更正一处早先的错误」撤销：来源是厂商博客的二手说法，核实后 GitHub `QwenAudio/SenseVoice` 未 archive、最后提交 2026-07-27、8951 stars，HF 下载量 5755 高于 Fun-ASR-Nano-GGUF 的 4813。ADR 原话是「这曾是排除 SenseVoice 的主要理由，它站不住」。
> **我的错误形态值得记**：那份文档顶部的横幅明说「选型结论已作废」，我读到了，但仍引用了横幅之下的一句具体事实——**「结论作废」不等于「其中每条事实都已被逐条更正」，也不等于「没被更正的就还有效」**。正确做法是顺着它指向的 ADR 读完再引。

### 2.2 🔴 交付形态受限（**范围已收窄**，见更正）

你们 `starter-kit/README.md` §5 写的例外是「数据敏感度高的客户，Voice 可以单独部署在他们机器上」。这句要分三层看：

| 平台 | 状态 |
|:---|:---|
| **Apple Silicon Mac** | ✅ 下载 `.app` 即用，AgentEar 的目标形态 |
| **Intel Mac** | ❌ **不成立**，且不是「暂时没做」——上游 FunASR 的 macOS 产物从 `v0.1.9` 到 `v0.2.6` 只有 `macos-arm64`。主程序编成通用二进制也没用，ASR 子进程起不来 |
| **Linux / Windows** | ⚠️ **路存在但要自己组装**（未实测）：上游有 `linux-x64`/`-avx2`/`-vulkan`/`linux-arm64`/`windows-x64`（含 CUDA）产物，但 AgentEar 的 `.app` 交付形态不覆盖它们 |

> **🔧 更正（2026-09-07，由 iDoris-website 逐个 tag 核出）**：本文早先版本写的是「Apple Silicon 限定，不支持 Windows/Linux」。**过度概括了。**
> 讽刺的是 AgentEar README 原文的括号里就写着「（Linux/Windows 才有 x64）」——**我读的是同一句话，却把它的限定词读丢了**。不成立的只有 **Intel Mac** 与「下载 `.app` 就能用」这个交付形态。

### 2.3 AgentEar 的理解层**已经在消费一个 OpenAI-compat 本地端点**

`src/correct.rs` / `src/label.rs` 默认连 `http://127.0.0.1:8793`，发 `POST /v1/chat/completions`，用的模型是 **Ornith**（提示词里专门处理了它默认吐 `Thinking Process:` 的行为）。

**这正是 iDoris 该顶上的位置。** 现在 README 要求用户「自己备一个本地 LLM」；接上 iDoris 之后，那一句可以变成「指向你的 iDoris URL」，并且顺带获得隐私路由、预算、审计。

## 3. 对你们的建议

1. **V0 判定为「已完成」**，答案在本文档 §1，不必再等 iDoris —— 你们的 V1（锁版本 + GPU 路径验证）可以直接开工，但对象是 AgentEar 的 `vendor/bin` 产物版本，不是 faster-whisper × ctranslate2 那一对。
2. **V2 泰语评测基线仍然要做 —— 但理由不是「没人测过」，而是「测的不是你们的场景」。**
   > **🔧 更正**：本文早先版本说「AgentEar 侧也还没有这个数」。**错了，他们有两批**：`docs/data/thai-cer-stats.txt`（FLEURS 朗读语料 n=80、六模型、自助法 4000 次 95% CI，最好 `ggml-medium-q8_0` CER **6.08%** [4.41%, 7.89%]）与 `docs/data/thai-corpus-arm-2026-09/RESULTS.md`（code-switch 22 条：纯泰 3.9%、夹英文 31.1%、加 initial prompt 降到 18.4%）。
   > **这条错误比事实本身更值得记：我断言了一个「不存在」，而我根本没去 `docs/data/` 看过。** 断言缺席的举证责任和断言存在一样重，但我当时没有对它施加同等要求。

   **真正的理由更强**：那两批是**朗读语料、安静、单人**，且 AgentEar 自己标注了 FLEURS 被 Thonburian 模型卡**声明为训练数据**。所以 6.08% 不能进销售话术，而你们客户的会议室/电话/噪声场景确实没人测过。
   **这让 V2 变便宜而不是变贵** —— 直接复用他们的 `cer-thai.py` / `cer-stats.py` / `reference.tsv` 格式 / `thai-recorder.html`。
3. **先确认客户机器架构**，再决定 §2.2 那个承诺怎么措辞。
4. **AgentEar 成果能否直接用 —— 许可这条有现成答案**，见 §4。

## 4. 许可：AgentEar 的成果可以用（回答你们新开的 [待核]）

| 项 | 许可 | 依据 |
|:---|:---|:---|
| AgentEar 本体 | **Apache-2.0** | 仓库 `LICENSE` |
| FunASR llamacpp runtime | **MIT** | `NOTICE`（版本 `runtime-llamacpp-v0.1.9`）|
| SenseVoiceSmall q8 GGUF | **Apache-2.0** | `NOTICE` |
| FSMN-VAD | **Apache-2.0** | `NOTICE` |
| **泰语 GGML（转换后权重）** | **MIT** | `docs/decisions/0004-thai-asr-engine.md`：上游 `biodatlab/distill-whisper-th-large-v3` 是 MIT，**再分发只需保留版权声明**；Release 说明已注明出处、revision 与许可 |

**「托管转换后 GGML 权重的再分发义务」这一条 AgentEar 已经核过并确认完毕**——`plan-i18n-thai.md` §4 原本要求「在确认再分发义务之前不要开始托管」，MIT 这一条确认完毕。

⚠️ **附带条件**：ADR 明写「如果日后换成 Apache-2.0 的 `medium`，NOTICE 义务要重新过一遍」（三个上游许可不同：`medium` = `biodatlab/whisper-th-medium-combined` Apache-2.0；`distill`、`turbo` 是 MIT）。

> **🔧 更正（2026-09-07，由 iDoris-website 指出）**：本文早先版本写「CER 最好的正是 `ggml-medium-q8_0`（6.08%）」并据此提醒「选了它要重核许可」。**那个提醒方向对，但前提本身是错的——我在读一个统计上读不出来的差。**
> `thai-cer-stats.txt:20` 的配对比较原文：`ggml-medium-q8_0 − ggml-distill-q5_0  Δ95%CI=[-0.0142, +0.0099]  **未检出差异**`。六个模型的 CI 大面积重叠，「6.08% 是最好的」是在给一组无法区分的结果排名次。
> **为一个测不出来的差异要付三样确定的成本**：多约 425 MB 内存、一次 NOTICE 重核、以及 FLEURS 污染对 `medium`/`distill` 是同向的（排名本来就不中立）。

**因此结论不写成「选 `medium` 时记得重核许可」，而写成**：

> **默认跟随 `distill` q5_0（MIT）；只有真实场景评测里 `medium` 拉开可检出的差距才谈换，而那时第一件事是重核 NOTICE，不是先上线。**

这样即使将来没人记得这份 findings，结论本身也挡了一道 —— 挂一条核查项的通常结局是「选完 `medium`，然后想起来要重核」。

> **门禁形状（学自 AgentEar，值得两边都抄）**：`plan-i18n-thai.md` §4 立的是「**在确认再分发义务之前不要开始托管**」，不是「记得核一下许可」。**把待核项挂在一个具体动作前面，而不是挂在待办列表里** —— 列表形状的待核清单不产生阻力，动作前置的门禁才产生阻力。
