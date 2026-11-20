# iDoris 评估与接口提案：IDORIS-CORE（ID-1 至 ID-5）

> 草稿，2026-11-20；§3.2.0 记录已定的决策。只读评估：没有改 iDoris 或 Agent24 仓库的任何文件，没有提交、没有 push。
>
> 读取基线：iDoris `main` = `01484547`（v0.5.0，wire 1.2.0）；工作区 `chore/post-v0.5.0` = `49cff2ed`，比 main 只多 CI 与测试阈值改动，本文引用的源码两者一致。Agent24 `main` = `bf7040479`（含 #1937）。
>
> 引用写法：`R/` = `crates/idoris-router/src/`，`B/` = `crates/idoris-backend/src/`，`U/` = `crates/idoris-upstream/src/`，`P/` = `crates/idoris-policy/src/`，`C/` = `crates/idoris-contracts/src/`；`A24:` 前缀 = Agent24 仓库。「已有」一律给 文件:行 或函数名；没有实测过的写「未实测」。

## 0. 结论

| 项 | iDoris 现状（一句话） | 可行性 | 建议 |
|---|---|---|---|
| ID-1 动态端口与发现 | 端口 0 只差两个 parser 放行（`serve_bound` 本来就读 `local_addr()`）；发现文件、launcher 帧扩展位、就绪/上游信息都没有；实例锁和 `/health.instance_id` 已有 | 高，工作量中 | v0.6.0，最先做 |
| ID-2 登记与心跳 | 完全没有（无 UDS 客户端、无父进程监视；launcher 写完 token 就关 stdin，所以 EOF 不能当「父进程死了」） | 高，工作量中 | v0.6.0，紧跟 ID-1；Agent24 的 IC-3 只依赖这两项 |
| ID-3 oMLX 生命周期 | iDoris 里没有任何启停 oMLX 服务进程的代码，只有模型级 load/unload；Supervisor 启动时采样一次引擎状态，失败就粘滞成 `startup_error`，之后一直 503，必须重启 iDoris | 可行，但是五项里最重的一项，难点不在「spawn 一个进程」而在「引擎起来后 Supervisor 怎么对账」 | v0.6.0 做「启动门控 + 拉起 + 停止/重启 + 孤儿接管」（3a；停止/重启的对外控制面取决于决策 D3）；运行期引擎换代后的热恢复（3b）后移 v0.6.1，v0.6.0 先用「引擎死了就让 iDoris 退出由 agent24d 重启」兜底 |
| ID-4 上游与 embedding | `/v1/embeddings` 路由已有，但只在 resident 卡上选候选，默认 on_demand 的 oMLX 卡走不通；自定义 OpenAI 兼容上游能用卡片表达（loopback、resident），但没有生成器、没有凭据注入、没有探测；没有任何预热接口 | embeddings 与预热可行；自定义上游可行但只做 loopback 无密钥 | v0.6.0：embeddings（伴生 resident 卡）+ 预热；自定义上游的 `setup upstream` 后移 v0.6.1 |
| ID-5 契约 | 规范现为修订 1.5，wire 1.2.0；Agent24 方案写的「§2.7 修订 1.4」已被 §2.9 占用，应记为修订 1.6 | 文档工作，需先于编码冻结 | 先发 §2.7 / 新登记协议草案给 Agent24 评审一轮，冻结后再写代码 |

### 比需求文字更重要的七个发现

1. **Supervisor 的启动错误是粘滞的。** `run_actor` 启动时对引擎调一次 `status()`，失败（或持久围栏存在）就把 `startup_error` 锁住，之后 list/status/chat/load/unload 全部直接返回该错误，没有「重新核对」命令（`B/supervisor.rs:1755-1863`，使用点 `:1892,1915,1956,2028,2045`）。README 和安装指南因此要求「先起 oMLX 再起 iDoris」（`README.md:48`、`docs/安装与首次调用-v0.3.1.md:63`）。所以 ID-3 要求的「oMLX 不可达时由 iDoris 拉起」不能只在 iDoris 里加一个 spawn：必须保证「引擎就绪之后才创建 Supervisor」，这又要求 iDoris 在引擎还没好时就能对外答 `/health`。iDoris 里没有 `NotConfigured`（grep 只命中预算与安全模块）；它是 Agent24 自己的错误码，`idoris_host.rs` 里缺 `omlx_url`、组件目录或额度时返回（`A24: rust/crates/agent24-idoris/src/idoris_host.rs:122,471,495,518`），也就是说「oMLX 不可达」这个症状在 Agent24 侧是 connect 阶段拿不到 oMLX 地址，在 iDoris 侧是下面这个粘滞的 `startup_error`，两边各有一半。
2. **`/v1/embeddings` 在默认安装下走不通。** 路由只在 resident `http_service` 卡里选候选（`R/lib.rs:826-829`，定义 `R/dispatch.rs:470-475`），而默认 oMLX 卡是 `on_demand`（`config/components/omlx.yaml`），`RuntimeAdapter` trait 也没有 embeddings 方法（`B/adapter.rs:46-182`）。另外 Agent24 的 `EmbeddingClient::idoris` 发的是具体模型名 `bge-m3-mlx-fp16`（`A24: rust/crates/agent24-models/src/embedding.rs:31,302-311`），没有 `idoris/` 前缀就是无角色请求，要求 key 具备全部角色（`R/auth.rs:72-88`），而 §2.7 的最小数据 key 只绑 `daily`：现在直接打会得到 403 `role_forbidden`。
3. **`idoris setup omlx` 要求 oMLX 已经在线**（`R/setup_omlx.rs:117-177`，不可达报 `unreachable`）。Agent24 IC-1 的 connect 流程是「探测 oMLX → setup → 签发 key」，oMLX 没起时 connect 失败，iDoris 连组件目录都没有。「iDoris 拉起 oMLX」要求 setup 有离线模式（模型绑定改为对照 `~/.omlx/models` 目录校验）。
4. **规范修订号冲突。** 现为修订 1.5（`docs/interfaces/…规范.md:3`）；Agent24 方案 §5 写的「§2.7 修订 1.4」早被 §2.9 占用（修订 1.4）。本次应记为修订 1.6。
5. **kill -9 与 `load.pending` 围栏。** 加载进行中被杀，围栏残留，重启后所有加载 fail-closed，恢复需要人工删文件（`README.md:54`；`B/load_fence.rs:26-88`；路径 `U/omlx/mod.rs:461-485`）。Agent24 的验收项「kill -9 iDoris 后自动恢复」在空闲时能过，加载中被杀就过不了。ID-3 里 iDoris 自己拥有的 oMLX 进程可以安全清围栏（见 2.7.5），外部 oMLX 不能。
6. **包布局是 5 个文件，Agent24 的 lock 里逐个列了。** `.github/workflows/release.yml:106`、`scripts/release-smoke.py:19-24`、`A24: rust/crates/agent24-idoris/idoris.lock.json` 的 `files`。本文的提案不新增随包文件（全部由 `setup` 生成），避免布局变动。
7. **Agent24 已经有同类基础设施，提案按它的惯例对齐。** `daemon.json` 原子写与 `remove_if_owner`（`A24: rust/crates/agent24-protocol/src/state_file.rs`）；`enroll.sock` 的 NDJSON、单帧 ≤16 KiB、服务端先发 challenge、逐帧复核 `LOCAL_PEERTOKEN`（`A24: docs/agent/CORE-2-AGENTEAR-SLICE.md §3.1`、`rust/apps/agent24d/src/enroll_endpoint.rs`）；iDoris 监管器的 `listeners_owned`（按进程组核对端口属主）、`validate_health`、`recover_leftover`（`A24: rust/crates/agent24-idoris/src/supervisor.rs:221,764,954`）。登记协议因此照 enroll 的帧风格设计（`{"v":1,"op":…}`），agent24d 可以复用帧读取与逐帧复核代码。

## 1. 逐项现状（ID-1 至 ID-4）

### 1.1 ID-1 动态端口与发现

| 问题 | 现状与证据 | 缺口 |
|---|---|---|
| serve 能不能绑 :0 | 数据面端口来自 `IDORIS_PORT`，`parse_port` 只收 1..=65535（`R/lib.rs:192-214`，0 在 `:209` 被拒）；Admin 端口 `IDORIS_ADMIN_PORT` 同样拒 0（`R/admin.rs:58-83`，`:77`）。但 `serve_bound` 本来就按 `listener.local_addr()` 取真实端口（`R/bin/idoris.rs:855-860`），测试 `bound_servers_serve_both_surfaces_and_stop_on_injected_signal` 已经用 `127.0.0.1:0` 绑过（`R/bin/idoris.rs:1139-1146`）；`ListenPlan::new` 不拦 0（`R/cloud/ingress/bind.rs:43-54`） | 放开两个 parser；`run` 里 `println!("idoris listening on http://{addr}")` 打的是请求地址（会打出 `:0`，`R/bin/idoris.rs:705`），要删；`idoris admin <res> --token-stdin` 用 `IDORIS_ADMIN_PORT` 连 Admin（`R/bin/idoris.rs:783-799`），动态端口下要能从发现文件取端口（可后移） |
| 就绪后写发现文件 | 没有。stdout 的 `idoris listening on …` 出现两次（`:705`、`:871`），规范 §2.7 明说「stdout 不属于契约」（规范 L159） | 新增发现文件模块、写入时机、清理 |
| stdin 帧有哪些字段 | 只有一种：`serve --admin-token-stdin`，一帧 65 字节 = 64 位小写 hex + `\n`，10 秒内读完，读取发生在端口绑定之后（`R/bin/idoris.rs:699-703,737-779`；`R/cli.rs:176-185` 对 serve 只精确匹配两种形式）。Agent24 的 launcher 写完就 `shutdown()` 关闭 stdin（`A24: …/agent24-idoris/src/supervisor.rs:910-931`） | 帧没有扩展位；`parse_args` 是精确匹配，加字段必须加新 flag |
| 实例锁怎么做 | `acquire_instance_lock`（`R/storage.rs:148-179`）：对 `<canonical IDORIS_DB_PATH>.lock` 加独占 flock（`File::try_lock`），内核在任何退出（含崩溃）时释放；`File` 是 close-on-exec，子进程不继承。符号链接别名走 `canonical_lock_base`（`:189-213`）共用一把锁。已被占用则在绑定端口之前退出码 3（`R/bin/idoris.rs:469-480`，常量 `:57`）；测试 `tests/launcher_lifecycle.rs::second_instance_on_same_db_exits_3_and_first_stays_healthy` | 无缺口。这是「孤儿接管不起第二个」在 iDoris 侧的唯一事实来源，发现文件只是提示 |
| `/health` 返回什么 | 免鉴权（`R/lib.rs:574-576`）；`{status:"ok", service:"idoris", version, contract_version, instance_id, components, dev_no_key_enabled}`（`R/lib.rs:217-225,1877-1887`）。`instance_id` 是每进程一个 UUID v4（`R/lib.rs:377`），与 Admin `status.instance_id` 同值（`R/admin.rs:309,355`，测试 `R/bin/idoris.rs:1173`）。`status` 恒为 `"ok"`，没有就绪、上游状态 | 加 `phase/ready/upstreams/started_at_ms/registered`，见 2.4 |

### 1.2 ID-2 登记与心跳

| 问题 | 现状与证据 | 缺口 |
|---|---|---|
| UDS 客户端、登记、心跳 | 没有 | 新增 `register` 模块（客户端状态机、退避、心跳、发现文件自愈、孤儿计时） |
| 父进程监视 | 没有，stdin 读完就不再读；且 launcher 写完即关，EOF 无意义 | 不能用 stdin EOF，只能用登记会话的有无做孤儿判定 |
| 信号与退出 | SIGTERM、SIGINT 同一优雅路径（`R/bin/idoris.rs:63-76`）；总时长约 15 秒 = 排空 10 秒（`:60`）+ 销毁 runtime 5 秒（`:85`）；退出码 0 / 1 / 3 | 新增退出码需要进规范（孤儿超时、引擎失败自退，见 2.6） |
| 可用依赖 | `ring`（随机数、HMAC）已在 router 依赖里（`crates/idoris-router/Cargo.toml`：`ring = "0.17"`）；`uuid` v4；tokio `net` 已启用，`tokio::net::UnixStream` 可用（根 `Cargo.toml` 的 tokio features） | 无新 crate 需求。注意 workspace `unsafe_code = "forbid"`（`Cargo.toml:106-107`）：不能裸调 libc 的 `getsockopt`/`proc_pidinfo`，要用 `nix`/`rustix` 的安全封装，或不做 |
| 被 Agent24 校验的签名身份 | 发布 CLI 以 `--identifier ai.idoris.cli` 签名（`scripts/macos_signed_release.py:313`），hardened runtime，Team `HYAP444FN9`（规范 §2.7 下载行） | 标识符必须保持稳定，改动要同步 Agent24 的允许清单 |

