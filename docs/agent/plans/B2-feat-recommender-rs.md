# B2 `feat/recommender-rs` —— 推荐器移植到 Rust（R5）

> 工作站 B · 2026-10-01；源码基线 `origin/main@ffed37a`，B1 参考 `origin/feat/rust-parity@4641ae3:docs/agent/plans/B1-rust-parity.md`。
> 依据：本机 `AGENTS.md`、[COLLAB](../COLLAB.md)、[交接的里程碑分工](../HANDOFF-2026-10-01.md)、[总体规划 §7](../../iDoris-总体规划.md#7-路线图m1m3-已完成新增-m4m8)。本次只提交计划，不实现代码。

## 0. 执行约定

- 集成分支 `feat/recommender-rs` 从最新 `origin/main` 建立；规划 worktree 为 `~/Dev/iDoris/wt-plan-B2`。
- 每个 task 一个独立 worktree，分支 `feat/recommender-rs-NN-<短名>`，PR 目标为 `feat/recommender-rs`。依赖尚未合并时，从直接依赖分支建立堆叠分支，PR 暂指依赖分支；依赖合并后检查并改回功能分支，不只依赖平台自动改指向。多依赖先等共同基线齐备，不私自合入其他功能分支。
- 每个 task PR **新增＋删除总计 ≤300 行，包含测试、fixture、文档**；表内是目标预算。超限先拆 task 并更新计划，不压缩代码或挪走测试规避限制。共享文件（manifest、lock、模块导出、policy role）串行修改。
- 主会话规划、审阅、验收；可派最多 3 个 `gpt-6-luna` 子 agent，职责按文件隔离。每条委派写明文件、实现、命令、禁止修改范围；主会话 `wait_agent` 后逐一审阅。用户本次要求优先于总体规划里的旧 Sonnet 分工。
- task 合并条件：Codex Tier 1 / prdaemon 批准＋CI 全绿，使用 merge commit；新增 FU 仅用 200–299。切片完成后 release PR `feat/recommender-rs` → `main`。**本次计划直接提交并推功能分支，不开 PR。**
- B2 交付 `probe`、`memory`、catalog 解析与 `recommend` 的可注入纯逻辑及消费接口。B1 负责 HTTP、逐 provider runtime、真实模型绑定及 Supervisor 接线；A 负责实机校准。B2 不加载模型、不写 sysctl、不下载权重、不连接真实 oMLX。
- 所有实现任务执行完整门禁：`cargo fmt --check`；`cargo clippy --all-targets --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks -- -D warnings`；`cargo test --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks`；`cargo deny check advisories bans licenses sources`；`pnpm lint && pnpm typecheck && pnpm check:contract-drift && pnpm build && pnpm test && pnpm conformance`。改变路由行为再跑 `bash scripts/conformance-rust.sh`。Rust 禁 unsafe、生产代码禁 unwrap/expect；只用 pnpm。

## 1. TS 参考实现与 Rust 现状的差异

以下 `file:line` 均相对仓库根，定位上述固定源码基线。TS 注释与实现冲突时先记差异，以实际执行结果为移植对照，不借移植改变策略。

| 范围 | TS 行为及证据 | Rust 现状及证据 / 计划 |
|---|---|---|
| HostFacts | `packages/recommender/src/probe.ts:16` 定义 ram/chip/gpu/os/source；`:64` 只对 RAM 做有限正数校验，其余默认 unknown/null；`:78` inspectHost 是 I/O 边界 | `crates/idoris-recommender/src/lib.rs:10` 是空 probe 模块；补类型、解析和注入采样接口，不在算法里探测本机 |
| 字节与探测文本 | `probe.ts:40,48,56,90`（此行及下表短路径均位于 `packages/recommender/src/`）：RAM 除 2^30 后 Math.round；Apple 芯片提取、英文 profiler 核数提取、合并只改 GPU | Rust 无实现；RAM 标称 GB 与权重十进制 GB 不可混用；未知文本回 null/unknown |
| 权重 / KV / footprint | `memory.ts:42,79,98,116`：量化表；实测 weights_gb 优先；KV=2×层×KV heads×head_dim×ctx×元素字节；MoE 用总参数，默认开销 1GB | `crates/idoris-recommender/src/lib.rs:17` 空 memory；`packages/recommender/tests/memory.test.ts:23` 已有 4.5045GB、9MiB/64token 等对照 |
| Apple 预算 | `memory.ts:127,138,143,148`：reserve=clamp(RAM×.30,3,16)，usable=min(RAM×pct,RAM−reserve)，pct=.66/.70/.75，sysctl=round(usable×1024) | Rust 无公式；建议值只返回数据，不执行系统修改；TS 即使 os=linux 也用同一公式，B2 先保留 |
| catalog 字段 | `recommend.ts:95,116,150,208`：有限数字、非空字符串、非空量化、bpp/weights 至少其一、必填 roles、重复角色/模型拒绝；`:133` capability 接受任意字符串键 | Rust 无 Catalog 类型或解析器；不能直接套只接收 TaskProfile 能力枚举的 map，现有目录含 instruct/agent/zh 等 |
| 宽松字段与文件加载 | `recommend.ts:102,191,219,228`：可选数字 null→缺省；部分可选字符串忽略错误类型；excluded 非数组忽略；loadCatalog=读 UTF-8＋YAML＋parseCatalog；未知字段丢弃 | `crates/idoris-recommender/Cargo.toml:14` 仅 contracts 依赖；复用仓库 YAML 依赖方案（`crates/idoris-router/Cargo.toml:36`），依赖变更单独 PR。`config/catalog.yaml:44,46` 的 context/scenarios 未被 TS parser 保留，不擅自启用 |
| 角色与 load_hint | `recommend.ts:164,180,336`：七个 catalog role，不含 auto；experiment 排除、RAM 过滤可选、顺序稳定；`config/catalog.yaml:20` 明确 load_hint 只是声明 | `crates/idoris-policy/src/role.rs:15,177,204` 已有 Role 和基于 Card 的筛选（含 NaN 防护），但无 catalog 读取。复用 Role 和共享筛选条件，不重写角色解析、不用 load_hint 替代 roles |
| 量化选择 | `recommend.ts:342,367,379,389`：达质量阈值且能装下→优先质量、再小 footprint、完全平手保留目录顺序；最低 footprint 忽略质量；标签取首个匹配 | Rust 无实现；保留边界 <= 及稳定平手顺序，不按 ID 重排 |
| 自动常驻与 BLOCKED | `recommend.ts:245,398,415,437`：临时槽每个留 3.5GB，另留 1GB headroom；daily 以 reasoning×quality 最大，平手选更省内存；所有低于 min_ram 的条目先进 BLOCKED（包括 experiment） | 空 recommend 模块 `crates/idoris-recommender/src/lib.rs:23`；24GB 基线为 ornith-1.0-9b@q6_k、footprint≈10.5075GB、常驻预算11.34GB（`packages/recommender/tests/recommend.test.ts:63`） |
| 强制 override | `recommend.ts:401,456`：默认 process.env；trim 后拆 id@quant；未知 id 回自动并警告，未知 quant 自动量化；强制可绕过 RAM、角色、experiment、质量及预算 | Rust 无实现；用显式 options 注入 override，调用层才读取环境。保留 yields/warnings，以及强制模型仍可能同时出现在 blocked 列表的实际行为 |
| 临时 admission | `recommend.ts:487`：先选能力值最高模型再选量化；不回退第二模型；每能力用同一个 usable−resident；不累计扣临时内存，不按 temp_slots 截断，不跨能力去重；独占仍装不下也标 requires_eviction | Rust 无实现；这是推荐展示，不是实际可加载保证。Supervisor 必须独立准入，不能让推荐状态替代内存账本 |
| 可读输出 | `recommend.ts:535,566`：返回完整硬件/策略、预算分项、resident_label、temp/blocked、warnings、tradeoff、sysctl、override；中文文本保留顺序，金额样式两位、score 四位 | Rust 无输出类型；`packages/recommender/tests/tradeoff.test.ts:16,46` 仅部分断言，需新增完整 TS 快照与 JS 舍入边界向量 |
| B1 task 15 模型与内存 | `packages/router/src/roles.ts:76` 有 catalog 顺序候选、installed 及可选 RAM 过滤；文件 `:8` 明示尚未接 dispatch。catalog id 没有 backend model 绑定字段 | `crates/idoris-router/src/dispatch.rs:40,284,351,365` 仍固定1GB、chosen provider id 当 model；`:42` 注释说明全角色/Ready 简化。B2 给候选与估计，B1 消除这些接线占位 |
| B1 task 33 容量 | `packages/router/src/capabilities.ts:103,113,125,137,153` 每次重算推荐、映射三类条目，并读 backend.status 汇总 loaded 数 | `crates/idoris-router/src/lib.rs:211` 无 capabilities 路由，B1负责接入；B2 输出未舍入的 Recommendation＋Catalog。TS `CapabilityEntry` 实际 **7 字段**（`:26`），B1 计划“8字段”应在接线时纠正，不能自行造第8字段 |

补充兼容边界：TS catalog 的数值校验是“有限”而非“必须正数/整数”，version 也未锁死为1；scenarios 虽在接口声明中却没被 parser 带出。modality 非数组会抛原生 TypeError。移植需区分“接受/拒绝与有效结果一致”和“照抄 JS 异常类”，建议见 §3。不得把解析成功等同于可用于安全加载。

## 2. 按依赖排序的 task 与接口交付

文件缩写：`C/`=`crates/idoris-recommender/`，`S/`=`C/src/`，`T/`=`C/tests/`，`TS/`=`packages/recommender/tests/`，`F/`=`testdata/recommender/`（新建，仅放跨语言对照）。每行列出的文件是该 task 的修改边界；`S/lib.rs` 只在相关任务串行补导出。

统一命令 **R**=`cargo test -p idoris-recommender`；**T**=`pnpm --filter @idoris/recommender test`；**P**=`cargo test -p idoris-policy`。T 前先 `pnpm --filter @idoris/contracts build`；所有 task 另跑 §0 全部门禁。fixture 从固定 TS 实现及测试输入生成并评审，运行测试只比较、不自动更新期望；Rust 测试不需要 Node、网络或真实硬件。

| task 分支 | 依赖 / 修改文件 / 目标行数 | 验收（含测试命令与负对照） |
|---|---|---|
| `feat/recommender-rs-01-deps` | 无；根 `Cargo.toml`、`Cargo.lock`、`C/Cargo.toml`；≤100 | 加 workspace recommender 入口及 serde/serde_json、现有 YAML 库、policy 依赖，保持 policy 不反向依赖 recommender；`cargo metadata --no-deps`、R、cargo deny；负控：缺依赖的最小 consumer 编译失败，不靠 dev-dependency 掩盖生产缺边 |
| `feat/recommender-rs-02-facts-vectors` | 无；`F/probe.json`、`F/memory.json`、`TS/parity-vectors.test.ts`；≤240 | 从现有 probe/memory 测试提取数字和文本，T 读取共享向量；覆盖 GiB/GB/MiB、三档预算、权重优先、MoE；改错1个 expected 必红。非 JSON 数值 NaN/Infinity 用命名输入标签恢复，不能串成普通数字 |
| `feat/recommender-rs-03-reference-ram` | 02；`F/ram.json`、`F/README.md`、`TS/parity-ram.test.ts`；≤260 | T 对真实 catalog 的8/16/24/32/64/128GB注入输入逐字段比对；24GB存完整输出，其余用明确字段投影，记录 TS/catalog commit 和哈希；负控：把24GB量化改为q8、预算改1GB即红。fixture超限先按RAM档拆task并更新编号后拆PR，不能靠省略失败断言过关 |
| `feat/recommender-rs-04-reference-edges` | 03；`F/edges.json`、`TS/parity-edges.test.ts`；≤260 | T 生成并锁定最小catalog平手、空候选、RAM/质量/预算等号、override正常/非法/过大、独占仍过大的临时项；正控＋负控只改变一个输入。完整固定 warnings/tradeoff 样本；不复制整个目录 |
| `feat/recommender-rs-05-host-facts` | 01、02；`S/probe.rs`、`S/lib.rs`、`T/probe.rs`；≤240 | R：make_host_facts、nominal_ram_gb、parse_chip、profiler解析/合并与向量一致；0/负值/NaN/Infinity拒绝，空chip/os默认；负控：10^9当RAM除数、merge改坏ram时测试必红 |
| `feat/recommender-rs-06-memory` | 01、02；`S/memory.rs`、`S/lib.rs`、`T/memory.rs`；≤260 | R：量化表、三种单位换算、weights/kv/footprint及默认值；4.5045GB、9MiB/64 token、q8为fp16一半；weights与bpp同时存在用weights；缺两者报错。用active替total、漏KV×2、GB改GiB均必红 |
| `feat/recommender-rs-07-apple-budget` | 06；`S/memory.rs`、`T/budget.rs`；≤180 | R：reserve上下夹取、三档wired、usable min、sysctl建议；24GB→15.84与16220、64GB→42.24；边界与JS round向量；负控：reserve不夹取/删min必红 |
| `feat/recommender-rs-08-catalog-types` | 01；`S/catalog/mod.rs`、`S/catalog/types.rs`、`S/lib.rs`、`T/catalog_types.rs`；≤250 | R：Catalog/Model/Quant/Excluded/带path错误；复用policy Role并排除Auto，capability保留非枚举键，保留声明顺序。类型序列化正控；roles缺失不能默认空、optional null与missing对照，未知字段不凭空进入输出 |
| `feat/recommender-rs-09-catalog-parse` | 08；`S/catalog/parse.rs`、`S/catalog/mod.rs`、`T/catalog_parse.rs`；≤280 | R：移植parseCatalog与字段校验，roles七值/空数组、重复id/role、空quant、缺bpp和weights、非有限数、坏arch/modality；错误定位path。最小单缺min_ram样本必须含roles，避免TS现有负例先被缺roles挡住；删对应校验必红 |
| `feat/recommender-rs-10-catalog-file` | 09；`S/catalog/io.rs`、`S/catalog/mod.rs`、`T/catalog_file.rs`；≤220 | R：显式路径读UTF-8/YAML后调用同一parser，真实`config/catalog.yaml`及excluded解析；临时目录正控；缺文件/坏UTF-8/坏YAML/重复键及YAML类型差异对照；不得悄悄回落仓库默认目录，验证从不同cwd结果一致 |
| `feat/recommender-rs-11-role-candidates` | 05、09；`S/roles.rs`、`S/lib.rs`、`crates/idoris-policy/src/role.rs`、`T/roles.rs`；≤260 | R＋P：抽出最小共享角色元数据筛选，既有Card入口委托且保留NaN防护，catalog候选也委托；对照TS router roles样本，installed=None与空集不同、RAM可选、experiment排除、顺序不变；auto拒绝。负控：load_hint=on_demand且roles=[daily]仍可候选，不能擅自新增过滤 |
| `feat/recommender-rs-12-quant-pick` | 06、09；`S/recommend/mod.rs`、`S/recommend/types.rs`、`S/recommend/quant.rs`、`S/lib.rs`、`T/quant.rs`；≤280 | R：Policy/PartialPolicy、QuantPick及pick/lowest/label；默认32768/q8/.98，平手质量→footprint→输入顺序；等号可入。负控：低质量虽小也不入常规pick，lowest不受质量影响，未知标签无匹配 |
| `feat/recommender-rs-13-resident-blocked` | 07、11、12；`S/recommend/resident.rs`、`S/recommend/types.rs`、`S/recommend/mod.rs`、`T/resident.rs`；≤260 | R：预算分项、全目录BLOCKED、daily自动常驻、reasoning×quality排名；注入24GB选q6，RAM=门槛可入；experiment可BLOCKED但不能自动常驻；空目录/负常驻预算→None。去临时预留、改评分或改平手顺序必红 |
| `feat/recommender-rs-14-core-override` | 13、04；`S/recommend/forced.rs`、`S/recommend/mod.rs`、`T/override.rs`；≤250 | R：显式override输入等价TS env，trim与split行为（空/多个@），未知模型/量化回退、强制超限及experiment允许但警告；保留原blocked项；负控：无override与强制结果不同、删警告必红；测试不改全局env |
| `feat/recommender-rs-15-temp-admission` | 14；`S/recommend/temp.rs`、`S/recommend/mod.rs`、`T/temp.rs`；≤240 | R：先选能力最强、排除resident/experiment/低RAM；ready与requires_eviction，保留输入能力顺序和重复项；负控：不能换成更弱但能装下的模型、不能逐项扣预算、不能截到temp_slots；无能力不输出伪候选 |
| `feat/recommender-rs-16-recommend-output` | 15、10；`S/recommend/diagnostics.rs`、`S/recommend/mod.rs`、`S/recommend/types.rs`、`S/lib.rs`、`T/output.rs`；≤280 | R：公开recommend/recommend_from_file，完整Recommendation、中文warnings/tradeoff、resident_label/sysctl/override；toFixed(2/4)与JS数字呈现边界对照，非空不足以验收；负控：改顺序/漏预算或override警告必红 |
| `feat/recommender-rs-17-ts-parity` | 03、04、16；`T/parity.rs`、`T/parity_support/mod.rs`；≤260 | R＋T：Rust消费共享固定JSON；全量24GB＋各RAM投影＋边界场景，逐字段定位差异。数值用绝对/相对1e-9，ID/状态/顺序/空值/文本精确相等，预算判定不加epsilon；改expected状态或×2公式必红，测试不能自己重建期望 |
| `feat/recommender-rs-18-model-estimate-api` | 11、12、17；`S/model.rs`、`S/lib.rs`、`T/model_api.rs`；≤240 | R：按catalog_id/quant/ctx/kv输出分项估计和目录元数据、按角色列候选；未知id/量化/无候选区分，供加载的ctx及估计有限且为正。两模型/两量化估值确实不同；负控：给provider id冒充catalog id必须失败、错误ctx不能固定估计1GB。该加载边界的严格校验不改变纯recommend兼容行为 |
| `feat/recommender-rs-19-capacity-consumer` | 17；`T/capacity_consumer.rs`、`C/README.md`；≤230 | R：仅用公共接口构造B1消费样例，拿到resident/temp/blocked各自内存、ctx与catalog能力；24/32GB状态对照，强制模型同时resident+blocked不去重；负控：以weights替footprint必红。文档列七字段映射；不实现HTTP/queue读取、不把示例当生产Provider |
| `feat/recommender-rs-20-probe-boundary` | 05；`S/probe/source.rs`、`S/probe.rs`、`T/probe_source.rs`；≤220 | R：inspect_host接收HostProbe采样接口（总字节/CPU文本/平台），标source=probe、gpu=None；GPU增强显式传文本；FakeProbe测试失败/缺CPU/未知平台。负控：recommend输入facts后即使probe设为panic也不调用。真实OS采集器由调用层实现，B只测注入raw facts |
| `feat/recommender-rs-21-handoff` | 18、19、20；`C/README.md`、`C/Cargo.toml`（去骨架描述）、`docs/agent/plans/B2-feat-recommender-rs.md`；≤200 | R的公共接口示例＋全部§0门禁；记录TS基线哈希、允许差异、B1调用契约、A校准模板及实测状态；负控：不能把只跑mock写成实机通过、不能把默认catalog id当已安装模型。完成接口release后才声明B1依赖已解锁 |

**对照数据纪律。** 现有 TS 四个测试文件共50例是起点，不是覆盖上限。每份 fixture 记录来源测试名、输入、expected 与生成命令；task03/04 的测试文件提供显式维护更新模式，普通 T 只验证。固定 JSON 不含机器时间、绝对路径和真实探测结果。数值容差只用于比较输出，不改变选型边界；exact-tie/相差极小值另设样本，避免浮点容差掩盖选错模型。catalog变更时先审TS期望再同步Rust，禁止两端各造一份“看似相同”的数据。

**发版切片。** 01–11＋20 可交付 facts、公式、catalog、角色候选；12–17 交付完整对照推荐；18–19＋21 交付B1消费契约。小切片可先release，B1 task33须等完整Recommendation接口发布；B1 task15可先做好显式模型传递，角色选型与估计接线等待B2。其他功能分支只通过main同步；急需cherry-pick需在PR正文说明，不直接合并B1/A未发布分支。

### B1 task 15：模型选择与内存估计接口

建议公共边界（签名在对应task落定，不是本次实现）：`Catalog`、`HostFacts`、`RecommenderPolicy`；`role_candidates(catalog, role, installed_catalog_ids?, ram_gb?)` 返回目录顺序候选；`estimate_model(catalog, catalog_id, quant_label, ctx, kv_quant)` 返回 `ModelEstimate { catalog_id, quant_label, ctx, weights_gb, kv_gb, overhead_gb, footprint_gb }`，错误必须区分未找到、无量化与非法估计。不把具体后端绑定混进catalog parser。

B1 用显式绑定 `(provider_id, catalog_id, quant_label) → backend_model_id` 将选择转为实际后端模型名；installed列表也必须通过此绑定反查，不能用字符串包含关系猜测。`resident_label=id@quant` 仅作推荐标签，既不是下载地址，也不保证等于 oMLX `/v1/models` 返回的名字。`Role::Auto` 由B1调用recommend取resident；其他角色按候选接口与B1既定选择规则处理，不能把TS目录顺序强塞成policy注册卡排序。

B1把同一绑定所得的真实model送到load/chat/status，并把相同quant/ctx/KV下的footprint交给Supervisor；取消固定1GB与全角色Ready占位。未知绑定、无候选或缺估计不能回落provider ID/假1GB。纯推荐的override让路不绕过Supervisor的实际预算/加载失败处理。B2不修改 `router/src/dispatch.rs`、`lib.rs`、backend或upstream；B1实际接线另设≤300行task，完成MockAdapter验证后才算task15整体验收。

### B1 task 33：`/capabilities` 接口

B2 `recommend(HostFacts, Catalog, PolicyPatch, explicit override)` 输出完整 `Recommendation`，无网络/环境读取；catalog保留能力分数。B1构造provider时固定catalog与facts（可以注入），每次snapshot重算推荐并读取实时backend.status。映射为顶层数组的七字段：`id`、`capability`、`resident`、`estimated_memory_gb`、`ctx_limit`、`queue_depth`、`admission_status`。

- resident：reasoning、resident=true、footprint、resident.ctx、ready；temp：对应capability、quant.footprint、policy.context_target、ready/requires_eviction；blocked：最低量化估计、policy.context_target、BLOCKED→blocked。
- 内存两位小数在B1展示层舍入；blocked的能力取TS优先级 reasoning/coding/vision/asr/tts/embedding/rerank/chat 中最高分，平手保留优先级、无值回chat。不得把catalog原始context字段当输出ctx_limit。
- queue_depth为各backend已加载模型数之和；单backend失败不贡献并脱敏警告。动态后端调用、HTTP 503/record_id、权限和路由门禁属于B1；B2不用静态0冒充实时容量。
- B1消费验收：同一facts/catalog/policy快照与TS逐字段相等；只改FakeBackend loaded数，queue变化但模型估计不变；只改ctx，KV及可能的admission变化；一个backend失败其余仍贡献。公共HTTP conformance由B1 31–33扩充，B2库对照不能代替它。能力估计仍来自B2公式，backend实测内存是另一事实来源，不静默覆盖estimated_memory_gb。

## 3. 需要 jason 拍板的问题与建议默认值

以下是建议，尚未视为已批准；本次规划提交不等待答复。实现前若未推翻，按暂定默认推进并在相关task PR显式列出差异。

| 编号 / 问题 | 建议默认值 / 影响 |
|---|---|
| D-B2-1：严格复刻到什么程度？ | 保持有效输入的选型、顺序、输出和宽松catalog字段语义；不顺手修temp累计、load_hint或override。拒绝输入可用带path的Rust错误，不复刻JS TypeError类名/完整栈；文件I/O/YAML错误分类稳定即可。加载接口对非有限/非正ctx与内存fail closed，单独记录为安全边界，不能污染纯推荐对照 |
| D-B2-2：override与“硬门槛”的冲突 | 保留TS yields及警告，包括低RAM模型同时resident/BLOCKED；实际加载仍由Supervisor独立准入。若要求禁止override越界或临时独占超量改blocked，作为TS/Rust共同语义变更另立任务，不暗改B2 |
| D-B2-3：模型ID绑定放哪里、未知模型怎么办？ | B1在显式本地启动配置保存provider＋catalog＋quant→backend_model_id映射，B2只给catalog候选与估计；未知本地生命周期模型拒绝，不给1GB默认。远程/直连路径按B1原有契约，不要求都进入本地catalog |
| D-B2-4：probe移植与生产采集范围 | B2含HostFacts、文本解析、注入HostProbe版inspect_host；真实OS采集器放启动适配层，由B1/A提供，B只mock验证。核心函数不默认读process env、不自动fork profiler；生产启动不能使用测试facts假报本机。若要求B2同时交付各OS采集器，另拆依赖审计/平台task后再估范围 |
| D-B2-5：是否本次纠正FU-19与引入实测校准？ | 保留现有catalog、角色和默认daily推荐，完成TS parity后再以A2/A6数据PR校准。当前公式估计不宣称峰值上界，A未实测项标“未校准”；sysctl只建议，不自动设置 |

## 4. 必须在工作站 A 验证的内容

B的RAM矩阵、目录解析、角色/量化、输出与消费接口全部用注入facts完成；这能证明TS对齐，不能证明真实模型能装入内存。A（M1 Max 64GB）执行以下实机验证，并保留对应commit/catalog哈希、oMLX版本、模型实际ID/量化、ctx/KV、命令、原始读数与结果；模型权重和凭证不入git。

| A 验证 | 数据与正负对照 / 交付 |
|---|---|
| 真实探测适配 | raw总字节→64GB、CPU→M1 Max、实际profiler GPU核数与系统信息相符；缺GPU信息返回null，采样失败明确报错。记录真实采集器来源，不能把注入64GB当实测 |
| A2 内存校准 | fast 1–4B、daily 7–12B、deep分别冷/热加载，记录基线、权重、prefill/长生成峰值与卸载后占用；至少两个ctx及KV档对照。ctx加倍应增加KV，权重量化改变与理论分项可解释；区分十进制GB/MiB、文件体积/运行峰值。偏差逐项记录，不套“全部±5%通过” |
| B1 task15 真模型接线 | 在B1消费B2发布接口后，验证provider id≠catalog id≠实际模型名的load/chat/status一致；换量化/ctx估值跟随改变；错误映射/未安装模型拒绝。确认Supervisor不再以固定1GB记账 |
| 共存 / 驱逐 / override | 真正并存resident+temp、需要驱逐、加载失败、释放恢复及多模型内存压力；故意超预算请求不得绕过Supervisor，override警告不构成准入许可。需要大内存或7B以上真实加载的测试只在A执行 |
| B1 task33 真容量 | 真实load/unload后queue_depth随loaded数变化；模型估计、ctx、admission与固定facts推荐一致；断开一个backend仍保留其他贡献且日志无凭证。明确queue是TS的loaded数代理，不宣称是实际排队请求数 |

A2可立即采集，不必等B2写完；B1消费冒烟等接口发布及接线后再做。A以catalog/校准数据PR反馈，B完成业务修改；不直接push对方分支。涉及oMLX实测使用既有 `IDORIS_OMLX_IT=1` opt-in 流程，只在A连接真实实例。B2纯逻辑release可以先行，但实际默认模型、校准承诺及R6切换需附A实测结论，未测不能标已通过。

本次规划核验：已读指定规范与B1计划，已执行§0 Rust/TS全量门禁并通过；TS推荐器50例通过，TS conformance为47 passed / 7 todo。cargo-deny通过但有已有依赖警告，未在文档任务中升级依赖；未改变路由行为，未跑Rust HTTP conformance；未做A实机验证。本次提交仅此计划文件。
