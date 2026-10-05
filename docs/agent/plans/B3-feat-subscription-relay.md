# B3 `feat/subscription-relay` —— Rust 订阅中转计划

> 2026-10-01，工作站 B；基线 `origin/main@ffed37a107a2e152963caea845450c9515d12044`。只写计划，未实现订阅中转。
> 依据：本机 AGENTS.md、[COLLAB](../COLLAB.md)、[交接 B3](../HANDOFF-2026-10-01.md)、[总体规划 §7](../../iDoris-总体规划.md)；执行格式参考 `origin/feat/rust-parity:docs/agent/plans/B1-rust-parity.md`（`4641ae3`）。

## 0. 执行约定

- 集成分支 `feat/subscription-relay` 从上述 `origin/main` 创建；工作区 `~/Dev/iDoris/wt-plan-B3`。每个 task 独立 worktree + `feat/subscription-relay-NN-<短名>`，PR 目标 `feat/subscription-relay`，不是 `main`。
- 未合并依赖采用 B1 的堆叠方式：从依赖 task 分支创建，PR 暂指向依赖分支；依赖合并后检查自动改指向是否成功，必要时手动改回功能分支。多个依赖必须都落入祖先或先合入功能分支，不能跨栈暗取代码。
- 每个 task PR **新增+删除总计 ≤300 行，包含测试、fixture、锁文件和文档**；表中预算是上限目标，超量继续拆分并更新本计划，不能省负对照凑行数。共享文件按依赖串行修改；子 agent 一律 `gpt-6-luna`，负责人审阅、修正、验收后提交。
- 新 FU 只用 200–299。依赖改动独立 task（01）；`RuntimeAdapter`/`BackendError` 属双机共享接口，默认不改。遵守禁 unsafe、生产禁 unwrap/expect、只用 pnpm、评审不用 Copilot。
- task 合并条件：prdaemon 批准 + CI 全绿，以 merge commit 合并；功能完成后另开 release PR 到 main。本次仅提交并直接推功能分支上的计划，不开 PR、不推 main 或 A 的分支。
- B1、kernel-hardening 的成果等 release 进入 main 后再同步；不直接合并未发布功能分支。当前 K04 只在 `origin/feat/kernel-hardening-K04-reject-subscription@8a276c7`，**不在本基线，也不在已核对的 集成头 `origin/feat/kernel-hardening@a719b67`**。24 替换其门禁前须确认 K04 已发布；若仍未发布，先协调 K04 发布，不把解除门禁和遗漏安全补丁混在一起。
- 验收目标：personal + 显式启用 + 合格沙箱 + 真实 loopback peer 才执行一次 CLI；其他组合零 spawn。默认关闭和 kill switch 不影响能力②/③；local_only 永不进入订阅；超时/断连/超限后整组回收；错误、日志无请求和 CLI 输出原文。

完整门禁（每个实现 task 本地先跑；本计划也运行基线门禁）：

```sh
cargo fmt --check
cargo clippy --all-targets --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks -- -D warnings
cargo test --features idoris-tenancy/test-bins,idoris-tenancy/mutation-test-hooks
cargo deny check advisories bans licenses sources
pnpm lint && pnpm typecheck && pnpm check:contract-drift && pnpm build && pnpm test && pnpm conformance
```

改变路由行为另跑 `bash scripts/conformance-rust.sh`。不能用跳过测试、删除断言、增加 todo 或真实 CLI 不可用时返回成功来换绿灯。新用例必须在撤掉对应防护的局部变异下变红，记录命令与结果，恢复变异后重跑；变异不提交。

## 1. TS 参考实现与 Rust 现状的差异

下表所有 `file:line` 相对仓库根，除显式 K04 外均指 `ffed37a`；`30d43c3..ffed37a` 只新增两份交接文档，业务代码未变。行号锚定此快照，实施时先同步并重定位。