### 1.3 ID-3 oMLX 生命周期

| 问题 | 现状与证据 | 缺口 |
|---|---|---|
| oMLX 现在由谁拉起 | iDoris 不拉起。用户自己（oMLX.app 或 `omlx serve`），或 Agent24 桌面端 `OmlxStart`：`execFile('omlx', ['serve','--port',p])`，key 走环境变量 `OMLX_API_KEY`（`A24: apps/desktop/src/main/ipc/index.ts:813-836`），探测端口 `[8088, 8000, 8001]`（`:265`）。iDoris 只在 `setup omlx` 里探测一次（`R/setup_omlx.rs:117-177`），endpoint 必须是 loopback、无路径（`:64-82`） | 探测、拉起、停止、重启、孤儿接管全部缺 |
| 进程所有权原语 | 只给单模型 runtime（mlx_lm.server、llama.cpp）：`LocalRuntimeKind`（`U/local_runtime.rs:12-16`）、`ManagedRuntimeProcess`（`U/local_runtime_process.rs:26-65`：独立进程组；`shutdown` SIGTERM 后 SIGKILL，`:93-129`；Drop 杀整组，`:393-399`；stdio 全 null，`:38-40`）、受信任主机配置 `IDORIS_LOCAL_RUNTIME_CONFIG`（`R/runtime_launch_config.rs`）。语义是「load 即 spawn、unload 即 stop」（`U/local_http_runtime.rs:263-293`） | oMLX 是多模型服务器，服务器寿命不等于模型寿命；需要「引擎级」生命周期，复用 `ManagedRuntimeProcess` 的进程组与停止逻辑，但 launch 结构、环境、日志都要新做 |
| lifecycle adapter 做的是什么 | `OmlxAdapter` 是纯 HTTP：`list`=`GET /v1/models/status`、`status`=`GET /api/status`+admin activity、`load`/`unload`=`POST /v1/models/{id}/load\|unload`、`probe_ready`、`chat`（`U/omlx/mod.rs:219-271,378-415`）。`create_adapter` 只认 provider id `omlx`（`U/factory.rs:9-19`）。这里的 load/unload 是**模型**，不是进程 | 不变，继续用；引擎级管理放在它下面 |
| Supervisor 已有的生命周期能力 | 模型级：`list/status/is_ready/chat/load_with_headroom/unload`（`B/supervisor.rs:274-362`），单写者 actor，加载/驱逐互斥，容量账本，持久加载围栏。**启动时对引擎采样一次，失败即粘滞**（见 0 的发现 1） | 没有「引擎换代后重新对账」；没有 `Shutdown/Join`（`Supervisor::spawn` 丢弃 JoinHandle，`B/supervisor.rs:480-494`，注释自述「graceful-shutdown task tracking…deferred」，`:386-389`） |
| 加载围栏 | `load.pending`，位置 `$IDORIS_STATE_DIR` 或 `$HOME/.local/state/idoris`，再加 `omlx/<URL 编码后的 endpoint>/load.pending`（`U/omlx/mod.rs:461-485`）；`LoadFence::claim` 还对 `.owner.lock` 加独占 flock，同一引擎只能有一个 Supervisor 持有（`B/load_fence.rs:26-48`） | 引擎换代时旧 actor 不释放就无法重新 claim（影响 3b 设计） |
| 模型列表与类型 | `GET /v1/models` 只有 `{id, object, owned_by}`（`R/models.rs:31-42`）；类型来自 `GET /v1/models/local`，但写死扫 `~/.omlx/models`（`R/recommendation.rs:54-70`、`idoris-recommender/src/local_models.rs:47-49`），不认 oMLX 的 `OMLX_BASE_PATH` / `--model-dir` | 托管模式下 iDoris 知道自己传的 model-dir，应该用同一个目录 |
| oMLX 密钥 | 唯一解析入口 `U/omlx/key.rs`（`IDORIS_OMLX_API_KEY` → `~/.omlx/settings.json` → 无）；生命周期适配器、常驻代理（`R/proxy.rs:785-791`）、模型列表（`R/models.rs:187`）都只经它 | 单点，便于给「iDoris 私有实例」注入进程内 key（S 级改动） |
| oMLX 本身怎么装 | iDoris 不负责安装引擎（`docs/安装与首次调用-v0.3.1.md:53`） | 见决策 D2：引擎缺失时只能报因，不能「拉起」 |

### 1.4 ID-4 上游与 embedding

| 问题 | 现状与证据 | 缺口 |
|---|---|---|
| 有没有 `/v1/embeddings` | 有：路由 `R/lib.rs:435`，处理函数 `:747-996`；强制能力 `Embedding`（`:779`），privacy 缺省 `local_only`，上游路径、请求体、响应体原样透传（测试 `R/embeddings_wiring_tests.rs::embeddings_preserves_upstream_path_payload_and_response`），回传 `X-iDoris-Served-Locality`（`:971-977`），付费卡直接拒（`:876-883`）。**只在 resident 卡里选候选**（`:826-829`） | 默认 oMLX 卡是 on_demand，候选为空，返回 503；Supervisor 与 `RuntimeAdapter` 没有 embed（`B/supervisor.rs:112-140`、`B/adapter.rs:46-182`） |
| 角色与密钥 | role 缺省（模型名不带 `idoris/`）要求 key 有全部角色（`R/auth.rs:72-88`）；`embed` 是合法角色（`P/role.rs:27-36`），可用 `extensions.idoris_role_models.embed` 绑定具体模型（`R/setup_omlx.rs:197-213`） | Agent24 要么改发 `model:"idoris/embed"` 并让数据 key 含 `embed`，要么拿全角色 key |
| 超时与限额 | resident 代理：header 10 秒、body 60 秒、响应上限 8 MiB、并发 32、连接失败重试 250 ms 与 1 秒（`R/proxy.rs:827-841`） | embedding 模型冷加载可能超过 10 秒 header 超时，需要预热或按路径放宽 |
| 自定义 OpenAI 兼容上游能不能用组件卡表达 | 可以：卡片契约是 `form: http_service` + `endpoint` + `load_policy`（`C/component_card.rs:35-49`），注册校验要求 loopback 声明的 endpoint 主机在白名单（`P/registry.rs:129-163`），resident 必须配 `keepalive.pinned:true`（`P/card_validation.rs:12-71`）；`provider.id` 任意，只要不重复（`P/registry.rs:185-200`）。`LM Studio`、`Ollama` 这类 loopback 上游，一张 resident 卡即可走 chat、embeddings、rerank、messages | 没有生成器：`setup` 只有 `omlx`，只写 `omlx.yaml` 与 `subscription.yaml`（`R/setup_omlx.rs:17,306-307`）；没有凭据注入：只有 `provider.id == "omlx"` 才带 key（`R/proxy.rs:152,787-791`），`/v1/models` 也只对 loopback `omlx@` 卡带 key（`R/models.rs:92-114`）；没有可达性探测；`Form` 枚举是冻结契约，不应加新 kind（`C/component_card.rs:15-22`） |
| 有没有预热 | 没有：grep `warm` 无命中；数据面路由全集见 `R/lib.rs:435-482`。聊天会隐式加载（`R/dispatch.rs:1011-1119`：catalog → `load_with_headroom` → chat），1GB 余量确认只挂在 chat 请求头上（规范 §2.8） | 新增预热接口，复用同一段加载前置逻辑 |

### 1.5 ID-5 契约

| 问题 | 现状与证据 |
|---|---|
| 规范版本 | 修订 1.5（2026-11-19，只改状态说法，规范 L3）；§2.7 标题仍写「修订 1.3」；wire 契约 `CONTRACT_VERSION = "1.2.0"` |
| wire 版本常量要改的位置 | `C/version.rs:21`、`packages/contracts/src/version.ts:26`、`packages/router/tests/health-identity.test.ts:17`、`conformance/tests/response-headers.test.ts:42`，另有 CI 的 `pnpm check:contract-drift` 一并校验 |
| 包布局 | 固定 5 文件（`.github/workflows/release.yml:106`、`scripts/release-smoke.py:19-24`；规范 §2.7「布局」行） |
| `/health` 没有 schema | 规范 §4.2 L298 明说 health 等 schema 不存在，只有文字规范；本次新增字段只能靠 conformance 用例锁形状 |

## 2. 接口提案

所有字段名、帧、常量都是「提案」，冻结前可改；冻结后写进规范（见第 5 节）。风格尽量与 Agent24 已有的 `daemon.json` 与 `enroll.sock` 一致。

### 2.1 标识、密钥，以及对 Agent24 三道闸门的回答

| 名称 | 谁生成 | 机密性 | 出现位置 | 用途 |
|---|---|---|---|---|
| `instance_id` | iDoris，进程启动时 UUID v4（已有，`R/lib.rs:377`） | 公开：`/health` 免鉴权，任何本机进程都能读 | `/health`、Admin `status`、发现文件、`register` 帧 | 标识一次进程生命；核对「这个端口上答话的是不是登记的那个实例」。不是凭据 |
| `instance_nonce` | iDoris，进程启动时 256 位（`ring::rand::SystemRandom` 取 32 字节，小写 hex 64 字符） | 秘密 | 只在进程内存与发现文件（0600）。不进 `/health`、Admin、日志、argv、环境变量、错误信息，也不在 UDS 上明文传输 | HMAC 密钥：证明「我就是写了这份发现文件的实例」；也让 agent24d 证明「我读得到发现文件」 |
| `conn_id`、挑战 `nonce` | agent24d（UDS 服务端），每个连接一次 | 一次性 | UDS | 防重放，绑定到 HMAC |
| `session_id` | agent24d，`ack` 里 | 非凭据 | `ack`、日志 | 会话标识 |
| Admin token | launcher（不变） | 秘密 | stdin 帧；会话内可轮换（2.5） | 不变 |

**nonce 由谁生成：建议 iDoris 生成。** 对 Agent24 提出的两个选项：

| | A：iDoris 生成，写进发现文件（建议） | B：agent24d 生成，经 stdin 交给 iDoris，并由 agent24d 自己持久化到 0600 文件 |
|---|---|---|
| nonce 的语义 | 「这一次进程生命」，由进程自己生成，天然不会跨生命复用，不依赖 launcher 的随机数实现 | 由 launcher 决定生命周期，launcher 误复用就会跨实例复用 |
| 接管孤儿 | 发现文件是唯一信息源，文件内容等于进程内存，不会出现两边不一致 | agent24d 崩在「写自己的文件」与「交给 iDoris」之间会留下不一致；而 Agent24 方案本来就要求 iDoris 把 nonce 写进发现文件，实际变成两份副本 |
| stdin 帧 | 少一个秘密字段 | 多一个秘密字段 |
| 手工启动（无 launcher） | 仍能产出完整发现文件 | 没有 nonce |
| 安全性 | 相当：两者最终都落在 0600 文件里 | 相当 |
| 代价 | agent24d 首次启动要等发现文件才能拿到 nonce（它本来就要等端口与就绪，没有额外等待） | 多一份要同步的持久状态 |

保留一个可选的帧字段 `instance_nonce`（缺省由 iDoris 生成），只给确定性测试用，生产不用。

**要说清楚的边界：** nonce 在 0600 文件里，挡不住同 UID 的恶意进程，挡的是（1）连错实例、读到陈旧文件，（2）读不到该目录的沙箱进程（例如 Agent24 的模块沙箱）冒充。真正的身份闸门是 Agent24 对登记方的 `LOCAL_PEERTOKEN` 与 codesign 校验，两者叠加。

**问：`/health` 能不能返回 `instance_nonce` 或 `instance_id`？** `instance_id` 已经在返回里（`R/lib.rs:222,1883`），公开，保持不变；`instance_nonce` 永远不出现在 `/health`、Admin 或任何响应里，因为它是重认证凭据。

**三道验收闸门如何满足：**

| Agent24 闸门 | iDoris 提案 | 需要 Agent24 做的 |
|---|---|---|
| ① 用 `LOCAL_PEERTOKEN` 与 codesign team 校验登记方，不看 pid | 保持标识符 `ai.idoris.cli` 与 Team `HYAP444FN9` 不变（`scripts/macos_signed_release.py:313`，规范 §2.7 下载行）；建议要求串 `identifier "ai.idoris.cli" and anchor apple generic and certificate leaf[subject.OU] = "HYAP444FN9"`。`register` 里的 `pid` 只供日志与交叉核对。iDoris 在收到 `challenge` 之前不写任何字节（配合 Agent24 R8 的入队检查）；UDS fd 是 close-on-exec（tokio 默认），不会被 oMLX 子进程继承。未签名的开发构建：iDoris 不提供绕过，由 Agent24 的开发开关处理 | 允许清单加 `ai.idoris.cli`；按安装路径核对可执行文件 |
| ② 新 agent24d 没有旧 nonce 与 admin token | nonce：从发现文件读，登记时用 HMAC 证明，nonce 本身不上线（2.5）。admin token：会话内可轮换，`set_admin_token` 帧，旧 token 立即失效（2.5） | 接管后生成新 token，通过 `set_admin_token` 设给 iDoris，再交给 Electron main；确认 Electron main 能重新取 token（Q3） |
| ③ 发现文件里的端口确实是登记进程在监听 | 登记帧带 `pid`、`data_port`、`admin_port`；Agent24 现有的 `listeners_owned`（按进程组核对 LISTEN 属主，`A24: …/agent24-idoris/src/supervisor.rs:764-802`）改成「`LOCAL_PEERTOKEN` 校验过的 pid 持有这两个端口」即可；`/health.instance_id` 与登记的 `instance_id` 一致作为第二道 | **不要带数据 key 调 `/health`**：它免鉴权，带上 key 反而可能把 key 发给占着端口的冒充者。key 只在上述核对全部通过之后才发 |

### 2.2 launcher stdin 帧 v2

新增 `idoris serve --launcher-stdin`，与 `--admin-token-stdin` 互斥（同时给 = 用法错误）。旧 flag 与旧 65 字节帧完全不变（ADR-035 的 Electron 侧消费者与现有测试不受影响）。

字节层：一行 UTF-8 JSON，以 `\n` 结尾，总长（含 `\n`）≤ 4096 字节；10 秒内读完（沿用 `read_admin_token_with_timeout`，`R/bin/idoris.rs:744-768`）；读到第一个 `\n` 即止，之后不再读 stdin，launcher 写完就关 stdin 是预期行为。读取位置不变：端口绑定之后、对外服务之前，所以帧无效仍然在服务任何请求之前退出 1。

| 字段 | 类型 | 必填 | 约束 |
|---|---|---|---|
| `v` | integer | 是 | 必须等于 1 |
| `admin_token` | string | 是 | 64 位小写 hex，同 `AdminSessionToken::from_launcher_secret`（`R/admin.rs:131-141`） |
| `discovery_path` | string | 否 | 绝对路径，≤ 1024 字节，不含 NUL，规则见 2.3 |
| `register_socket` | string | 否 | 绝对路径，≤ 103 字节（macOS `sun_path` 为 104 含 NUL），不含 NUL。**出现则 `discovery_path` 必填**，否则 agent24d 拿不到 nonce |
| `orphan_timeout_s` | integer | 否 | 0 到 86400；0 或缺省 = 永不因「无登记会话」退出（2.6） |
| `instance_nonce` | string | 否 | 64 位小写 hex；缺省由 iDoris 生成（建议）；只给确定性测试用 |
| `engine` | object | 否 | ID-3：`{"omlx_executable"?: 绝对路径, "stop_on_exit"?: "owned" 或 "never"}`；缺省 `stop_on_exit:"owned"`（2.7） |

示例（一行）：

```
{"v":1,"admin_token":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","discovery_path":"/Users/x/.agent24/run/idoris.json","register_socket":"/Users/x/.agent24/run/idoris-register.sock","orphan_timeout_s":900}\n
```

（行末的 `\n` 是一个换行字节 0x0A，不是反斜杠加 n。）

解析规则：严格，`deny_unknown_fields`（launcher 与 iDoris 是锁版本一起发的，字段拼错静默失效比直接失败更危险）；任何错误都只输出固定文案 `[idoris] 启动失败：launcher 帧无效（<原因码>[：<字段名>]）`，退出码 1，**绝不回显字段值**（serde 的默认错误信息会带输入片段，要映射成固定原因码：`frame_too_long`、`frame_not_json`、`unsupported_frame_version`、`field_invalid`、`unknown_field`、`timeout`）。

被否决的做法：
- argv `--discovery-file/--register-socket`：路径虽不是秘密，但 `ps` 可见，契约分散在两处，`parse_args` 是精确匹配（`R/cli.rs:176-235`）要大改。
- 环境变量：Agent24 `env_clear()` 之后显式传，可行；但环境变量会被 iDoris 自己的子进程（oMLX）继承，要额外清洗，且多一个通道。
- 直接替换旧 65 字节帧：破坏 ADR-035 与现有测试（`tests/cli.rs` 的 launcher token 用例）。

### 2.3 发现文件

**路径：** 由 launcher 在帧里给绝对路径（Agent24 方案：`<data_dir>/run/idoris.json`）。iDoris **没有默认路径**：帧里没有 `discovery_path` 就不写。理由：iDoris 不知道 Agent24 的数据目录，在 `IDORIS_STATE_DIR` 下放默认文件会产生第二个事实来源。

**路径校验**（全部在写任何东西之前，失败 = 退出 1、尚未对外服务）：
1. 绝对路径、≤ 1024 字节、不含 NUL。
2. `parent = dirname(path)`，要求 `canonicalize(parent)` 与 `parent` 逐字节相等，这样所有中间目录的符号链接都被拒绝。launcher 必须传 canonical 路径（macOS 的 `/var` 实为 `/private/var`，传 `/var/folders/...` 会被拒；Agent24 的 `DataPaths::canonical()` 已经这样做）。
3. 以 `O_DIRECTORY|O_NOFOLLOW` 打开 parent，`fstat`：属主等于 euid，`mode & 0o022 == 0`（组与其他人不可写），建议 0700。
4. 目标若已存在：`lstat` 必须是普通文件（不是符号链接、目录、FIFO、socket）且属主等于 euid，否则拒绝并退出，不删除。
5. iDoris 不创建 parent 目录（由 launcher 负责，避免在错误位置建出权限不对的目录）。

**内容**（紧凑单行 JSON + `\n`，≤ 4096 字节，UTF-8）：

```json
{"schema":1,"service":"idoris","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","instance_nonce":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","pid":41234,"data_port":51234,"admin_port":51235,"version":"0.6.0","contract_version":"1.3.0","started_at_ms":1763625330123,"launch":{"db_path":"/Users/x/.agent24/idoris/idoris.sqlite3","state_dir":"/Users/x/.agent24/idoris/state","components_dir":"/Users/x/.agent24/components/idoris-config","allocated_budget_bytes":17179869184}}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `schema` | integer | 固定 1 |
| `service` | string | 固定 `"idoris"` |
| `instance_id` | string | UUID v4，36 字符，等于 `/health.instance_id` |
| `instance_nonce` | string | 64 位小写 hex，秘密 |
| `pid` | integer | `std::process::id()` |
| `data_port`、`admin_port` | integer | 1 到 65535，取自 `local_addr()`，不是请求值 |
| `version`、`contract_version` | string | 等于 `/health` 同名字段 |
| `started_at_ms` | integer | 进程开始 `run()` 时的 Unix 毫秒（墙钟） |
| `launch` | object，可选 | 启动输入快照：`db_path`（canonical 锁基路径，`R/storage.rs:189-213`）、`state_dir`、`components_dir`（canonical）、`allocated_budget_bytes`（u64 或 null）。供 agent24d 判断「这个孤儿的启动输入还是不是我要的」，不一致就重启，不接管 |

没有放 `pid_start`：iDoris 取自己的内核启动时间需要 libproc 或 unsafe（workspace 禁 unsafe）。消费者自己读内核启动时间，与 `started_at_ms` 粗略比对（几秒内）即可；pid 复用由登记（HMAC）与 `/health.instance_id` 兜底。

**原子写法（iDoris 侧）：**
1. 序列化成字节。
2. 在同目录创建临时文件 `.idoris-discovery.<pid>.<16 位随机 hex>.tmp`：`open(O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC, 0600)`，再 `fchmod(fd, 0600)`（不受 umask 影响）。名字不可预测加 `O_EXCL`，不会被预先放置的链接带偏。
3. `write_all`，`fsync(fd)`，关闭。
4. `rename(tmp, final)`（同目录同卷，原子替换；读者要么看到旧文件，要么看到完整新文件）。
5. 打开 parent，`fsync`（尽力而为，失败只记日志）。
6. 失败路径 `unlink(tmp)`。launcher 要求了发现文件却写失败 = 启动失败（退出 1，固定文案），不允许「跑着但找不到」。

macOS 上 `fsync` 不保证落盘到磁盘缓存，但这里不需要掉电后仍在（重启后 pid 都没了），所以不用 `F_FULLFSYNC`。

**何时写：** 同时满足（a）已持有实例锁，（b）数据面与 Admin 都已绑定，（c）服务任务已启动且 `/health` 能答（含 `phase:"starting"`）。登记在写文件之后。

**自愈：** 每个心跳周期（10 秒）`lstat` 目标并核对 `instance_id`；文件缺失或不一致就重写（`rm -rf ~/.agent24/run` 这类误删可恢复）。

**何时删：** 优雅退出（SIGTERM、SIGINT、孤儿超时）时，在监听关闭之后、实例锁释放之前，**仅当文件里的 `instance_id` 等于自己**才删（持 `<final>.lock` 的 flock 做读、比对、unlink，照 `A24: state_file::remove_if_owner`）。SIGKILL、panic、掉电不删，即「崩溃残留」。

**残留的识别（消费者侧，建议顺序）：** 读文件（≤ 4 KiB、普通文件、属主正确、`schema==1`）；`kill(pid,0)` 存活且 uid 相同且可执行路径等于安装路径；`GET /health`（不带任何凭据，2 秒）返回 `service=="idoris"` 且 `instance_id` 相等；LISTEN 属主核对；最后以登记为准。任何一步失败即视为陈旧，pid 已死才可以删文件。

**iDoris 自己遇到陈旧文件：** 实例锁是唯一事实来源。拿到锁的新实例把已有文件视为陈旧并覆盖；没拿到锁（退出码 3）的实例**不碰**文件，因为它属于还活着的那个实例。

### 2.4 `/health` 扩展（wire 1.3.0，只增不改）

保持不变（Agent24 的 `validate_health` 依赖它们，`A24: …/supervisor.rs:945-966`；它的反序列化结构忽略未知字段，所以新增字段是兼容的）：`status`（恒为 `"ok"`，语义仍是「进程活着并在服务」）、`service`、`version`、`contract_version`、`instance_id`、`components`、`dev_no_key_enabled`。

新增：

```json
{"status":"ok","service":"idoris","version":"0.6.0","contract_version":"1.3.0","instance_id":"3f0c6a52-…","components":2,"dev_no_key_enabled":false,
 "phase":"ready","ready":true,"started_at_ms":1763625330123,"registered":true,
 "upstreams":[{"id":"omlx","state":"ready","managed":true,"owned":true},
              {"id":"omlx-embed","state":"ready","managed":true,"owned":false}]}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `phase` | string | `starting`：已绑定、已在答 `/health`，但启动门控未完成（正在探测或拉起 oMLX）；`ready`：至少一个 upstream 为 `ready`；`degraded`：进程正常但没有 upstream 为 `ready` |
| `ready` | boolean | 等于 `phase=="ready"` |
| `started_at_ms` | integer | 同发现文件 |
| `registered` | boolean | 当前是否有已 `ack` 的登记会话；没有 launcher 时恒为 false |
| `upstreams[].id` | string | 卡的 `provider.id` |
| `upstreams[].state` | string | `ready`、`starting`、`unavailable`、`restarting`、`failed`、`stopped`、`absent` |
| `upstreams[].reason` | string，可选 | 稳定原因码：`engine_not_found`、`port_in_use`、`auth_required`、`auth_failed`、`startup_timeout`、`crash_loop`、`fence_pending`、`budget_zero`、`startup_latched`。给人看的文字另放，不属于契约 |
| `upstreams[].managed` | boolean | 卡声明了由 iDoris 管理引擎（2.7） |
| `upstreams[].owned` | boolean | 引擎进程是 iDoris 自己拉起的（外部引擎、被接管的外部引擎为 false，iDoris 永远不会给它发信号） |

不变量：
- **`/health` 必须 O(1)，不做任何上游 I/O，响应 ≤ 200 毫秒。** 状态来自后台探测器与管理器写入的快照。Agent24 每 5 秒探测、2 秒超时、连续 3 次超时就判卡死并 SIGTERM（`A24: docs/design/IDORIS-CORE-PLAN.md` §3.2），一个会被慢上游拖住的 `/health` 会造成误杀与重启风暴。
- 由 Supervisor 管理的卡，状态要问 `RuntimeRegistry` 的 `status()`（`R/runtime.rs:209-217`），带 200 毫秒超时；`status()` 返回启动错误就报 `unavailable` 加 `reason:"startup_latched"`，不能因为 `/v1/models` 能通就报 `ready`（聊天实际是 503）。
- resident 卡由探测器周期探测（`GET /v1/models`，带 `key.rs` 的 key，间隔 5 秒、2 秒超时）；401/403 报 `auth_required` 或 `auth_failed`。