| 能力 | TS 证据与实际行为 | Rust 证据与缺口／移植要求 |
|---|---|---|
| 注册与默认禁用 | `packages/adapters/subscription/registration.ts:50`；`packages/router/src/egress-guard.ts:92`：非 personal 优先 refuse；personal 下 disable=1 优先于 enable；未 enable 跳过；启用且无合法 profile 拒绝 | `crates/idoris-router/src/components.rs:145` 只有读卡/常规校验；`crates/idoris-router/src/profile.rs:119` 已把空模式归 personal，其他非 personal 归 tenant，但未接订阅注册。必须统一配置快照传至 loader/factory/request gate |
| K04 过渡门禁 | TS 支持安全条件下注册，非永久总拒绝 | **K04 ref `8a276c7`** 的 `crates/idoris-router/src/components.rs:73,190` 新增 `SubscriptionUnsupported`，按 ID 无条件拒；`crates/idoris-router/tests/subscription_startup.rs:1` 验证多种 form/env 不能绕过。B3 最后用完整门禁矩阵替换，保留变体防绕过测试 |
| factory 与执行路径 | `packages/adapters/src/factory.ts:17` 按固定 ID `subscription` 构造并二次检查；`packages/router/src/server.ts:344` 的 spawn_cli 独立调用 backend，不走 HTTP proxy | `crates/idoris-upstream/src/lib.rs:21` 仅 chat/error/omlx/remote；`crates/idoris-router/src/lib.rs:504,521` 只有 resident HTTP 与 Supervisor 路径；`crates/idoris-router/src/bin/idoris.rs:54` 组装 oMLX。新增独立无状态 relay handle，不能把 CLI 硬套成 oMLX load/unload |
| personal-only／单用户 | `packages/adapters/subscription/registration.ts:25,52`：tenant/community/city/未知值拒；`packages/router/src/server.ts:308` 用 socket peer。没有 OS 用户鉴权 | `crates/idoris-router/src/lib.rs:145,196` 已存 deploy_mode；`crates/idoris-router/src/bin/idoris.rs:119,133` 固定 loopback，未传 ConnectInfo。需真实 peer 与缺失 peer fail-closed，忽略 X-Forwarded-For/Forwarded；身份能力归 B5 |
| 来源与 Tailscale | `packages/router/src/egress-guard.ts:35,45,61` 支持 loopback/mapped IPv6，可选 100.64/10；IPv4 正则未校验全部 octet；`packages/router/src/server.ts:51,99` 实际仅绑定 127.0.0.1 | 无订阅来源门禁。用标准 IpAddr 而非宽松字符串匹配；TS CIDR 不是用户身份白名单，也未证明远端入口可达。D-B3-2 决定是否纳入 Tailscale |
| 隐私／实际 locality | `packages/router/src/registry.ts:77` 拒 relay 声称 local/local_only；`packages/router/src/server.ts:324` 订阅响应标 remote | `crates/idoris-policy/src/privacy.rs:17,34` 已识别固定 ID、所有 SpawnCli 并标 Remote；`crates/idoris-policy/src/registry.rs:159,177` 已拒矛盾卡。保留并复测；本地 spawn 不是本地推理，local_only 零 CLI 调用 |
| CLI 参数与 prompt | `packages/adapters/subscription/relay.ts:144`：claude -p 禁工具/设置/MCP/会话；codex exec read-only/ephemeral/忽略配置规则，prompt 走 stdin；`:180` 多消息带 role | Rust 未实现。固定 CLI 枚举+argv，不用 shell、不把卡 endpoint 当命令模板。TS flags 是参考，不能当作所有已安装版本的能力证明；特别是 read-only 本身不能证明 no-tools |
| 沙箱保证及历史要求差异 | `packages/adapters/subscription/sandbox.ts:9,15,62` 显式 profile，无裸跑回退；仅 CLI flags + 0555 cwd，明确不限制网络目的地、HOME 凭据或 cwd 外写入。`docs/agent/tasks.md:189,193` 却要求无任意工具/子进程和网络/凭据等限制 | Rust 没有沙箱实现。这是**现有保证与历史验收文字的差异**，不能声称完整安全追平；D-B3-1 必须决定边界。不能证明 no-tools 的 CLI 禁止注册，不能默默去掉不支持的安全参数 |
| 环境与错误保密 | `packages/adapters/subscription/relay.ts:115,132` 删除已知凭据及全部 IDORIS_*；`:51,77` 仅白名单诊断。`packages/router/src/server.ts:358,370` 返回 502 + reason_code | Rust 无 relay 错误层。保留退出码、字节数、可选 digest，不透传 spawn/OS/CLI 自由文本。环境过滤不等于 HOME 凭据隔离；还应清掉 B 机实际使用的 IDORIS 凭据 |
| 进程组／超时／输出 | `packages/adapters/subscription/relay.ts:31,339,367,388,434`：120s、5s grace、256KiB stdout+stderr，detached PGID，TERM→KILL；`:330` 吞 signal 错误；`:293` Codex output file 无限额读取 | Rust workspace `Cargo.toml:24` 未开 Tokio process/signal；`crates/idoris-backend/src/adapter.rs:149` 仅有整组取消约定，无实现。需安全 API、父进程 wait/reap、孙进程观测、输出文件独立上限和生命周期任务，不照搬吞错与无界读取 |
| 取消端到端 | `packages/router/src/server.ts:329,339,349` 已修为 res.close→AbortSignal→relay；`packages/adapters/tests/subscription.test.ts:261` 测超时父孙进程；现有 router 取消测试是 HTTP 后端 | `crates/idoris-router/src/lib.rs:510,528` 注明 token 尚未接客户端断连；`crates/idoris-router/src/dispatch.rs:210,345` 只保证 future drop→cancel。须证明完整 body 发完、响应前断开仍杀 CLI，不能只靠手动 drop 单测 |
| 返回／流式／用量 | `packages/adapters/subscription/relay.ts:186,190,287` 非流式、字符/4估算；`packages/router/src/server.ts:345,350` 再包装时 prompt 不含 role。即使 stream=true 仍回整块 JSON | `crates/idoris-router/src/lib.rs:320,333` 已有估算和 OpenAI 包装，可复用 HTTP 层口径；不得声称真实 token usage。D-B3-4 明确 stream、错误、重试语义，不创建伪 SSE |
| models／核心独立性 | `packages/adapters/subscription/relay.ts:256,260` 返回 cli-subscription 模型名，无实际模型加载；`packages/router/tests/subscription-gate.test.ts:67` 关闭订阅不影响普通组件；`config/components/subscription.yaml:4,17,21` remote/any/spawn_cli | `crates/idoris-router/src/models.rs:71` 仅聚合现有 HTTP 列表，测试 `:161` 跳过 SpawnCli。新增静态模型发现，不运行 CLI；disable 时模型/health 注册数同步移除；能力②/③继续可用 |

实现边界：`crates/idoris-upstream/src/subscription/` 管 CLI、沙箱、进程与稳定错误；`crates/idoris-router/src/subscription/` 管配置、来源、handle 与 HTTP 接线。复用 idoris_backend 的 ChatRequest/ChatResponse 和 CancellationToken，但订阅按 remote 无状态请求执行，不占 Supervisor 模型显存、不伪造本地 loaded 状态。仍经过现有隐私、策略、准入选择；B1 的规则/逐卡 runtime/审计接线发布后复用，接口未发布则在本模块局部实现，不改造第二套通用 runtime。

`allowed_egress: [loopback]` 在现有订阅卡里表达入口限制，**不证明 CLI 只向 loopback 出网**。订阅使用已有个人登录态，能力② API key 不传子进程；不新增 OAuth、组织订阅共享、身份系统、计费统一或本地模型加载功能。默认策略仍只选 local。

## 2. 按依赖排序的 task

共 **28 个 task**。路径缩写：`U/`=`crates/idoris-upstream/`，`R/`=`crates/idoris-router/`，`P/`=`crates/idoris-policy/`，`C/`=`conformance/`；`US/`=`U/src/subscription/`，`RS/`=`R/src/subscription/`。未存在的文件是规划路径。表内“对应测试”均计入预算；同名模块由后续依赖 task 顺序扩充，不并行改。

以下各行验收为必须项，Rust 自动测试以 `cargo test -p idoris-upstream subscription` / `cargo test -p idoris-router subscription` 运行（测试名含 subscription）；fixtures 在 07 统一建设后复用。24 之前生产入口保持 K04 总拒绝，允许的测试注入仅存在测试构建中；不得先发布一个能裸跑的中间版本。