**两级就绪（需要 Agent24 同意，见 Q4）：** Agent24 现在的 `ready()` 等 `/health` 加端口属主加 Admin `status` 200，窗口 30 秒（`A24: …/supervisor.rs:606-693`、`Timings.ready`）。托管 oMLX 冷启动（Python 加 MLX 加载加扫描模型目录）可能超过 30 秒，因此「iDoris 进程可用」（`status=="ok"`、端口、Admin）和「模型可用」（`phase=="ready"`）必须分开：前者用现有 30 秒窗口，后者允许 `starting` 持续到引擎启动超时（默认 90 秒），界面显示「模型服务启动中」，而不是把 iDoris 判为失败并重启。

### 2.5 登记协议 v1（iDoris 为客户端，agent24d 为服务端）

**传输与帧**：AF_UNIX、SOCK_STREAM，路径来自 stdin 帧的 `register_socket`。一行一帧：UTF-8 JSON 对象，以 `\n` 结尾，**每帧（含 `\n`）≤ 16384 字节**，不含 `\r`、BOM；超长、非 UTF-8、非对象、空行都按 `bad_frame` 处理并关闭。接收方忽略未知字段，必填字段严格校验，`v` 必须等于 1。

选 NDJSON 而不是长度前缀的理由：与 Agent24 已有的 `enroll.sock` 一致（`A24: …/enroll_endpoint.rs`：`MAX_FRAME = 16*1024`，一行一帧），agent24d 可以复用同一个帧读取器与逐帧 `LOCAL_PEERTOKEN` 复核；全是文本，`socat` 可直接调试；本协议没有二进制载荷。16 KiB 上限同 enroll；本协议最大的帧也只有约 350 字节。

**时序**（agent24d 先说话，同 Agent24 enroll 的 R8）：

```
iDoris                                   agent24d
  | -------- connect(register_socket) ----> |   (2 秒超时)
  | <-------- challenge ------------------- |   (iDoris 收到前不写任何字节；5 秒内必须到)
  | --------- register -------------------> |   (收到 challenge 后 1 秒内发出)
  | <-------- ack ------------------------- |   (5 秒内)
  | --------- heartbeat seq=1 -------------> |   (ack 后立即发，之后每 heartbeat_ms)
  | <-------- heartbeat_ack seq=1 ---------- |   (5 秒内)
  | ...                                      |
  | --------- bye (优雅停止时) -------------> |
```

**帧（字节级示例，测试向量见附录 B）：**

`challenge`（服务端到客户端）：
```
{"v":1,"op":"challenge","conn_id":"0123456789abcdef","nonce":"00112233445566778899aabbccddeeff"}
```
`conn_id` 16 个小写 hex，`nonce` 32 个小写 hex（128 位，同 enroll 的 challenge，`A24: CORE-2-AGENTEAR-SLICE.md §3.1`）。

`register`（客户端到服务端）：
```
{"v":1,"op":"register","nonce":"00112233445566778899aabbccddeeff","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","pid":41234,"version":"0.6.0","contract_version":"1.3.0","data_port":51234,"admin_port":51235,"started_at_ms":1763625330123,"phase":"ready","resume":false,"proof":"386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884"}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `nonce` | string | 原样回显 challenge 的 nonce；缺失或不符 = `identity_rejected` |
| `instance_id`、`pid`、`version`、`contract_version`、`data_port`、`admin_port`、`started_at_ms` | 同发现文件 | 服务端应与发现文件交叉核对 |
| `phase` | string | `starting` 或 `ready` 或 `degraded`，同 `/health` |
| `resume` | boolean | 本进程此前已登记过（重连）则为 true，仅供日志与统计 |
| `proof` | string | 64 位小写 hex，见下 |

**`proof`（不让秘密上线）：** `proof = HMAC-SHA256(key, transcript)`，其中 `key` 是 `instance_nonce` 解码后的 **32 个原始字节**，`transcript` 是下列 ASCII 字符串，以单个 `\n`（0x0A）分隔、末尾没有 `\n`，数字用十进制、无前导零：

```
idoris-register/v1 \n conn_id \n challenge_nonce \n instance_id \n pid \n data_port \n admin_port \n started_at_ms
```

`challenge_nonce` 和 `conn_id` 每个连接都新，所以不能重放；端口与 pid 进了 transcript，中途不能篡改。服务端用常量时间比较。冒充的服务端只能看到公开信息（instance_id、端口、pid），拿不到 nonce。

`ack`（服务端到客户端）：
```
{"v":1,"op":"ack","session_id":"a1b2c3d4e5f60718293a4b5c6d7e8f90","heartbeat_ms":10000,"proof":"099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967"}
```
`session_id` 32 个小写 hex；`heartbeat_ms` 心跳间隔，缺省 10000，iDoris 夹到 2000 到 60000；`proof = HMAC-SHA256(key, "idoris-ack/v1" \n conn_id \n challenge_nonce \n session_id)`，这是**服务端对 iDoris 的证明**（双向握手）：它读得到发现文件，才算得出来。iDoris 校验失败就关闭连接，不发心跳，按退避重连，仍继续服务。

`heartbeat` 与 `heartbeat_ack`：
```
{"v":1,"op":"heartbeat","seq":1,"phase":"ready","ready":true}
{"v":1,"op":"heartbeat_ack","seq":1}
```
`seq` 从 1 起每次加 1（u64）；`ack` 必须回显 `seq`。

`error`（任一方向，之后关闭连接）：
```
{"v":1,"op":"error","code":"identity_rejected","message":"…"}
```
`message` ≤ 200 字符，不含秘密。服务端可发的 `code`：`identity_rejected`、`proof_invalid`、`unsupported_version`、`contract_incompatible`（`contract_version` 主版本不是 1，或低于 Agent24 要求的最低次版本）、`bad_frame`、`timeout`、`busy`（并发已满）、`instance_conflict`、`internal`。

`bye`（客户端到服务端，优雅停止时尽力发送，预算 ≤ 500 毫秒）：
```
{"v":1,"op":"bye","reason":"shutdown"}
```
让 agent24d 区分「预期停止」与「崩溃」，对应 Agent24 方案里卡死与崩溃分开记录的要求。

`set_admin_token`（服务端到客户端，**仅在 `ack.proof` 已通过的会话上**接受）与 `admin_token_ack`：
```
{"v":1,"op":"set_admin_token","token":"<64 位小写 hex>"}
{"v":1,"op":"admin_token_ack"}
```
这是闸门②里 admin token 的重认证办法：新 agent24d 没有旧 token，就生成新的设给 iDoris（进程内存里原子替换，旧 token 立即失效），再交给 Electron main。校验同 `from_launcher_secret`；10 秒内最多一次；iDoris 的 Admin 状态里 `token` 目前是 `Arc<AdminSessionToken>` 不可变（`R/admin.rs:163-167`），要改成可原子替换。ADR-035 的约束（token 不进 argv、环境变量、文件、日志）不受影响；需要规范明确「会话内轮换」这一条。

**计时与退避：**

| 项 | 值 |
|---|---|
| 连接超时 | 2 秒 |
| 等 challenge | 5 秒 |
| 等 ack | 5 秒 |
| 心跳间隔 | `ack.heartbeat_ms`，缺省 10 秒，±10% 抖动 |
| 等 heartbeat_ack | 5 秒 |
| iDoris 判失联 | 连续 3 次心跳未应答（约 30 秒）、或任何读写错误、或 EOF |
| 服务端判失联（建议） | 3 倍 `heartbeat_ms`（30 秒）没有任何帧 |
| 重连退避 | 第 1 次立即；之后 0.25、0.5、1、2、4、8 秒，再之后固定 10 秒；每次 ±20% 抖动；会话持续满 60 秒后清零 |
| 特殊原因 | `identity_rejected`、`unsupported_version`、`contract_incompatible`：固定 60 秒；`proof_invalid`：30 秒；`busy`：5 秒 |
| 日志 | 只记状态变化，首次失败与之后每第 10 次失败，不刷屏（Agent24 会把子进程输出管道读走，别写满） |

上限取 10 秒而不是常见的 30 秒，是因为本机 UDS 连接几乎没有成本，不存在惊群，而 agent24d 被 launchd 重启后希望尽快重新握手；延迟上限就是退避上限。

**agent24d 拒绝时 iDoris 怎么做：继续服务，永不因为登记失败而退出。** `/health.registered` 为 false，数据面与 Admin 不受影响，按上表退避重试。唯一会让进程结束的是 launcher 显式给了 `orphan_timeout_s`（2.6）。

**版本协商：** `v` 必须是 1；服务端不支持就回 `unsupported_version` 并关闭。`contract_version` 是 HTTP wire 版本，服务端按它的 lock（主版本 1、最低次版本）判兼容。以后升 `v` 属于破坏性变更，必须走规范修订。

**会话规则：** 同一时刻 iDoris 只持有一个连接；服务端正常关闭（EOF）就立即重连；未知的服务端帧一律忽略（除 `challenge`、`ack`、`heartbeat_ack`、`error`、`set_admin_token`）。

### 2.6 孤儿策略、退出码、关机顺序

- **孤儿计时：** 从「没有已 `ack` 的登记会话」开始计时（包含进程启动后一直没登记上的情形），任何一次成功 `ack` 清零。超过 `orphan_timeout_s`（仅当 launcher 给了）就走优雅停机，退出码 4，日志 `idoris: 无启动器登记超过 N 秒，已停止`。这条是 Agent24 方案里没写的缺口：agent24d 被永久卸载或停掉后，孤儿 iDoris（连同它托管的 oMLX 和已加载的模型）会一直占着内存。建议 Agent24 取 900 秒。
- **退出码**（规范 §2.7 现为 0、1、3，新增两个，现有三个语义不变）：

| 码 | 含义 | launcher 行为建议 |
|---|---|---|
| 0 | 正常停止（SIGTERM、SIGINT） | 按预期 |
| 1 | 启动失败或停止失败（含 launcher 帧无效、发现文件写入失败） | 按崩溃处理，退避重启 |
| 3 | 同一数据库已有实例 | 去接管，不要重启 |
| 4（新） | 孤儿超时，主动停止 | 按预期停止，不计入崩溃熔断 |
| 5（新，仅当采纳决策 D4 的 B） | 自己托管的引擎无法恢复，自退让上层重启 | 计入崩溃统计，界面显示「模型引擎异常」 |

- **关机顺序**（总预算沿用 §2.7：iDoris 自己约 15 秒，即排空 10 秒加销毁 runtime 5 秒，`R/bin/idoris.rs:60,85`；launcher 17 秒后才 SIGKILL）：收到 SIGTERM 后并行执行：（1）向 agent24d 发 `bye`（≤ 0.5 秒）；（2）停止自己拥有的 oMLX（SIGTERM 整个进程组，8 秒宽限，之后 SIGKILL）；（3）HTTP 排空（沿用现有 10 秒上限）；然后（4）删发现文件（仅当 `instance_id` 匹配）；（5）销毁 runtime（Drop 会 SIGKILL 剩余进程组，`U/local_runtime_process.rs:393-399`，是最后一道兜底）；进程退出时内核释放实例锁。

### 2.7 ID-3：oMLX 生命周期

**范围与术语。** 「引擎」= oMLX 服务进程；「模型」= 引擎里的权重。现有 Supervisor 管模型，新增的「引擎管理器」在它之下，只管进程。`OmlxAdapter` 不变，继续用 HTTP 与引擎对话。

#### 2.7.1 配置形状与默认

只对**显式声明托管**的卡生效，现有部署行为不变。卡里用 `extensions`（卡契约认可的扩展点，同 `idoris_role_models`，必须带 `_degradation`，`P/card_validation.rs:15-28`）：

```yaml
extensions:
  _degradation: "未托管时退回外部 oMLX：iDoris 只探测，不拉起、不停止"
  idoris_managed: { engine: omlx, mode: auto }