| task 分支 | 文件、依赖、行数预算 | 验收标准（测试与负对照） |
|---|---|---|
| `feat/subscription-relay-01-deps` | 根 Cargo.toml/Cargo.lock、U/Cargo.toml、R/Cargo.toml；无依赖；≤220 | 按最小功能启用 Tokio process/io-util/signal，安全 Unix 信号 API（建议 nix signal/process）、tempfile、摘要依赖按需引入；编译+deny 通过。创建独立PGID及整组发信号均须有安全API的编译/运行探针，禁直接 unsafe libc/pre_exec；非 Unix 编译有明确 unsupported 分支，不回退单 PID kill |
| `feat/subscription-relay-02-gate-policy` | RS/config.rs、RS/mod.rs、R/src/lib.rs；无依赖；≤250 | 纯函数 register/skip/refuse 和 immutable 配置快照；空模式 personal，其他非 personal 即使 disable 也拒；personal disable 胜 enable；缺/错 profile、错 CLI 拒。表驱动真值矩阵；删 mode 判断必红，零 spawn |
| `feat/subscription-relay-03-card-boundary` | RS/card.rs、R/tests/subscription_card.rs；依赖02；≤240 | 身份组合只接受固定 subscription + SpawnCli + spawn://subscription；订阅 ID 换 HTTP、其他 ID 伪装 SpawnCli、local/local_only、未知/非零价格拒绝，普通 HTTP 正控可用；保持 duplicate 检查覆盖被跳过卡。暂不解除 K04 |
| `feat/subscription-relay-04-cli-profile` | US/profile.rs、US/command.rs、US/mod.rs、U/src/lib.rs；依赖01；≤270 | typed profile、CLI 枚举、固定 argv、Codex stdin、多消息 role 编码；精确测试安全 flag、前导 '-' prompt 不被解释为选项。CLI 安全能力/版本检查方案按 D1 落定；不支持 flags/no-tools 则固定错误，不剥 flag 重试；只读不等于无工具 |
| `feat/subscription-relay-05-workspace` | US/workspace.rs、U/tests/subscription_workspace.rs；依赖01、04；≤250 | 每请求自有空临时 cwd 0555、control dir 0700、结果文件0600；拒绝外部 cwd 注入。确认进程组回收后清理并恢复自有权限；普通直接写探针拒绝，改回可写即红（同UID可chmod，故不是隔离证明）；临时目录失败零 spawn，不 chmod 用户仓库 |
| `feat/subscription-relay-06-diagnostics` | US/environment.rs、US/error.rs、对应单测；依赖04；≤240 | 沿TS复制环境再删除已知 API/AWS 凭据及所有 IDORIS_*，保留HOME/PATH/其他值；这是黑名单过滤，不称凭据白名单隔离；reason_code 固定枚举。stderr secret/prompt sentinel 不在 Display/Debug/HTTP 可用信息中；移除过滤/拼原文必红 |
| `feat/subscription-relay-07-fake-cli` | U/tests/support/subscription_cli.rs、R/tests/support/subscription_cli.rs（薄复用）、测试 fixture 目录；依赖01；≤240 | 固定脚本/测试二进制模式：echo、stderr、非零、空输出、hang、忽略 TERM、孙进程、持管道、output-file、PID/PGID 标记。fixture 自测确实生成父孙进程并可清理；不能只靠无进程就通过的 ps 断言；不需订阅账号 |
| `feat/subscription-relay-08-spawn-io` | US/process.rs、U/tests/subscription_spawn.rs；依赖04–07；≤270 | 固定 command+args（无 shell），独立 process_group(0) 或等效安全 API；并行写 stdin/读双管，合计256KiB限额、byte而非字符、分块UTF-8正确；spawn失败/EPIPE固定错误。stdin阻塞+大stderr无死锁；只计算stdout或串行读管道的变异必红 |
| `feat/subscription-relay-09-group-reaper` | US/reaper.rs、U/tests/subscription_reaper.rs；依赖07、08；≤280 | 唯一生命周期 owner：TERM整组→grace→KILL→wait直属child；父退出即清组，不等孙进程关闭继承管道；有界排空。ESRCH可幂等，EPERM/其他失败记固定清理错误。只kill父、漏wait、父先退孙持管道都被测试抓到；无脱组恶意进程的虚假保证 |
| `feat/subscription-relay-10-timeout-cancel` | US/process.rs、US/reaper.rs、U/tests/subscription_cancel.rs；依赖09；≤270 | 默认120s/5s并断言常量；测试注入短时钟。pre-cancel零spawn；执行中cancel/timeout/超限合流一次终结，调用future drop仍由owner回收。TERM被忽略时KILL收尾；未取消正常长请求成功为负控，取消A不杀B |
| `feat/subscription-relay-11-output-file` | US/output.rs、U/tests/subscription_output.rs；依赖05、08–10；≤250 | Codex仅接受本请求私有control路径下普通结果文件；安全API以no-follow打开，对同一文件句柄fstat并有界读最多256KiB+1，拒symlink/目录及路径替换竞态，不先检查再重新按路径打开；非空file优先、缺/空file回stdout（TS口径）；非零exit无论file内容仍失败。这是Rust安全收紧，不是TS已有保证。并发请求不串文件，超大file/替换路径必拒；说明此为读取内存限额非磁盘配额；不允许假称该限额阻止CLI写满磁盘 |
| `feat/subscription-relay-12-relay-api` | US/relay.rs、US/mod.rs、U/tests/subscription_relay.rs；依赖06、10、11；≤260 | 无状态 chat/list API，返回 cli-subscription 与正确 model/content；错误覆盖spawn/empty/exit/timeout/cancel/limit，正常退出也无PGID残留。成功/错误/取消均在回收确认后清理自有临时目录；回收未确认按下文异常规则处理；移除exit检查、输出空白当成功时必红 |
| `feat/subscription-relay-13-source-guard` | RS/source.rs、R/tests/subscription_source.rs；依赖02；≤230 | 真实 SocketAddr，127/8、::1、mapped IPv4 正控，缺peer/LAN/公网拒403；转发头不能提权；非法octet拒。默认拒Tailscale，D2改变时扩相应精确名单用例。测试可注入peer但生产只能连接层提供 |
| `feat/subscription-relay-14-shutdown` | US/service.rs、U/tests/subscription_shutdown.rs；依赖12；≤250 | 服务持有活动owner，优雅shutdown拒新spawn、cancel全部、有界await回收后退出；重复shutdown/finish竞态只有一次收尾；多请求父孙进程均消失。移除drain后进程观测必红；不能承诺宿主SIGKILL/掉电仍清理 |
| `feat/subscription-relay-15-runtime-handle` | RS/runtime.rs、R/src/lib.rs、R/src/bin/idoris.rs；依赖02、03、12、14；≤250 | typed授权状态才能构造relay，factory二次验证；逐provider handle绑定，不落oMLX Supervisor或HTTP client；生产订阅入口仍拒绝。disabled配置不建目录、不探CLI、不读登录态；错handle固定错误；真实spawn计数证明未误调 |
| `feat/subscription-relay-16-dispatch` | RS/dispatch.rs、R/src/lib.rs、R/src/dispatch.rs；依赖13、15；≤260 | 复用策略/隐私后的selected结果，先模式/来源再spawn；订阅remote、local_only零调用，普通local HTTP正控成功；默认local链不选订阅。订阅失败不自动重试/不缓存结果，不以返回错误后的隐式fallback重复消费；生产仍锁住 |
| `feat/subscription-relay-17-http-lifetime` | RS/connection.rs、R/src/bin/idoris.rs、R/src/lib.rs；依赖16；≤280 | 将真实连接关闭/服务shutdown映射到每请求token并传relay，传ConnectInfo；先用最小真实TCP探针验证所选Axum/Hyper连接服务生命周期，必要时安全连接层包装。不能假设handler必drop，也不能body读完就取消；keep-alive下一请求不继承已取消token |
| `feat/subscription-relay-18-disconnect-test` | R/tests/subscription_disconnect.rs；依赖07、17；≤250 | 真HTTP完整发完body、等fake CLI握手后关闭socket，grace+观测容差内父孙PGID消失且临时目录删除；保持连接长请求成功。另测并发A取消B继续、响应前取消、重复断连；断开token接线的变异必红，不能以手工abort handler代替 |
| `feat/subscription-relay-19-http-result` | RS/response.rs、R/tests/subscription_response.rs、R/src/lib.rs；依赖16、18；≤250 | success非流式OpenAI JSON，模型正确、usage沿HTTP原估算口径含中文/emoji；502 subscription_relay_failed + RELAY_*，403来源拒绝；实际执行后成功/后端错都remote+record-id，前置拒绝不冒充served；stdout/stderr sentinel只允许成功正文，不出错误或日志；B1审计若已发布则复用其成功/错误/取消终结钩子、同record只落一次，不自造审计库；D4默认stream=true仍整块JSON |
| `feat/subscription-relay-20-discovery` | R/src/models.rs、RS/runtime.rs、R/tests/subscription_discovery.rs；依赖15、19；≤230 | models静态枚举唯一CLI模型、不spawn；health组件数反映注册结果，disabled无订阅条目。B1 capabilities接口若已发布则接既有provider枚举，不伪造本地resident/内存；未发布时记录后续接线，不声明已实现。撤掉skip过滤必红 |
| `feat/subscription-relay-21-startup-matrix` | R/tests/subscription_startup.rs（接续K04）、测试配置；依赖03、15、20；≤260 | 真实二进制测试组织模式、未知mode、缺沙箱、伪装form、default-off/disable、enable+profile矩阵；在测试构建验证成功路径、生产保持K04拒绝。启用与禁用同设仍无CLI，去掉组织优先或factory复核必红；24转生产验收 |
| `feat/subscription-relay-22-security-acceptance` | R/tests/subscription_security.rs、U/tests/subscription_sandbox.rs；依赖18–21；≤270 | 已知秘密环境/配置sentinel不泄漏；cwd不可写、安全argv必带、默认/禁用/非personal/local_only零spawn；普通免费HTTP模型仍200。探针正控确认确实能看见child spawn/连接；不把fake CLI测试解释为真实CLI工具隔离证明 |
| `feat/subscription-relay-23-conformance-fixtures` | C/src/harness.ts、C/src/fixtures.ts、C/src/subscription-fixture.ts；依赖07、21；≤240 | 受控临时PATH放名为codex/claude的假CLI，不给生产增加任意命令覆盖变量；隔离用户IDORIS环境、只加本case允许项；spawn marker/PGID可观测。fixture正控执行一次并清理；指错被测二进制/假CLI时测试必须失败 |
| `feat/subscription-relay-24-enable-gates` | R/src/components.rs、RS/runtime.rs、R/src/bin/idoris.rs、R/tests/subscription_startup.rs；依赖01–23、K04已发布、D1/D2/D3/D4结论已记录；≤280 | 原子替换K04总拒绝为02/03/13完整门禁、开放生产授权构造、接shutdown；全部真实二进制用例改用默认production build（非dev-mock），默认skip与组织硬拒成立。回退总拒绝会使启用正控红；去门禁会使零spawn负控红；不能只删除K04测试 |
| `feat/subscription-relay-25-conformance-cases` | C/tests/subscription.test.ts、C/README.md；依赖24；≤260 | TS共同用例锁定default-off、disable、组织拒绝、enable无沙箱、loopback成功/remote头、local_only零spawn、非流式形状、HTTP断连；TS先正控，旧Rust失败，实施栈Rust通过。Rust更严form/输出file/平台限制单独cargo测，不逼TS新增M4功能；恢复对应gate缺陷时公共断言必红，不增加todo |
| `feat/subscription-relay-26-ci-platforms` | .github/workflows/ci.yml、scripts/conformance-rust.sh、scripts/check-subscription.sh；依赖25；≤220 | Linux与macOS必跑fake CLI进程组/取消/沙箱，Rust与TS共同conformance必跑；B1 CI改动若已发布则只补矩阵，不重复搭建。Unix保护不可在Linux跳过；故意删测试调用/错argv的验证必红；无账号/真实外网依赖 |
| `feat/subscription-relay-27-real-cli-smoke` | U/tests/subscription_real.rs、docs/agent/acceptance.md；依赖24–26；≤250 | B显式opt-in逐个codex/claude：固定回复、工具诱导、只读workspace、正常/超时/断连后无组残留；记录CLI版本/安全flags与通过项，不记录凭据或自由文本。不启用显式ignored；启用却缺CLI/登录/安全能力或测试失败必须失败，不能像TS probe catch后SKIPPED冒充验收 |
| `feat/subscription-relay-28-release-evidence` | docs/agent/plans/B3-feat-subscription-relay.md、docs/agent/acceptance.md、config/components/subscription.yaml注释；依赖26、27；≤200 | 逐task证据、允许差异、D项结论、完整门禁与负控记录齐全；文档用例可复制，说明kill switch重启流程、CLI版本不兼容时拒绝。确认无孤儿/临时产物、无实现未测却打勾，功能完成才另开release PR；A联合验证需要时附§4结果 |