```

**可执行文件路径、模型目录这类宿主相关信息不进卡**（`R/runtime_launch_config.rs` 的文件头注释就是这条原则：卡是可移植的契约）。可执行文件由运行期发现（2.7.3）或 stdin 帧的 `engine.omlx_executable` 给出。端口取卡的 `endpoint`，保持静态，这样 `ComponentCard.endpoint`、`BoundSupervisor.endpoint`、加载围栏路径（以 endpoint 为键）都不用动。

#### 2.7.2 启动决策（adopt 优先，缺失才拉起）

对每张托管卡，在创建 Supervisor 之前：

1. 探测 `endpoint`（`GET /v1/models`，带 `key.rs` 的 key；判据同 `setup omlx`，`R/setup_omlx.rs:159-175`，要求 `/v1/models/status` 与 `/api/status` 也是 oMLX 的形状）：
   - 200 且形状是 oMLX：外部引擎就绪，`owned=false`。**iDoris 永远不给它发信号，退出时也不停它。**
   - 401/403 且没有可用 key：`unavailable`，`reason:"auth_required"`（或 `auth_failed`）。**不再拉起第二个**。
   - 连接被拒：进入 2。
   - 其他（端口被非 oMLX 的服务占用，包括形状不符）：`failed`，`reason:"port_in_use"`。
2. 读所有权记录（2.7.5）：记录存在且进程身份全部吻合，则接管自己遗留的引擎（`owned=true`）；否则清理后继续。
3. 发现并校验可执行文件（2.7.3）：找不到 = `absent`，`reason:"engine_not_found"`，iDoris 本身继续服务，`/health` 给出原因，界面据此引导用户安装 oMLX。
4. 拉起（2.7.4），等就绪（探测同 1，默认 90 秒超时，轮询 250 毫秒起退避到 1 秒）。

#### 2.7.3 可执行文件的发现与校验

发现顺序：（1）stdin 帧 `engine.omlx_executable`（launcher 已校验，视为可信）；（2）`/Applications/oMLX.app/Contents/MacOS/omlx-cli`；（3）`~/Applications/oMLX.app/Contents/MacOS/omlx-cli`。**不使用 PATH 查找**：Agent24 启动 iDoris 时把 PATH 固定为 `/usr/bin:/bin:/usr/sbin:/sbin`（`A24: …/supervisor.rs:880-881`），Homebrew 路径本来就不在里面。`~/.omlx/bin/omlx`、`/opt/homebrew/bin/omlx`、`/usr/local/bin/omlx` 都只是指向 app 内 `omlx-cli` 的包装或符号链接（本机实测，见附录 A），不作为发现目标。

校验：`canonicalize` 后必须是普通可执行文件；位于 `.app` 内时用 `/usr/bin/codesign --verify --strict` 并要求 `TeamIdentifier == PSK5Q5T46L`（本机 oMLX 0.6.4 的签名团队，附录 A）。launcher 显式给的路径不再校验团队（launcher 已负责）。

模型目录：`~/Library/Application Support/oMLX/base-path` 若存在且可读，就把其内容作为 `OMLX_BASE_PATH` 传给引擎（`~/.omlx/bin/omlx` 包装脚本做的就是这件事，本机实测）；否则不传，引擎用默认 `~/.omlx/models`。

#### 2.7.4 启动命令（flags 已对照本机 oMLX 0.6.4 的 `omlx serve --help`）

```
可执行文件: <2.7.3 解析出的 omlx-cli 绝对路径>
argv:       serve --host 127.0.0.1 --port <卡 endpoint 的端口>
            [--memory-guard-gb <floor(IDORIS_ALLOCATED_BUDGET_BYTES / 1e9)>]   仅当设置了额度且 >= 1
环境:        env_clear，只放 PATH=/usr/bin:/bin:/usr/sbin:/sbin、HOME、TMPDIR、LANG、LC_ALL、LC_CTYPE、TZ，
            OMLX_API_KEY=<见 2.7.10>，OMLX_BASE_PATH=<见 2.7.3，若有>
cwd:        状态目录
stdio:      stdin=null；stdout 与 stderr 追加到 <IDORIS_STATE_DIR>/logs/omlx.log（超过 10 MiB 滚动，保留 3 份）
进程组:      process_group(0)，复用 ManagedRuntimeProcess 的停止与 Drop 杀组逻辑（U/local_runtime_process.rs）
```

关键点：
- **清洗环境**：iDoris 自己的 `IDORIS_*`（数据库路径、`IDORIS_OMLX_API_KEY` 等）不得泄给引擎子进程。
- **`--memory-guard-gb`**：`docs/13-…md` B-11 记录了「不开 memory guard，第二个模型载入会让 server 无响应」（oMLX issue #2290）；`--help` 说默认是 `balanced`，传 `--memory-guard-gb` 则把它改成自定义上限并开启。让 oMLX 自己的上限与 iDoris 的额度账本一致，是 iDoris 托管比外部引擎更稳的一个具体理由。单位是 GB 还是 GiB，`--help` 没说，取十进制整数 GB 向下取整（偏保守）。**额度为 0 时不拉起**（`reason:"budget_zero"`），因为什么都装不下。
- 不使用 `--api-key` 命令行参数（argv 同 UID 可见）。

#### 2.7.5 所有权记录与孤儿 oMLX（kill -9 iDoris 的场景）

iDoris 被 SIGKILL 时 `Drop` 不会运行，引擎成为孤儿，继续占着端口和内存。所以拉起成功后写所有权记录：`<IDORIS_STATE_DIR>/omlx-owned/<endpoint 编码>.json`（0600，目录 0700，写法同 2.3 的原子写）：

```json
{"schema":1,"pid":51234,"port":19731,"started_at":"Fri Nov 20 15:14:05 2026","executable":"/Applications/oMLX.app/Contents/MacOS/omlx-cli","api_key":"<引擎私有 key>","launched_by_instance":"3f0c6a52-…"}
```

`started_at` 是 `ps -o lstart= -p <pid>` 的原文，用 `ps` 子进程取，规避 workspace 的 `unsafe_code = "forbid"`（也可以加 `libproc` 依赖，Agent24 已在用；二选一，见风险 R5）。这与 Agent24 `omlx-kept.ts` 的做法一致：pid、启动时间、命令、监听端口四项全部吻合才算同一个进程，读不出来不算不吻合（不发信号，记录保留）。

新 iDoris 启动时对记录的处理：
- 四项吻合：接管（沿用记录里的 key，不重启引擎，已加载的模型继续在内存里；Supervisor 的「继承驻留」逻辑本来就处理引擎启动时已有模型，`B/supervisor.rs:1829-1851`）。
- 记录存在但进程不吻合（pid 被复用或已死）：丢弃记录。
- 吻合但存在 `load.pending` 围栏：引擎可能正处于加载中。**对自己拥有的引擎，安全的做法是杀掉整个进程组、确认已消失、清围栏、重新拉起**；这是 iDoris 托管比外部引擎多出来的一项能力，也是 Agent24 验收「kill -9 iDoris 后自动恢复」在加载中被杀时能过的前提。外部引擎的围栏保持原样，仍需人工（`README.md:54`），因为没有证据证明加载已停止。
- `stop_on_exit:"never"` 时，正常退出也保留引擎和记录，下一个 iDoris 接管（适合 iDoris 升级重启时不想重新加载大模型）。默认 `"owned"`：退出时停掉自己拉起的引擎，避免孤儿占内存。

#### 2.7.6 状态机、退避与就绪

状态与 `/health.upstreams[].state` 对应：`starting` → `ready`；`ready` 探测失败或进程退出 → `restarting`（退避 1、2、4 秒起翻倍，封顶 30 秒，±20% 抖动）；5 分钟内失败超过 5 次 → `failed`（`crash_loop`），之后每 60 秒半开重试一次，不永久放弃（与 Agent24 方案 §3.1 同一套参数，便于两层口径一致）；Admin 显式停止 → `stopped`；找不到引擎 → `absent`。

就绪探测：`GET /v1/models` 返回 200 且 JSON 里有 `data` 数组即可（空列表也算，模型目录可能是空的）。oMLX 没有 `/health`（`B/adapter.rs:28-37` 的模块注释）。

#### 2.7.7 与 Supervisor、围栏、`/health` 的关系（这是 ID-3 里最需要先定的部分）

问题：Supervisor 创建时采样一次引擎状态（发现 1），所以必须「引擎就绪之后才创建 Supervisor」；而引擎可能要 90 秒才好，期间 iDoris 要能答 `/health`。

做法：**可替换的服务体（swap）**。`bin/idoris.rs` 里数据面与 Admin 两个监听器照旧最先绑定，各自服务一个可原子替换的 `Router`（`RwLock<Arc<Router>>` 加一个薄 `tower::Service`，约百行，不必引入新 crate；`ConnectInfo` 随请求扩展透传，不受影响）：
1. 启动期服务「starting 体」：`/health` 返回 `phase:"starting"`，Admin `status` 可用（Agent24 的 `ready()` 要它，`A24: …/supervisor.rs:636-651`），其余路径 503 `idoris_starting` 并带 `Retry-After`。
2. 管理器按 2.7.2 把引擎带到就绪。
3. 用真实 `AppState`（此时才 `RuntimeRegistry::spawn…`）构建完整 Router 并 swap。Supervisor 对一个已就绪的引擎采样，不会再锁住启动错误。
4. 没有任何托管卡时，完全跳过 1 到 3，走现有路径，行为字节级不变。

**3a 与 3b 的边界。** 3a（v0.6.0）：上述启动门控加拉起、停止、重启、孤儿接管。3b（后移 v0.6.1）：运行期引擎换代（崩溃重启、外部引擎被用户重启）后 Supervisor 的账本已过期（它以为 `Ready` 的模型其实已不在引擎里）。「重建加 swap」的办法是再走一遍 2.7.2 到 3，把整个 `RuntimeRegistry`、`AppState` 重建并 swap，旧的在在途请求结束后自然释放；难点是旧 actor 还持有加载围栏的 `.owner.lock` flock（`B/load_fence.rs:26-48`），新 actor 无法重新 claim，需要给自己拥有的引擎的围栏路径加「代际」后缀。此外 `Supervisor` 没有 Shutdown/Join API（`B/supervisor.rs:386-389` 自述推迟）。**v0.6.0 的兜底（决策 D4 的 B）：自己拥有的引擎无法恢复时，iDoris 以退出码 5 自退，由 agent24d 重启，重启后走 3a 全新启动。** 外部引擎被用户重启这件事，今天就存在，3a 不引入新问题，只在文档里写明。

#### 2.7.8 停止、重启与控制面；与 #611、§2.9 下载的关系

语义：
- `stop`：向自己拥有的引擎进程组发 SIGTERM，8 秒宽限后 SIGKILL，状态 `stopped`；对外部引擎一律拒绝（409 `upstream_not_owned`），iDoris 不会去停别人的进程。
- `restart`：`stop` 后重新走 2.7.2；对外部引擎同样拒绝。
- `start`：从 `stopped` 或 `failed` 回到 2.7.2。

控制面（决策 D3 的 A，建议）：Admin 新增只读 `GET /admin/api/v1/upstreams` 与写接口 `POST /admin/api/v1/upstreams/{id}/start|stop|restart`，返回 202 与当前状态，鉴权同 Admin token，幂等。这要求规范把「Admin v0 只读」（规范 §2.1 L82）改为「Admin 有受限写接口」，与 [#611](https://github.com/iDoris-ai/iDoris/issues/611)（模型 load/unload，owner 已建议与 ID-3 一起定接口）和 §2.9 的下载（也是 iDoris 主动管理模型）**同一次修订**一起冻结。v0.6.0 只实现 `upstreams` 这一族，#611 与下载后移，但接口风格现在就按同一套定。

与预热的分工：`unload`、固定（pin）、显式 load 带策略属于运维操作（#611，Admin）；**预热**属于应用自己的流程（首次启动、切换模型），走数据面加数据 key（2.8.3），两者不重叠。

#### 2.7.9 `setup omlx` 的扩展（解除 IC-1 对「oMLX 必须在线」的依赖）

```
idoris setup omlx --out-dir <dir> [--endpoint <url> | --auto] [--managed] [--managed-port <n>]
                  [--bind role=model]… [--load-mode on_demand|resident] [--api-key-stdin] [--force] [--no-probe]
```
- `--auto`：只读探测候选端点 `127.0.0.1:{8088, 8000, 8001}`（Agent24 现在在自己代码里探测的同一组，`A24: …/ipc/index.ts:265`，移进 iDoris 后 Agent24 才能真正删掉所有 oMLX 地址）。找到 → 等同 `--endpoint <找到的>`（adopt，`managed` 缺省为否）；没找到 → 必须带 `--managed` 才继续，否则报 `unreachable`（保持现状）。
- `--managed`：写入 `extensions.idoris_managed`；端点取 `--managed-port`，缺省在 19000 到 19999 里挑一个此刻空闲的写进卡（私有端口，不去占 oMLX 官方默认的 8000，避免用户之后启动 oMLX.app 时端口冲突）。
- `--no-probe`（`--managed` 隐含）：不连引擎。`--bind role=model` 改为对照 `~/.omlx/models` 目录校验（`idoris models local` 已能离线读取并给出 `kind`，`R/recommendation.rs`），所以 `--bind embed=bge-m3-mlx-fp16` 在引擎没起时也能写。
- 新增错误码：`engine_not_installed`、`port_in_use`、`model_not_installed`；现有 11 个错误码语义不变（规范 §2.7「组件配置」行）。
- 输出 JSON 增加 `mode:"adopt"|"managed"`、`engine:{installed,executable,version?}`。
- `MANAGED` 白名单（`R/setup_omlx.rs:17`）加入 `omlx-embed.yaml`（2.8.1），`--force` 才能整体替换。

#### 2.7.10 引擎密钥（待实测）

自己拥有的引擎由 iDoris 生成每次拉起一个随机 key，经环境变量 `OMLX_API_KEY` 传给子进程（Agent24 已核对 oMLX 读这个环境变量，`A24: …/ipc/index.ts:819-824` 的注释，引用自 `oMLX.app/Contents/Resources/omlx/settings.py`），记进所有权记录（0600）。效果：**别的进程打这个引擎会得到 401**，才把「所有模型调用只经 iDoris」落成技术约束，而不是约定。实现上 `U/omlx/key.rs` 是唯一解析入口，加一个进程内「私有引擎 key 表」优先查它即可，但三个调用点（`omlx/mod.rs` 的 `for_base_url`、`proxy.rs:787-791`、`models.rs:187`）要把 URL 传下去（现在 `resolve_omlx_api_key_os` 只收一个 `loopback` 布尔）。

**未实测、需要先验证：** 环境变量 `OMLX_API_KEY` 与用户 `~/.omlx/settings.json` 里的 `auth.api_key` 同时存在时 oMLX 认哪个。若 settings.json 优先，私有 key 不生效。验证办法：在一个空闲端口临时起 `omlx serve`，设环境变量，另用一份 settings.json，分别带两个 key 请求 `/v1/models` 看谁被接受（不加载任何模型）。本次评估没有做：既定约束是不自己起推理进程，只用已经在跑的 oMLX。验证不通过时退回方案：沿用现有 key 顺序，失去隔离，其余设计不变（决策 D10）。

### 2.8 ID-4：embedding、自定义上游、预热

#### 2.8.1 `/v1/embeddings`

**v0.6.0 的做法（方案 E1）：伴生 resident 卡。** 路由、鉴权、透传、`local_only` 语义都已经有（1.4），缺的只是「默认 oMLX 卡走不通」。让 `setup omlx` 在写 `omlx.yaml` 的同时，再写一张指向**同一个 endpoint** 的 resident 卡 `omlx-embed.yaml`：

```yaml
provider: { id: omlx-embed, family: local, tier: local, capabilities: [embedding],
            privacy_class: local_only, cost: { input_per_m: 0, output_per_m: 0 }, locality: loopback }