并行窗口：01/02 可并行；04–06 与03/13可按文件独立推进；进程执行链08–12必须顺序；接线15–25按依赖。24先用既有Rust安全矩阵验证生产开放，25再扩共同conformance；每个task独立全绿。实现量若超预算，尤其09、17，必须先拆模块/测试task，不能靠“另一个PR补取消”提前开放。

生命周期验收以本测试独占 PID/PGID 和握手标记为依据，启动正控后才断言消失；使用有界轮询而非固定长sleep。回收直属子进程并确认同组成员退出；进程组不能约束主动 setsid 脱组的恶意CLI，不能写成任意进程树安全隔离。工具/脱组行为是否允许由沙箱能力验收决定，失败则该CLI不开放。 若整组终止或child reap未确认，owner保留未清理状态和受限临时目录，不报告成功清理、不先删仍可能被使用的目录；shutdown有界等待后固定错误退出并记录待人工回收元数据，不能无限挂起或吞掉EPERM。

## 3. 需要 jason 拍板的问题与建议默认值

以下是规划建议，**不是已批准结论**；本次不等待决策即可提交计划。实现24解除总拒绝前须记录D1–D4结论并具备所定义的保证；未批准或验证未满足均保持拒绝。27实测按批准的版本和保证验收。

| 编号 | 需要拍板 | 建议默认值与影响 |
|---|---|---|
| D-B3-1 | 是否接受TS现有沙箱边界，与T1.4.1历史强隔离验收有差异；怎样才算Codex/Claude可用 | 先交付显式opt-in、personal、remote、best-effort能力，绝不进local_only可信本地路径；明确不保证网络目的地、HOME凭据隔离、任意恶意CLI隔离。**无模型工具/工作区写入须对固定CLI版本和参数实测验收**（0555只限制普通写操作，同UID进程可改权限，不能作为OS隔离证明），read-only参数不能代替no-tools证明；无法证明的CLI保持禁用。历史还要求只允许固定CLI及必要运行时子进程、输入目录读取边界、网络目的地、凭据范围和输出大小限制；本计划的结果文件读取上限不提供磁盘配额，也没有文件系统读取隔离。若要求这些强隔离保证，B3需增加OS沙箱专项计划，不能把当前28项算完整T1.4.1安全验收 |
| D-B3-2 | 是否移植Tailscale例外；“单用户”边界是什么 | 首版只接受真实loopback peer，保留loopback监听；若设置ALLOW_TAILSCALE=1则明确unsupported，不静默放开100.64/10。个人单用户机器是部署前提，不声称能鉴别本机其它用户；B5再提供身份。若要Tailscale，另加可信入口与精确peer名单/身份验收task，单凭CGNAT CIDR不够 |
| D-B3-3 | kill switch是启动禁用，还是运行中立即停止所有订阅 | 首版沿TS：启动时快照，DISABLE=1胜ENABLE=1；改父shell环境不会改变已运行进程。操作为设置禁用后优雅重启，shutdown取消并回收活动CLI，新进程不注册。若需热切换，另拆受保护控制面与active-owner取消task，不伪称环境变量已实现即时按钮 |
| D-B3-4 | 非流式、错误与自动重试是否保持TS行为 | 首版不做SSE；stream=true沿TS返回整块JSON，明确文档。来源拒403，CLI失败502+固定RELAY_*，local_only保持现有策略错误；超时120s/grace5s、pipe256KiB，结果文件另限256KiB。每请求最多一次CLI，无透明重试/订阅结果缓存；usage保持估算，不当实测账单。若要拒stream=true或改504，登记允许差异并改共同用例策略 |
| D-B3-5 | 默认CLI、支持平台、真实CLI验收与升级策略 | 保留TS默认claude，Codex显式选择；首版支持macOS/Linux，其他平台enable即拒。27记录通过的精确CLI版本及安全配置，新版本安全flags不兼容fail-closed；未实际通过的CLI不宣传可用。B可测Codex，Claude缺账号可明确“未验收”，不能将双CLI完整交付标完成 |