form: http_service
endpoint: "<与 omlx.yaml 相同>"
version_pin: "omlx@0.6.4"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
extensions:
  _degradation: "未绑定 embed 角色时 /v1/embeddings 对此卡返回 503；其余能力不受影响"
  idoris_role_models: { embed: "bge-m3-mlx-fp16" }
```

这张卡就是 `build_card(…, LoadMode::Resident, …)` 现有逻辑的产物（`R/setup_omlx.rs:179-215`，resident 自动配 pinned 与 coexist），换个 `provider.id`、能力只留 embedding。`validate_registration` 只拒重复的 `provider.id`，同一 endpoint 两张卡是允许的（`P/registry.rs:185-200`）。

代码改动：
1. **密钥注入改按卡识别。** 现在只有 `provider_id == "omlx"` 才带 oMLX key（`R/proxy.rs:152,787-791`），改成「loopback 的 `omlx@` 卡」，判据同 `R/models.rs:92-114` 的 `is_loopback_omlx`；`outbound()` 现在只拿到 `provider_id`，要把卡或 `version_pin` 传进去。
2. `setup omlx` 写伴生卡，`MANAGED` 白名单加 `omlx-embed.yaml`（`R/setup_omlx.rs:17`）。
3. `lib.rs` 的 `embeddings` 处理函数不用改。

行为约定（写进规范）：
- 鉴权：与其他推理端点同一套虚拟 key。**请求写 `model:"idoris/embed"`，数据 key 的 `allowed_roles` 含 `embed`。** 写具体模型名（Agent24 现在的 `bge-m3-mlx-fp16`）会被当作无角色请求，要求全角色 key，否则 403 `role_forbidden`（`R/auth.rs:72-88`）。
- `local_only`：缺省即 `local_only`；只有可信本地卡（loopback、`local_only`、egress 只有 none 或 loopback）有资格；响应带 `X-iDoris-Served-Locality: loopback`，Agent24 的 `EmbeddingClient::idoris` 要求这个值（`A24: …/embedding.rs`）。远程或局域网卡必须 `X-iDoris-Privacy: any` 且 key 允许 `any`；付费卡直接 503 `paid_proxy_unavailable`（`R/lib.rs:876-883`）。
- 多张 embedding 卡：走现有的确定性选择管线，不做负载均衡。
- 已知限制，要在规范和发行说明里写明：（a）resident 路径绕过 Supervisor，embedding 模型的内存不在 iDoris 账本里，由 oMLX 自己的内存上限兜底（托管引擎有 `--memory-guard-gb`，2.7.4）；（b）冷加载 embedding 模型可能超过代理的 10 秒 header 超时（`R/proxy.rs:827`），所以首次启用向量召回前应先调预热（2.8.3），并建议给 `BufferedUpstreamPath::EMBEDDINGS` 单独放宽到 30 秒；（c）Agent24 一批最多 64 条（`A24: …/embedding.rs` 的 `EMBED_BATCH_LIMIT`），响应远小于 8 MiB 上限。

方案 E2（以后做）：给 `RuntimeAdapter` 加 `embed`，Supervisor 加 `Command::Embed`（带在途计数、驱逐感知、容量记账）。账本最准确，但要改 trait、`MockAdapter`、`OmlxAdapter`、`LocalHttpRuntimeAdapter` 和大量测试，量大。

#### 2.8.2 自定义 OpenAI 兼容上游（LM Studio 等）

**卡片形状：复用现有 component card，不新增 `Form`。** `Form` 与卡片 schema 是冻结契约（`C/component_card.rs:15-22`、`packages/contracts/schema/component-card.schema.json`），上游「种类」放在 `extensions` 里（卡契约认可的扩展点）：

```yaml
provider: { id: lmstudio, family: local, tier: local, capabilities: [chat, embedding],
            privacy_class: local_only, cost: { input_per_m: 0, output_per_m: 0 }, locality: loopback }
form: http_service
endpoint: "http://127.0.0.1:1234"
version_pin: "lmstudio@unknown"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
extensions:
  _degradation: "上游不可用时该卡不参与选择；不回退到其他卡"
  idoris_upstream: { kind: openai_compat, flavor: lmstudio }
```

新子命令（Agent24 不写 YAML，所以要有生成器）：

```
idoris setup upstream --id <slug> --endpoint <loopback url> [--capability chat,embedding,rerank]…
                      [--bind role=model]… --out-dir <已存在的组件目录> [--force]
idoris setup upstream --remove <slug> --out-dir <组件目录>
```

探测 `GET /v1/models`（OpenAI 形状，不要求 oMLX 专有接口）；在暂存目录用 `load_components` 校验**整个目录**（含新卡）后，只原子 rename 自己那一个文件，不动其他卡；错误码 `unreachable`、`not_openai_compatible`、`invalid_endpoint`、`id_conflict`、`invalid_role`、`model_not_found`、`validation_failed`、`write_failed`，输出 JSON 风格同 `setup omlx`。

边界（v0.6.x）：
- **只做 loopback、不带密钥。** 非 oMLX 的卡没有任何凭据注入（`R/proxy.rs:787-791` 只认 `omlx`；规范 §2.2 的 vault 注入「纯核心、未接线」，规范 L133）。需要密钥的上游和局域网、远程上游（隐私等级 `any`、egress 计数、`decorrelate` 脱敏，`R/proxy.rs:795-800`）后移。
- 探测器覆盖 resident 卡（2.4），界面能显示可达性。
- `GET /v1/models` 对自定义上游只有 `{id, owned_by}`，没有 `kind`；Agent24 内部的名称启发式保持 Agent24 自己的事（规范 §2.9「Agent24 的调用顺序」末句）。
- resident 路径原样转发请求体，所以 `max_tokens` 之类参数能通过（Supervisor 路径会拒绝，规范 L140）。

#### 2.8.3 预热 `POST /v1/models/warmup`

形状（HTTP、数据面、数据 key，不是 Admin）：

```
POST /v1/models/warmup
Authorization: Bearer idk_…
X-iDoris-Privacy: local_only            (缺省 local_only)
X-iDoris-Request-Id: <可选>
X-iDoris-Headroom-Breach: confirmed     (可选，只接受这一个值，语义同规范 §2.8)
Content-Type: application/json

{"model":"idoris/daily","timeout_ms":120000}
```

| 字段 | 说明 |
|---|---|
| `model` | 角色别名（`idoris/<role>`）或具体模型 id，选择与角色绑定逻辑同聊天 |
| `timeout_ms` | 1000 到 300000，缺省 120000；未知字段一律 400 `unsupported_field`（G-6「不静默」） |

行为：
- 鉴权与 `enforce_scope` 同聊天（`R/auth.rs:64-98`）：聊天在该模型上会被拒的 key，预热也被拒。
- 选卡同聊天。**生命周期卡（oMLX）**：复用聊天路径里加载前置那一段（`R/dispatch.rs:1011-1119`：`supervisor.list()` 取 catalog、正的加载前占用检查、`load_with_headroom`），抽成 `ensure_loaded` 共用，加载完成即返回，**不生成任何 token**。**resident 卡**：没有生命周期可管，按能力发一个最小的真实请求（chat：`max_tokens:1`；embedding：输入 `"warmup"`），受 `timeout_ms` 约束。
- 200 响应：`{"model":"<解析后的具体 id>","provider_id":"omlx","state":"ready","already_loaded":false,"load_ms":4210}`，带 `X-iDoris-Record-Id` 与 `X-iDoris-Served-Locality`。
- 错误沿用聊天的信封与代码（规范 §2.5、§2.8）：400 `invalid_json`/`unsupported_field`；401、403；404 `model_not_found`；503 `headroom_confirmation_required`（界面询问用户，用户确认后带 `X-iDoris-Headroom-Breach: confirmed` 重发）；503 `local_only_unavailable` 且 `reason_code:"supervisor_busy"`（有限退避后重试）；503 `local_only_unavailable` 其他（终态，例如超过整个额度）；**新增** 504 `warmup_timeout`（超过 `timeout_ms`，加载可能仍在引擎里继续，不等于失败，见风险 R2）。
- 副作用：不进用量与预算记录（本地免费）；配置了事件存储时写一条 `warmup` 审计事件；幂等；同一模型的并发预热由 Supervisor 现有的「同 id、同策略」合并处理（`B/adapter.rs:88-108`）。

与既有规范的关系：
- **与 §2.8 余量确认**：预热是用户在设置里选大模型时最自然的触发点，所以复用同一套 503 原因码与同一个请求头，Agent24 已有的 ID-HR 交互（Agent24#1455、#1457：按 `type`、`reason_code` 判定，不匹配文字）原样适用。
- **与 #611（Admin 写接口）**：`unload`、固定（pin）、带策略的显式 load 仍归 Admin（决策 D3）；预热是面向应用的数据面子集，不重叠。

**同一片里必须一起做的前置修复：把加载超时从一般管理调用超时里分离出来。** 现在 oMLX 的 `POST /v1/models/{id}/load` 用 `call_timeout`（默认 10 秒，`U/omlx/mod.rs:64,247-255`，`U/omlx/http.rs:90-100`），Supervisor 对 `load` 再包一层 `adapter_call_timeout`（默认 30 秒，`B/supervisor.rs:56,78`）；超时按「结果未知」处理，抛 `LoadUnconfirmed` 并留下 `load.pending` 围栏（`U/omlx/http.rs:132-139`）。Qwen3-8B 冷加载实测 3.8 秒（`spike/u0/U0-LOG.md:40`），而 §2.9 的档位表在 48 GiB 以上推荐 27B（权重 20.68 GB）：它的冷加载时间**我没有实测**，但很可能超过 10 秒。首次启动自动预热恰好撞上这个点。建议新增独立的 `load_timeout`，缺省 180 秒、可配置，只作用于 load 这一次调用，其他管理调用不变（规范 L261 的「目标」清单里本来就有「chat/load timeout 分离」）。

## 3. 风险与需要拍板的点

### 3.1 风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | Supervisor 启动错误粘滞；引擎换代后账本过期（0 的发现 1、2.7.7） | 引擎晚起或重启后，聊天长期 503；或以为已加载的模型其实不在引擎里 | 3a：引擎就绪后才创建 Supervisor；3b 或 v0.6.0 的退出码 5 兜底（D4） |
| R2 | 冷加载超过 10 秒或 30 秒，留下 `load.pending` 围栏（2.8.3） | 首次启动预热大模型后 iDoris「不可用」，且需人工恢复 | 分离 `load_timeout`；owned 引擎超时后可走「杀引擎、确认已死、清围栏、重启」（2.7.5） |
| R3 | 用户自己的 oMLX.app 与 iDoris 拉起的实例并存，双份内存 | 内存吃紧、加载失败 | adopt 优先；私有端口不占 8000；`--memory-guard-gb` 对齐额度（2.7.2、2.7.4、2.7.9） |
| R4 | 私有引擎 key 的优先级未实测（2.7.10） | 隔离不生效，或请求 401 | 先实测；失败退回现有 key 顺序（D10） |
| R5 | workspace `unsafe_code = "forbid"`（`Cargo.toml:106-107`），取进程启动时间、`getsockopt` 都不能裸调 libc | 所有权记录的 pid 复用防护 | 用 `ps -o lstart=` 子进程（Agent24 的 `omlx-kept.ts` 也这么做）；或加 `libproc`（Agent24 已用，MIT，需过 `cargo deny`，未核对）；`nix` 的 `LocalPeerCred`（client 侧 uid 校验，次要）是否可用，未核对，不可用就略过 |
| R6 | 热点文件冲突：`R/bin/idoris.rs`（1235 行）、`R/lib.rs`（6500 行）、`R/cli.rs` 几乎每片都碰 | 并行分支反复冲突 | 按依赖串行 S1、S2、S5；其余片限定在 `idoris-upstream`、`setup_omlx.rs`、`proxy.rs` |
| R7 | embedding 冷加载撞 10 秒 header 超时；resident 路径内存不入账本（2.8.1） | 首次向量召回偶发 504；内存超配由 oMLX 上限兜底 | 预热；`EMBEDDINGS` 路径放宽到 30 秒；E2 以后做 |
| R8 | Agent24 的 30 秒就绪窗口与引擎冷启动（2.4 两级就绪） | iDoris 被误判失败并重启，形成重启风暴 | 两级就绪；`starting` 阶段 `/health` 与 Admin 照常答 |
| R9 | oMLX 版本漂移：本机 0.6.4，调研文档提到 0.7.0 开发版（`mlx-serving-ecosystem-2026-09-19.md`）；`--memory-guard-gb` 单位未文档化；`FU-16` 记录过端点漂移 | 参数或端点变化后拉起失败 | 版本探测写进 `engine` 输出；flags 用 `--help` 实测；卡里 `version_pin` 目前只是信息，不做版本校验（现状），不要把它当兼容性保证 |
| R10 | `LOCAL_PEERTOKEN` 的最后写入者语义依赖内核行为（Agent24 R7、R8 已识别） | 无 iDoris 侧新增 | iDoris 遵守「收到 challenge 前不写」「fd 不继承」即可 |

### 3.2 需要 owner 或 Agent24 拍板的决策
### 3.2.0 已定（2026-11-20）

| 决策 | 结论 | 谁定 |
|---|---|---|
| D1 nonce | A：iDoris 生成 256 位，写进 0600 发现文件；登记用 HMAC 证明，nonce 不上线；`/health` 只返回公开的 `instance_id` | iDoris 提、Agent24 已同意 |
| D2 引擎来源与所有权 | D2a：adopt 优先、找不到才拉起、拉起的实例私有；D2b：v0.6.0 用户自装，iDoris 探测并报 `engine_not_found`，Agent24 界面引导安装（附带 / 按需下载以后再议） | owner |
| D3 控制面 | A：Admin 新增受限写接口 `upstreams/{id}/start\|stop\|restart`，与 #611、§2.9 下载同一次修订冻结 | owner |
| D4 运行期引擎崩溃 | B：v0.6.0 自有引擎无法恢复时 iDoris 以退出码 5 自退，由 agent24d 重启；热恢复（3b）放 v0.6.1 | iDoris（技术） |
| D5 孤儿超时 | B：launcher 在帧里给 `orphan_timeout_s`，Agent24 取 900 秒，超时退出码 4；手工运行不受影响 | owner |
| D6 embeddings | E1：伴生 resident 卡 | iDoris（技术） |
| D7 自定义上游 | A：loopback、不带密钥、resident；`setup upstream` 放 v0.6.1 | iDoris（范围随 D11） |
| D8 `/health.status` | A：`status` 仍是 `"ok"` = 活着，新增 `ready`、`phase` | iDoris（技术，兼容 Agent24 现有校验） |
| D9 stdin 帧 | A：新增 `--launcher-stdin`，保留 `--admin-token-stdin` | iDoris（技术） |
| D10 私有引擎 key | A：先实测环境变量与 settings.json 的优先级，通过则启用；不通过退回现有顺序并在规范写明 | iDoris（技术） |
| D11 发版节奏 | A：一次发 v0.6.0；动态端口与登记合入 main 后先给 Agent24 未签名构建联调高可用；签名公证只做一次 | owner |


每条给背景、性质（规则还是技术）、选项与推荐。

**D1 nonce 由谁生成，新 agent24d 怎么重认证（技术加安全规则，影响 Agent24 方案 §3.3 第 1、3 条）**
- 背景：闸门②。Agent24 方案让 agent24d 生成 nonce 经 stdin 交给 iDoris，同时又要 iDoris 把它写进发现文件。
- A（建议）iDoris 生成 256 位，写发现文件；登记用 HMAC 证明，nonce 不上线；admin token 用会话内 `set_admin_token` 轮换。取舍见 2.1 的表。
- B agent24d 生成并自己持久化，经 stdin 给 iDoris。多一份持久状态，且仍要写发现文件，实际两份。
- C 用 Keychain 带签名 ACL 的条目替代文件。最强，但要 Agent24 与 iDoris 都接入 Keychain，留到后续。

**D2 oMLX 的所有权与引擎从哪来（产品规则）**
- D2a 所有权：A（建议）adopt 优先，找不到才拉起，拉起的实例私有；B 总是拉起自己的私有实例（会和用户的 oMLX.app 并存、吃双份内存）；C 只 adopt 不拉起（不满足 ID-3）。
- D2b 引擎本身：（i，建议 v0.6.0）用户自装，iDoris 探测并报原因（`engine_not_found`），界面引导安装；（ii）Agent24 的 DMG 附带 oMLX.app（上游 Apache-2.0，调研文档记载；增大体积，要固定版本并处理升级，团队是 `PSK5Q5T46L` 的第三方签名，合并进 DMG 时要核对公证与嵌套签名）；（iii）iDoris 按需下载 pin 住版本与 sha256 的 oMLX 发行包（要新写下载与校验，与 §2.9 的下载同批更合适）。这条影响「用户无需任何操作」能否真的成立：只要引擎靠用户自装，首次体验里就有一步人工。

**D3 控制面：谁能让 iDoris 停止或重启引擎（规则，改规范「Admin 只读」）**
- A（建议）Admin 新增受限写接口 `upstreams/{id}/start|stop|restart`，与 #611、下载同一次修订冻结（2.7.8）。可用 curl 测，不依赖 UDS 是否在线。
- B 走 UDS 控制帧，只有 launcher 能控制，不动 Admin 的只读条款；但控制能力绑在登记状态上，登记失败时反而控制不了。
- C v0.6.0 不提供，「重启引擎」等于重启 iDoris。最省事，但 Agent24 桌面端「停止、重启 oMLX」按钮没有对应能力。

**D4 运行期引擎崩溃怎么办（技术，影响 v0.6.0 范围）**
- A 在 v0.6.0 就做 3b-light（重建加 swap，含围栏代际），约再加 2 天，风险集中在旧 actor 释放与围栏交接。
- B（建议）v0.6.0 先让自己拥有的引擎无法恢复时 iDoris 以退出码 5 自退，由 agent24d 重启走 3a；3b 放 v0.6.1。代价是在途请求被切断、且引擎崩溃被计入 iDoris 的崩溃统计（靠退出码 5 区分）。
- C 都不做，只写文档。外部引擎被重启后账本过期的问题仍在（今天就存在）。

**D5 孤儿 iDoris 多久自己退出（规则）**
- A 不设，永远等 agent24d。孤儿会连同引擎和已加载的模型一直占内存。
- B（建议）launcher 在帧里给 `orphan_timeout_s`，Agent24 取 900 秒，超时退出码 4。
- C iDoris 内置默认 600 秒。会影响没有 launcher 的手工运行，需要额外开关。

**D6 embeddings 在 oMLX 上怎么实现（技术）**：E1 伴生 resident 卡（建议 v0.6.0，量小，代价是 embedding 内存不入账本）；E2 生命周期原生（准确，量大，v0.7 以后）。

**D7 自定义上游 v0.6.x 做到哪（规则）**：A（建议）loopback、不带密钥、resident；B 再加密钥（经 stdin 帧或环境变量）；C 再加局域网、远程（隐私等级 `any`、egress 计数、同意流程）。

**D8 `/health` 的 `status` 语义（规则）**：A（建议）`status` 维持 `"ok"` = 活着，新增 `ready`、`phase`；B 把 `status` 改成枚举。B 会破坏 Agent24 的 `validate_health`（`status != "ok"` 即失败）和规范 §2.7「就绪」行。

**D9 stdin 帧形式（技术）**：A（建议）新增 `--launcher-stdin`，保留 `--admin-token-stdin`；B 直接替换旧帧（破坏 ADR-035 与现有测试）；C 用 argv 或环境变量传路径（`ps` 可见或被子进程继承）。

**D10 私有引擎 key（技术，待实测）**：A（建议）先实测环境变量与 settings.json 的优先级，通过则启用；B 沿用现有 key 顺序，不做隔离，「所有模型调用只经 iDoris」只靠约定而不靠技术约束。

**D11 发版节奏（规则）**：A（建议）一次发 v0.6.0，S1、S2 合入 main 后（约第 3 到 4 天）Agent24 用 `workflow_dispatch` 的未签名构建先联调 IC-3，签名与公证只在最终发版做一次；B 拆两版，v0.6.0 只含 ID-1、2、5，其余进 v0.7.0，多一轮签名公证与 Agent24 pin 更新；C 一次发齐，再加自定义上游与 3b，约再加 3 天。

### 3.3 要向 Agent24 确认的问题

- **Q1** 登记方允许清单是否加入标识符 `ai.idoris.cli`（Team `HYAP444FN9`）；开发构建怎么处理。
- **Q2** 是否接受 nonce 由 iDoris 生成（D1 的 A），并同意发现文件里字段名用 `started_at_ms`（毫秒整数）而不是方案里的 `started_at`。
- **Q3** 接管后新 agent24d 通过 `set_admin_token` 设新 token 之后，Electron main 如何拿到新 token。
- **Q4** 是否接受两级就绪（2.4）：`status=="ok"` 加 Admin 用于「iDoris 可用」，`phase=="ready"` 用于「模型可用」，后者允许 `starting` 到 90 秒。方案 §3.2 把「至少一个上游可用」放进了就绪，且请求最多等 30 秒（§3.4）。
- **Q5** 孤儿超时取值，以及退出码 4、5 在 agent24d 的崩溃熔断里怎么计（4 不计，5 计）。
- **Q6** 嵌入请求是否改发 `model:"idoris/embed"` 并让数据 key 含 `embed`；还是给全角色 key。
- **Q7** Agent24 的 `omlx_probe`、桌面端探测是否全部迁到 `idoris setup omlx --auto`，这样 Agent24 里才真的不再出现任何 oMLX 地址。

## 4. 拆分与排期

估算口径：一片 = 一个 Sonnet 子代理开发加一轮 PR-Daemon 评审，S 约半天，M 约一天，L 约 2 到 3 天；评审有修改要求时各加半天。以下是我的估计，没有按实际吞吐校准。

| 片 | 目标 | 主要文件 | 主要测试 | 量 | 依赖 |
|---|---|---|---|---|---|
| S0 | 规范修订 1.6 草案 PR：§2.7 新增行、新 §2.10 登记协议、wire 1.3.0 节；发 Agent24 评审一轮后冻结 | `docs/interfaces/…规范.md` | 无 | S | 无，最先做 |
| S1 | ID-1：端口 0；`--launcher-stdin` 帧；发现文件（校验、原子写、自愈、清理）；`instance_nonce`；`/health` 新字段与上游探测器 v0（`ready/phase/upstreams` 先基于 Supervisor 状态与 resident 探测） | `R/cli.rs`、`R/bin/idoris.rs`、`R/lib.rs`（`parse_port`、`HealthResponse`）、`R/admin.rs`（`parse`、token 可替换）、新 `R/discovery.rs`、`R/launcher_frame.rs`、`R/prober.rs` | 新 `tests/launcher_discovery.rs`：端口 0 起真二进制、发现文件内容与权限、读者并发读不到半截文件、kill -9 后残留可识别、再起覆盖陈旧文件、第二实例退出码 3 且不碰文件、帧错误不回显 token、符号链接与目录权限拒绝 | M+（约 1.5 天） | S0 草案 |
| S2 | ID-2：UDS 客户端（握手、HMAC、心跳、退避、`bye`、发现文件自愈、孤儿计时、`set_admin_token`） | 新 `R/register.rs`，`R/bin/idoris.rs` | 新 `tests/register_uds.rs` 带一个假 agent24d（Rust 测试服务端）：握手与测试向量、心跳丢失重连、服务端重启后重新握手、`proof` 不符、服务端先发前不写字节、退避序列、拒绝时继续服务、孤儿超时退出码 4、UDS fd 不被子进程继承 | M（1 到 1.5 天） | S1 |
| S3 | ID-3 引擎原语：可执行文件发现与签名校验、`EngineLaunch` 与 spawn（清洗环境、日志、进程组；`ManagedRuntimeProcess::spawn_command` 现为私有且只收 `LocalRuntimeLaunch`，要加引擎用的构造器，并支持关掉 Drop 杀组以实现 `stop_on_exit:"never"`）、所有权记录、停止 | `U/omlx/engine.rs`（新）、`U/local_runtime_process.rs`、状态目录模块 | 用假 `omlx` 脚本和假 HTTP 引擎：spawn 与就绪、SIGTERM 与 SIGKILL 回收、孤儿记录接管与四项身份校验、pid 复用不误杀、环境不泄露 `IDORIS_*` | M（1 天） | 无，可与 S1、S2 并行（文件不重叠） |
| S4 | `setup omlx` 扩展：`--auto`、`--managed`、`--no-probe`、离线绑定校验、伴生 embed 卡、新错误码 | `R/cli.rs`、`R/setup_omlx.rs` | 扩展 `tests/setup_omlx.rs` | S 到 M（0.5 到 1 天） | S1 合入后做（同碰 `cli.rs`） |
| S5 | ID-3 管理器：状态机、退避、熔断、探测；启动门控加可替换服务体（swap）；`/health.upstreams` 真实数据；owned 引擎清围栏；私有 key 表（D10）；退出码 5 兜底（D4 的 B） | 新 `R/lifecycle/`、`R/bin/idoris.rs`、`R/lib.rs`、`U/omlx/key.rs`、`U/omlx/mod.rs`、`B/load_fence.rs`（路径助手） | 新 `tests/lifecycle_managed.rs`：引擎不在时拉起并就绪、先答 `/health` 后切换、外部引擎 adopt 不发信号、kill -9 iDoris 后重启接管、加载中被杀后清围栏重拉、崩溃循环熔断、额度为 0 不拉起 | L（2.5 到 3 天） | S1、S3 |
| S6 | Admin `GET upstreams` 与 `POST start/stop/restart`（取决于 D3） | `R/admin.rs`、`C/admin_v0.rs` 加 schema、TS 契约 | 扩展 Admin 测试 | M（1 天） | S5 |
| S7 | ID-4 embeddings：伴生卡、按卡识别 oMLX key、embed 路径超时、角色绑定测试 | `R/proxy.rs`、`R/models.rs`、`R/setup_omlx.rs` | 扩展 `embeddings_wiring_tests.rs`、`resident_omlx_key.rs`（伴生卡带 key、非 oMLX 卡不带） | S 到 M（0.75 天） | 无，可并行；与 S4 同碰 `setup_omlx.rs`，排在 S4 前后均可 |
| S8 | ID-4 预热：`POST /v1/models/warmup`；抽取 `ensure_loaded`；独立 `load_timeout` | `R/lib.rs`（路由）、`R/dispatch.rs`、`U/omlx/mod.rs`、`B/supervisor.rs`（load 超时参数） | 新 `tests/warmup.rs`：头与错误码矩阵（含余量确认）、key 范围、resident 卡最小请求、并发合并、超时不留围栏的路径 | M（1.5 天） | S5 之后更稳；oMLX 路径可先对 mock Supervisor 测 |
| S9 | 自定义上游：`setup upstream`（含 `--remove`） | `R/cli.rs`、新 `R/setup_upstream.rs` | 新 `tests/setup_upstream.rs` | M（1 天） | S5 的探测框架 |
| S10 | 3b：引擎代际变化后重建加 swap（含围栏代际、旧 actor 释放） | `R/lifecycle/`、`B/supervisor.rs`（Shutdown/Join） | 外部与自有引擎重启后聊天恢复 | M 到 L（2 天） | S5 |
| S11 | 发版：版本号 0.6.0、`CONTRACT_VERSION` 1.3.0 四处、发行说明、conformance 加 `/health` 新字段形状、`pnpm check:contract-drift`；签名公证由 operator 做；Agent24 更新 pin 与 lock 的 `files` | `Cargo.toml`、`C/version.rs`、`packages/contracts/src/version.ts`、`docs/releases/v0.6.0.md` | 现有 CI；`release-smoke.py` | S（0.5 天） | 前面各片 |

**v0.6.0 建议范围：** S0、S1、S2、S3、S4、S5、S7、S8、S11；S6 取决于 D3（采纳 A 则进）。

**后移 v0.6.1：** S9（自定义上游，Agent24 IC-4 的「模型来源页」要等它，其余 IC-4 不受影响）、S10（3b）、#611 的 Admin load/unload、§2.9 的下载。

**时间：** 单线串行约 11 到 12 个工作日；两条并行线（核心线 S0、S1、S2、S5、S6、S8、S11；引擎与上游线 S3、S7，S4 在 S1 合入后插入）约 8 到 10 个工作日，加评审返工与 operator 的签名公证，日历上约两周。如果只要解锁 IC-3，S0、S1、S2 合计约 3.25 天，合入 main 后即可用未签名构建联调。采纳 D11 的 C 约再加 3 天。

### 4.1 Agent24 发版验收四项（`IDORIS-CORE-PLAN.md` §6）在 iDoris 侧怎么对应

| Agent24 验收项 | iDoris 侧行为 | 前提与注意 |
|---|---|---|
| 抓包确认除 iDoris 外没有进程连接 oMLX | 自有引擎：每次拉起带私有 key，别的进程得到 401（D10 实测通过后）。外部引擎：iDoris 无法阻止别的进程去连，只能保证 Agent24 自己的代码不连 | Agent24 先删 `omlx_probe`、桌面端探测和 B1 至 B9；抓包要排除用户自己装的其他客户端 |
| `kill -9` iDoris 后自动恢复 | 空闲时：实例锁由内核释放，陈旧发现文件被新实例覆盖，agent24d 重启即可。有自有引擎：新 iDoris 按所有权记录接管仍在跑的引擎，已加载的模型不丢。**加载进行中被杀：** 自有引擎杀掉、清围栏、重拉；外部引擎的围栏仍需人工（R2） | 依赖 S3、S5；验收脚本要包含「加载中被杀」才能暴露围栏问题 |
| `kill -STOP` 模拟卡死后自动恢复 | iDoris 无需配合：`/health` 超时，agent24d 发 SIGTERM，17 秒后 SIGKILL（SIGKILL 对已停止的进程有效）。STOP 期间发现文件还在、登记连接静默：**活性只看 `/health`，心跳只用来发现连接断开并重新握手**，与方案 §3.2 一致 | 心跳不能当活性判据，否则 STOP 的进程与「agent24d 自己卡住」分不清 |
| 重启 agent24d 后接管孤儿 iDoris，不出现第二个实例 | 第二个实例必然被实例锁挡下并退出码 3，且不碰发现文件；iDoris 在连接断开后按 0.25 到 10 秒退避重连，新 agent24d 读发现文件、核对 `/health`、等登记（通常在 10 秒内）后接管 | 发现文件 `launch` 里的额度、组件目录与新 agent24d 想要的不一致时，应重启而不是接管 |

## 5. 规范修订提纲（修订 1.6 与 wire 1.3.0；不写全文）

| 位置 | 改动 |
|---|---|
| 文档头与 §0 表 | 新增「修订 1.6」说明；注明 Agent24 方案里的「§2.7 修订 1.4」已被 §2.9 占用；§0 的「文档修订」行与 wire 版本说明更新 |
| §2.1 | L68「`IDORIS_URL` 必须显式配置，无隐含端口」改为：由 launcher 经发现文件与登记取得动态端口；凭证表新增 `instance_nonce` 一行（来源、不进 `/health`、不明文上线）；L82「Admin v0 只读」若采纳 D3 的 A，改为「Admin 有受限写接口」，并说明会话内 token 轮换 |
| §2.5 L138 | `/health` 字段描述更新；明确 `status` 仍是「活着」，`ready` 才是「模型可用」 |
| §2.7 表（标题保留「修订 1.3」历史，另加「修订 1.6 增补」） | 「启动」行：新增 `--launcher-stdin` 与帧字段表，保留 `--admin-token-stdin`；「就绪」行：两级就绪；「停止」行：自有引擎一并停止，17 秒预算内的分配；「退出码」行：新增 4、5；「布局」行：随包仍为 5 个文件，伴生卡等由 `setup` 生成；新增行「发现文件」「登记」「动态端口」「oMLX 生命周期」「引擎密钥」；「不承诺」行更新 |
| 新 §2.10 登记协议 v1 | 传输、帧、时序、HMAC、计时与退避、错误码、版本协商、测试向量（附录 B） |
| 新 §2.11 wire 契约 1.3.0 | 只有新增：`/health` 新字段；`POST /v1/models/warmup`；`/v1/embeddings` 的 `idoris/embed` 与 key 范围说明；若采纳 D3，Admin `upstreams` |
| §2.2、§2.3 | `embed` 角色与伴生卡；`idoris_managed`、`idoris_upstream` 两个 `extensions` 键；§2.3 L106 的「下载、预热归属」改为「预热已实现，下载仍未」 |
| §2.8 | 预热与余量确认的关系一段 |
| §4.2 L298 | 「不存在的目标工件」清单：`/health` 仍无 schema，靠 conformance 锁形状 |
| §7 | 新增 R12 行：2026-11，IDORIS-CORE，双方确认状态 |
| 其他文件 | `README.md`（已知限制、进程生命周期）、`docs/安装与首次调用-v0.6.0.md`、`docs/releases/v0.6.0.md`、`conformance/tests/response-headers.test.ts`、`packages/router/tests/health-identity.test.ts`、TS 与 Rust 的 `CONTRACT_VERSION` |

## 附录 A：本机实测（只读，没有启动任何引擎或推理进程）

- oMLX.app：`/Applications/oMLX.app`，`CFBundleIdentifier=app.omlx`，版本 0.6.4；签名 `Developer ID Application: Heejun Kim`，`TeamIdentifier=PSK5Q5T46L`，标识符 `app.omlx`。
- `/Applications/oMLX.app/Contents/MacOS/omlx-cli` 是 POSIX shell 脚本：设置 `PYTHONHOME`、`PYTHONPATH` 后 `exec` 内置的 cpython-3.11 运行 `python -m omlx.cli "$@"`，所以拉起后只有一个进程（进程标题被改成 `omlx-server`）。
- `~/.omlx/bin/omlx` 是 304 字节的包装脚本：若存在 `~/Library/Application Support/oMLX/base-path` 就把内容作为 `OMLX_BASE_PATH`，再 `exec` 上面的 `omlx-cli`。`/opt/homebrew/bin/omlx` 指向它，`/usr/local/bin/omlx` 指向 `omlx-cli`。
- `omlx serve --help`（0.6.4）确认的参数：`--model-dir`（缺省 `~/.omlx/models`）、`--host`（缺省 127.0.0.1）、`--port`（缺省 8000）、`--memory-guard {off,safe,balanced,aggressive}`（缺省 balanced）、`--memory-guard-gb`（自定义上限并开启）、`--base-path`、`--api-key`、`--max-concurrent-requests`（缺省 8）、`--embedding-batch-size`（缺省 32）、`--paged-ssd-cache-dir` 等。
- 本机正在运行的 oMLX 实例监听 `127.0.0.1:8088`，父进程是 launchd（PPID 1），说明是 app 或服务拉起的，不是某个终端的子进程；`~/.omlx/settings.json` 权限 0600；`~/.omlx/models` 里有 Agent24 默认的 embedding 模型 `bge-m3-mlx-fp16`。
- 没有验证：环境变量 `OMLX_API_KEY` 与 settings.json 的优先级；27B 模型冷加载耗时；`nix` 的 `LocalPeerCred` 是否可用；`libproc` 是否能过 `cargo deny`。
- 本稿所有「iDoris 已有」都是静态阅读源码得出的，没有编译，也没有运行 iDoris 的测试；「embeddings 在默认安装下 503」「`/health` 不报上游状态」这类行为结论来自代码路径与既有测试名，不是实际请求的结果。

## 附录 B：登记协议测试向量

下面 transcript 里的 `\n` 表示 0x0A 一个字节，不是反斜杠加 n。输入（均为 ASCII）：
```
instance_nonce  = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f   (hex 解码后 32 字节作为 HMAC key)
conn_id         = 0123456789abcdef
challenge nonce = 00112233445566778899aabbccddeeff
instance_id     = 3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11
pid=41234  data_port=51234  admin_port=51235  started_at_ms=1763625330123
session_id      = a1b2c3d4e5f60718293a4b5c6d7e8f90
```
`register` 的 transcript（137 字节，`\n` 分隔，末尾无换行）：
```
idoris-register/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\n3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\n41234\n51234\n51235\n1763625330123
```
`proof` = `386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884`

`ack` 的 transcript（96 字节）：
```
idoris-ack/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\na1b2c3d4e5f60718293a4b5c6d7e8f90
```
`proof` = `099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967`

（用 Python `hmac.new(bytes.fromhex(nonce), transcript, hashlib.sha256).hexdigest()` 计算，两端实现都应得到同样的值。）