## 4. 工作站 A 必须做哪些验证

**B3本身没有必须在A上做的验证。** 交接B3明确“不需要A”；B完成fake CLI、真实个人Codex/Claude、macOS进程组/取消验证；Linux交CI。CLI安全能力不依赖64GB内存，不可用“等A加载模型”代替订阅验收。

若本次release同时宣称与真实本地模型联合工作的结果，以下部分必须在A（M1 Max 64GB、真实oMLX/模型）验收，B使用fake HTTP的通过不能代替；它们是联合release证据，不是订阅模块单测前置：

| A联合验证 | 操作与负对照 | 交付证据 |
|---|---|---|
| 关闭能力①后真实能力③仍工作 | 加载真实daily 7B+模型，通过本地策略成功chat；设DISABLE=1并重启，重复同一请求成功，订阅spawn数0；显式仅订阅策略无候选而不是暗走本地 | CLI/模型/oMLX版本、状态码、locality、进程组观测；不留正文/凭据 |
| 本地与订阅并存、取消隔离 | 真实本地长生成同时执行订阅，断开订阅请求，只有对应CLI组退出，本地生成不被取消；反向取消本地不误杀订阅；未取消组为正控 | 两路独立请求ID和生命周期、Supervisor loaded状态，无错误unload或显存占用归到订阅 |
| 内存压力与超时隔离（只有改共享runtime时为必跑） | 在真实多模型/压力场景执行上述流程；local_only无CLI，订阅不进入模型显存准入/驱逐，不继承oMLX load的30s超时 | A实测报告，异常以PR提回B功能分支；不直接push A分支 |

不在B加载大模型；A现有opt-in oMLX测试使用 `IDORIS_OMLX_IT=1 cargo test -p idoris-upstream --test omlx_integration`，联合场景另附可复现配置与HTTP命令。A数据未取得时，只能声明B3独立验收通过，不能声明联合模型/内存验证完成。

本次计划校验记录（2026-10-01）：Rust fmt、带指定features的clippy/test、cargo-deny四项以及全部TS门禁通过；TS conformance为47 passed / 7 todo。cargo-deny有依赖警告但退出0；未改锁文件。仅新增本计划，未改路由行为，因此未追加Rust HTTP conformance；未运行真实订阅或oMLX测试。负责人复核三位gpt-6-luna的证据/任务草稿，并修正任务顺序、符号链接竞态、异常回收和沙箱边界。

## 5. 2026-10-05 release evidence（task28）

### 5.1 task / PR 状态

| task | PR | 状态 | 关键证据 |
|---|---:|---|---|
| 01 deps | #254 | merged | 安全 Unix process-group API / non-Unix fail-closed |
| 02 gate policy | #259 | merged | personal-only、default-off、disable 胜 enable |
| 03 card boundary | #260 | merged | 固定 subscription + SpawnCli + spawn://subscription |
| 04 CLI profile | #264 | merged | 固定 Claude/Codex argv、stdin prompt、安全 flag |
| 05 workspace | #271 | merged | request-private 0555 cwd + 0700 control + 0600 result |
| 06 diagnostics | #274 | merged | API/AWS + IDORIS_* scrub；错误不带自由文本 |
| 07 fake CLI | #272 | merged | PID/PGID、hang、TERM-ignore、孙进程、output-file fixture |
| 08 spawn I/O | #281 | merged | stdout+stderr 共用 256KiB 上限；变异测试承重 |
| 09 group reaper | #284 | merged | TERM→grace→KILL→wait；future drop 回收 |
| 10 timeout/cancel | #290 | merged | 120s/5s defaults；pre-cancel/timeout/limit/并发隔离 |
| 11 output file | #292 | merged | O_NOFOLLOW + same-handle metadata/read；竞态/超限拒绝 |
| 12 relay API | #293 | merged | stateless relay；固定错误；清理后才删 workspace |
| 13 source guard | #282 | merged | 真实 loopback peer；无 forwarded-header 提权 |
| 14 shutdown | #295 | merged | stop-accepting + cancel-all + bounded drain |
| 15 runtime handle | #301 | merged | typed authorization；重复 provider 不替换旧 handle |
| 16 dispatch | #302 | merged | policy/privacy/source 后单次 relay；无隐式 fallback/retry |
| 17 HTTP lifetime | #305 | merged | 真 TCP 断连→request token；keep-alive request token 隔离 |
| 18 disconnect acceptance | #306 | merged | 真 HTTP 断连回收父孙 PGID + workspace；生产 wiring 变异必红 |
| 19 HTTP result | #307 | merged | 200/502/403 shape；UTF-16 usage；错误无 stderr/prompt |
| 20 discovery | #308 | merged | 静态 subscription model；不 spawn、不伪造 local capacity |
| 21 startup matrix | #309 | merged | deploy/profile/enable/disable/unknown matrix |
| 22 security acceptance | #310 | merged | env/cwd/argv/local_only/普通 HTTP 正控 |
| 23 conformance fixture | #311 | merged | 临时 PATH fake CLI；不增加生产 command override |
| 24 production gates | #312 | merged | 原子替换 K04 总拒绝；授权构造 + shutdown 接线 |
| 25 shared conformance | #313 | merged | Rust release 二进制全量 conformance：134 passed / 8 todo |
| 26 Unix CI matrix | #314 | **merged** | repaired head `615ccab` 已纳入 #315/#317；Linux+macOS matrix、Rust/gates 全绿后 merge |
| 27 real CLI smoke | #316 | **approved; repaired/hardened head `dd801c0` pushed** | Codex 0.156.1 + Claude Code 2.1.289 本机真实 PASS；#318 已合入 task27 分支，收紧 flag token 与失败日志 |
| 28 release evidence | 本 task | in progress | 本节 + acceptance + component 注释 |

**B3 不能在 #316 repaired/hardened head `dd801c0` CI 全绿并实际 merge 前声明 complete，也不能开 release PR 到 main。**

### 5.2 现行安全/行为结论

- **实现语言**：Rust 是唯一后续维护实现；TS 仅作 PoC / 参考契约与共同 conformance，不再新增产品能力。
- **启用条件**：仅 personal 模式、显式 enable、固定 sandbox profile、固定 CLI；disable kill switch 优先。
- **来源**：首版只接受真实 loopback peer；不信任 Forwarded/X-Forwarded-*；不开放 Tailscale/CGNAT。
- **隐私**：subscription 永远是 remote；local_only 在 spawn 前拒绝，CLI 调用数必须为 0。
- **进程**：每请求独立 PGID；取消/超时/输出超限均 TERM→grace→KILL；不承诺约束主动 setsid/setpgid 脱组的恶意后代。
- **文件系统**：request cwd 仅是合作式 0555 边界；Codex output file 使用 no-follow + same-handle fstat/read；这**不是**完整同 UID OS 沙箱，也不是磁盘配额。
- **凭据**：移除已知 API/AWS env 以及全部 IDORIS_*；保留 HOME/PATH 以使用 CLI 自己的登录态。不能声称 HOME 凭据隔离或网络目的地隔离。
- **协议**：subscription 目前非流式；stream=true 仍按已记录兼容策略返回整块 JSON；每请求最多执行一次 CLI，无透明 retry/cache。
- **usage**：仅估算 token，不能当真实供应商账单。
- **平台**：本轮生产支持/验收目标是 macOS + Linux；Windows 不在 B3 放开范围。

### 5.3 D-B3 结论（按已实现/已验收行为记录）

| 决策 | release 结论 |
|---|---|
| D-B3-1 沙箱边界 | 接受当前显式 best-effort 边界：Claude no-tools + restricted；Codex read-only；不宣称网络/HOME/恶意同UID完全隔离。真实 CLI smoke 必须通过，否则该 CLI 不宣传可用。 |
| D-B3-2 Tailscale | 首版拒绝；只允许 loopback。未来若开放必须另做可信入口/身份与精确 peer allowlist。 |
| D-B3-3 kill switch | startup snapshot；`IDORIS_DISABLE_SUBSCRIPTION=1` 胜 enable。不是热切换：操作上必须设置 disable 后优雅重启。 |
| D-B3-4 stream/error/retry | 非流式整块 JSON；source 403；relay 502 + 固定 RELAY_*；120s timeout / 5s grace；无透明 retry/cache。 |
| D-B3-5 CLI/platform | 默认 Claude，Codex 显式可选；macOS/Linux。2026-10-05 实测 Codex 0.156.1、Claude Code 2.1.289 通过固定安全 profile。 |

### 5.4 kill switch / 回滚流程

1. 设置 `IDORIS_DISABLE_SUBSCRIPTION=1`（即使 enable 仍存在，disable 优先）。
2. **优雅重启 iDoris**；运行中环境变量变化不会热更新当前快照。
3. shutdown 阶段停止接受新的 subscription 请求，取消 active relay，并在有界等待内回收 owned PGID。
4. 新进程启动后 subscription card 不注册；`/v1/models` 不再出现 subscription model。
5. 用 local/free HTTP 正控确认能力②/③仍正常；local_only 请求确认 0 CLI spawn。
6. 若清理未确认，保留受限 workspace / 固定 CleanupFailed 证据，不提前宣称清理成功；人工检查残留 PGID 后再重启。

### 5.5 CLI 升级策略

- 升级 Codex/Claude 后先运行 task27 opt-in smoke；required flags/version profile 不满足即 fail-closed。
- 禁止因新版本不支持安全 flag 就自动剥 flag 重试。
- 只有通过 fixed reply、write-inducement 下禁写文件未出现、timeout、显式 cancel、PGID cleanup 后，才更新本节记录的支持版本；这些证据不能扩写成“证明模型一定尝试了写入”或“证明 cancel 发生在确认生成中”。
- 真实 smoke 的失败断言也不得记录 prompt、stderr、凭据或模型自由文本。

### 5.6 已知未关闭项 / release-preview 证据

- #315 已合并：`killpg EPERM` 不再立即覆盖请求终止原因，而是交给有界存在性检查继续确认。
- #317 已合并：`ignore-term` fixture 先安装 TERM trap 再发布 ready marker，测试只认 `ESRCH` 为“组真的消失”；该修复已同步到 task26 repaired head。
- #314 repaired head `615ccab` 已包含 #315/#317；Linux+macOS subscription matrix、Rust/gates 全绿后已 merge，task26 正式闭环。
- #318 已合入 task27 分支：CLI help 检查从子串匹配收紧为 token-exact，并阻止 fixed-reply 失败时回显真实模型自由文本。task27 repaired/hardened head 为 `dd801c0`，最终仍需 CI 全绿并 merge。
- #314 同一次 run 的 B1 `load_fence::ownership_is_exclusive_even_after_marker_clear` 失败是独立既有 flake；不得与 B3 macOS blocker 混为一谈。
- #305 review 记录：Linux 对 pipelined-bytes + close 的 POLLHUP/POLLIN 行为仍应由 Linux task26/task18 matrix 继续覆盖；不能只用 macOS 结论外推。
- B3 无 A 机必测前置；若 release 同时宣称与真实 oMLX 大模型并发，则按 §4 另附 A 联合验证，当前不得冒充已完成。
- 本地 release-preview（#315 + #317 + task26 + task27 hardening + task28）已验证：`idoris-upstream` 110/110、`idoris-router --lib` 251/251、subscription Rust matrix（含 disconnect/security/startup）全绿、clippy/fmt/diff-check 全绿。当前 harness 无 Node runtime，`pnpm` 阶段未本地执行；跨平台/Node conformance 以 GitHub CI 为权威，不能把本地缺 Node 写成已通过。
