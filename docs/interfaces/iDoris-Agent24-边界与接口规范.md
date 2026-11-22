# iDoris × Agent24 分工边界与接口数据规范

> **文档修订 1.6（已冻结，wire 1.3.0，2026-11-21；Agent24 已评审并同意）**：Agent24 的 IDORIS-CORE（[Agent24#1937](https://github.com/iDoris-ai/Agent24/pull/1937)）要求 iDoris 成为 Agent24 的必选核心：动态端口与发现、登记与心跳、由 iDoris 托管本地推理引擎（首个实现是 oMLX）、embeddings 与预热。owner 同日追加：契约按多引擎设计，v0.6.0 只实现 oMLX。本修订新增 §2.10（登记协议 v1，含冻结时补入的 §2.10.10 口径）、§2.11（wire 契约 1.3.0，只新增）与 §2.12（多引擎，预留），增补 §2.1、§2.2、§2.3、§2.5、§2.7、§2.8、§2.9、§4.2，协商记录见 R12。依据是 iDoris 侧评估与接口提案（[PR #614](https://github.com/iDoris-ai/iDoris/pull/614)，下称设计稿）和 Agent24 2026-11-20 对设计稿 §3.3 七个问题的答复。**本修订已冻结（2026-11-21，Agent24 已同意）；冻结的是契约，不表示 iDoris 已实现，实现随 v0.6.0 发版；§2.12 中标「预留」的部分除外，待后续修订冻结。本修订不改变任何更早冻结条款的承诺，增补处逐处标注，被取代的句子保留原文并注明。** 修订号说明：Agent24 方案里的「§2.7 修订 1.4」已被 §2.9（修订 1.4）占用，故本次记为修订 1.6。
>
> **文档修订 1.6 勘误（不改 wire，2026-11-21）**：补入 PR #617（oMLX 配方）带出的事实，只增不改契约：§2.7.6 自有 base 的目录名与 `lstat` 核对方式、`--model-dir` 的空 `models/` 与逗号约束、落盘设置跨启动生效、`HOME` 下 `~/.omlx/bin/omlx-cluster-python` 的写入为已知行为；§2.7.7 的 D10 静态结论；§2.7.5 退出码 3、4 的排空失败口径；§2.10.5 `error` 帧不带认证。
>
> **文档修订 1.6 勘误二（S4 验收定案，不改 wire，2026-11-21）**：§2.7.6（3）托管引擎的模型目录取 iDoris 运行用户的 `~/.omlx/models` 并显式传 `--model-dir`（取代勘误一里「不给就用自有 base 下的默认模型目录」的口径），`setup` 对该路径校验绝对路径与逗号；§2.7.8 `--auto` 与 `--api-key-stdin` 互斥，#619 落地之后可放宽；`--auto --managed` 找到外部 oMLX 就采纳，已确认。
>
> **文档修订 1.6 勘误三（`setup upstream` 最小版回填，不改 wire，2026-11-21）**：§2.7.8 本来写明「以 v0.6.0 发版时的实现为准并回填」的几处，按实现写进 `setup upstream` 一节：`--out-dir` 是已有的组件目录（加一张卡、不动别的文件）、`--id` 的格式与缺省值、`--force` 的替换范围、`--default` 的卡面效果、`--bind` 接受的角色、输出 JSON 与新增错误码；另记一处与卡契约的出入：回环主机的写法。不改任何已冻结字段，不改 wire。
>
> **文档修订 1.6 勘误四（S7 伴生卡与 oMLX key 识别回填，不改 wire，2026-11-21）**：（1）§2.3、§2.7.8：伴生卡 `omlx-embed.yaml` 只在 `--bind embed=<模型>` 给出时生成（没给就不生成；原文「未绑定时这张卡 fail-closed 503」只剩「没有这张卡就没有 embeddings 候选，503」一种情形）；`embed` 绑定只出现在伴生卡上，主卡的 `idoris_role_models` 不含它；`--force` 在这次没给 `embed` 时会把上次生成的 `omlx-embed.yaml` 一并删掉；输出 JSON 只增不改，生成了伴生卡才有 `companion_card_file`。（2）§2.3 修订 1.6 增补（1）：key 的注入按卡识别，另加一条排除：卡带 `extensions.idoris_upstream`（自定义上游）的，即使 `version_pin`、`flavor`、端口、`provider.id` 都写成 oMLX，也一律不带 key。（3）§2.11.3：`/v1/embeddings` 的响应头超时放宽到 30 秒已实现，聊天、rerank、messages 仍是 10 秒。
>
> **文档修订 1.6 勘误五（`setup upstream` 接受 `--bind embed=`，S7 追加，不改 wire，2026-11-21）**：Agent24 IC-5 要求。§2.7.8 `setup upstream` 的 `--bind` 在 chat 角色之外接受 `embed`：生成的仍是一张 resident 卡，能力 `[chat, embedding]`（单数；复数 `embeddings` 会让 serve 退出 1），`idoris_role_models.embed` 绑定该模型，与 Agent24 在 v0.5.0 上手写并实测过的 LM Studio 卡一致；至少要有一个 chat 角色，`rerank`、`vision` 仍拒绝，`embed` 的模型也必须在 `/v1/models` 的列表里。这张卡带 `idoris_upstream`，所以不带 oMLX 的 key（勘误四）。数据 key 的 `allowed_roles` 含 `embed` 即可用 `model:"idoris/embed"` 请求。
>
> **文档修订 1.6 勘误六（S5 引擎管理器回填，不改 wire，2026-11-21）**：把 S5 实现时定下的口径写进正文，只增不改契约：（1）§2.7.6「待定」已回填：启动门控未完成期间，数据面除 `/health` 外对**所有**卡一律 503 `idoris_starting`，含非托管的 resident 卡（完整的 Router 是一次整体替换）；门控在**所有**托管引擎都「落定」时放行（落定 = `ready`，或不会自己好的终局；`starting`、`restarting` 不算；等待有上限，到点放弃），见下（8）至（10）；放行之前 `/health.phase` 是 `starting`（仍在进行），任一托管上游已处于终态（`absent`、`failed`、`unavailable`、`stopped`）时是 `degraded`；（2）退出码 5 的触发条件已回填：门控完成之后自有引擎的**进程退出**立即触发（不等退避）；进程活着而探测失败只报 `unavailable`，不杀它也不退出；门控完成之前的失败走重启与熔断，不是退出码 5；退出码 5 与 3、4 一样，HTTP 排空失败时仍以 5 退出；引擎实例任务自己 panic（实现的缺陷）同样按引擎失效处理，也是退出码 5，不会无声地卡在 `starting`（启动门控任务 panic 则按启动失败，退出码 1）；（3）oMLX 配方每次拉起都传 `--model-dir` 与内存护栏（有额度传 `--memory-guard-gb`，**没有额度传 `--memory-guard balanced`**，即 oMLX 自己的缺省档位：落盘的设置是本次的意图，不沿用旧值），取代勘误一、本节「没有每次都传的保证」的说法；`--model-dir` 在拉起前再取一次 S4 的 `managed_models_dir()`，含逗号的路径一律拒绝；（4）端口属主校验（[#619](https://github.com/iDoris-ai/iDoris/issues/619)）落地，范围是引擎管理器与 `setup`：向端口发私有 key 之前，先确认监听它的进程就是我们拉起的那个引擎（pid 在该端口上监听）；接管外部引擎前，要求监听者是同一个用户、签名团队对（oMLX：`PSK5Q5T46L`），不符则不发 key、不接管，报 `port_in_use`；带 key 的请求用不保留空闲连接的客户端，只走校验之后新开的连接。非托管卡的数据路径（代理、适配器、模型列表对静态配置的 `settings.json` key）仍是 v0.5.0 的行为，不在本次范围；（5）§2.7.8 `--auto` 与 `--api-key-stdin` 的互斥放宽：候选要鉴权时先过属主校验，通过才发 key；（6）同一种引擎只托管一个实例（它们会共用同一个私有 base 目录），第二张同种托管卡是卡校验失败；（7）接管外部引擎时 `load.pending` 围栏还在（上一个 iDoris 在加载中被停）：围栏原样保留、引擎不发信号，但**不当作就绪**：该上游是 `unavailable` + `reason:"fence_pending"`（Supervisor 在围栏存在时会把启动错误粘滞住，把它当成就绪只会得到一张一直 503 的卡）；它是终局的失败，门控不被它拖住，降级放行（下（8）至（10））；人工确认加载已经停止并删除围栏之后，重启 iDoris 再试。（8）**启动门控不被一个起不来的托管引擎无限期拖住**（取代上面（1）与 §2.7.6 正文里「所有托管引擎都 `ready` 才放行」的口径）：门控在所有托管引擎都**落定**时放行。落定是 `ready`，或不会自己好的终局：配置性失败（`budget_zero`、`base_unavailable`、模型目录取不到、`fence_pending`、`port_in_use`）、`absent` + `engine_not_found`、要 key 而没有（`auth_required`、`auth_failed`）、`failed` + `crash_loop`（熔断已经触发）；`starting`、`restarting` 还在往前走，不算。**等待有上限**：到了管理器启动之后一个启动超时（`Timings::startup_timeout`，oMLX 缺省 90 秒）还没落定的引擎，**到点就算它落定**：放弃它（停掉半拉起的引擎，`failed` + `startup_timeout`，不再重试），然后放行。这样选，是因为等熔断的上限太大：熔断要攒够 6 次失败，每次失败最长是一个启动超时，最坏近 10 分钟，这段时间里同一进程里健康的 resident 卡、自定义上游也都在 503（与 §2.7.6「`engine_not_found` 时 iDoris 本身继续服务」相悖）；（9）**降级放行**：有托管引擎没就绪而放行时，完整的 Router 照常服务别的卡；没就绪的托管卡不建 Supervisor；路由到它的请求（chat、messages、embeddings、rerank）**不发给引擎**，直接 503，`error.type` 是这条路径上已有的 `local_only_unavailable`，`error.reason_code` 是该上游在 `/health` 里的 `reason`（`engine_not_found`、`budget_zero`、`crash_loop`、`startup_timeout` 等都是已有的原因码，**不新增错误码**）；没有稳定原因码的失败（例如模型目录取不到）沿用这条路径上已有的 `supervisor_unavailable`。`/health.phase` 在门控放行之后，有托管上游处于终态就是 `degraded`，哪怕别的上游都在服务（`ready` 要求没有托管上游处于终态），`upstreams[]` 是真实的状态与原因；（10）**降级放行之后冻结**（v0.6.0 的保守做法，**已知限制**）：放行时没就绪的托管引擎不再半开重试，也不再接受 `start`、`restart`（管理器报 `RestartRequired`，S6 的 Admin 据此告诉用户「要重启 iDoris」），恢复要重启 iDoris（agent24d，或 S6 之后的 Admin 重启）：Supervisor 表是放行时整体创建的，之后引擎好了也读不到，运行期引擎换代（热恢复）是 v0.6.1 / S10 的事。退出码 5 的语义不变：只有放行时已经就绪的自有引擎，之后进程退出才触发；冻结的实例没有进程；**（勘误十（2）收窄了这一条的适用：放行时就绪的上游不冻结，它们的 `start`、`restart` 经 Supervisor 的原地复位照常可用；仍然冻结的只有放行时没就绪的上游）**；（11）没有额度时传 `--memory-guard balanced`（上面（3））：已确认保留；活着但永远不答的自有引擎不转成退出码 5，仍只报 `unavailable`（上面（2））；（12）每种引擎一个实例：v0.6.0 没有出口（第二张同种托管卡是卡校验失败）；同种引擎的多实例是预留（§2.12）。
>
> **文档修订 1.6 勘误七（#626 与 S5 实机冒烟的四处回填，不改 wire，2026-11-21）**：（1）§2.3：卡带 `extensions.idoris_upstream` 时，serve 在加载组件卡时核对 `flavor`：必须存在、符合 §2.12.1 的种类格式，并与 `version_pin` 的产品名一致（`version_pin` 没有合法产品名也算不一致）；否则卡校验失败、serve 退出码非 0，说明里带卡文件名与原因码 `upstream_flavor_invalid`（缺失、不是字符串、格式非法，或 `idoris_upstream` 不是对象）或 `upstream_flavor_mismatch`。不带 `idoris_upstream` 的卡行为不变；按卡识别 oMLX key 的规则（勘误四）不变。（2）§2.7.8：`setup omlx --force` 不再要求目录里只有 oMLX 自己的文件：它只替换 `omlx.yaml`、伴生卡 `omlx-embed.yaml`、固定文件 `subscription.yaml`，目录里别的卡与文件（例如 `setup upstream` 生成的卡）原样保留，所以 `setup omlx` 与 `setup upstream` 不再有先后顺序要求。保留的卡与新卡一起放进暂存目录、用 serve 同一个加载器整体校验：`provider.id` 冲突（含手写了 `omlx` 或 `omlx-embed`）、保留的卡本身加载不了（含 `flavor` 写错）、最终的集合（保留的加新写的）里默认卡（`extensions.idoris_default: true`）超过一张（`setup upstream` 同此规则；目录里手写出两张默认卡时，连不带 `--default` 的写入也不放行），都报 `validation_failed`（说明里点名文件或卡）并整体不落盘，目录一个字节不变；失败时整体恢复。落盘事务：旧文件先挪进暂存目录里独占创建的 0700 子目录（不再往用户目录里放 `*.idoris-bak-*` 之类的备份；用户目录里名字像备份的文件按被保留的文件处理，原样不变），新文件用 `hard_link` 放入（目标名上冒出别的文件就失败，不覆盖）；任何一步失败都把已放入的新文件删掉、旧文件放回；放不回的文件，错误说明（`write_failed`）逐个给出它的原位置与现在的位置（路径，不含内容），暂存目录保留以便手工恢复。整段（快照保留的卡、校验、落盘）在 out-dir 的独占 `flock` 里（out-dir 不存在时锁它的父目录；目录本身当锁，不留锁文件），`setup omlx` 与 `setup upstream` 用同一把锁，对同一目录的并发运行排队；落盘前重新核对被保留的卡（条目集合、设备与 inode、大小、mtime、内容 SHA-256），任何一项变了就整体放弃（`validation_failed`，什么也没写，请重跑）。保留的符号链接按加载器的口径跟随读取并按卡参与校验，链接本身不被替换；读不出内容的条目（断链、名字像卡的目录）整体失败。**剩余风险（已接受）**：建议锁约束不了不遵守它的进程，最后一次核对与落盘（`hard_link`、`rename`）之间仍有一个很短的窗口，恶意写入者可以在窗口里动手；setup 是用户自己运行的命令行，不防这一类。不带 `--force` 且目录非空仍报 `out_dir_not_empty`。（3）§2.11：`GET /v1/models` 里，指向同一个端点（比较时按请求的实际发法解析 URL：scheme 与主机不分大小写、补全默认端口、去掉末尾 `/`；路径、查询、用户信息区分大小写，原样比较）的多张 HTTP 卡（典型：主卡 `omlx` 与伴生卡 `omlx-embed`）列出的同一个模型 id 只出现一次，`provider.id` 字典序最小的卡胜出（主卡压过伴生卡），与卡的加载顺序无关；端点不同的卡仍各列各的。本规范没有规定 `/v1/models` 列出 `idoris/<role>` 别名，实现也不列，不变。（4）§2.7.8：`setup omlx` 输出的 `next.needs_env` 形状不变（仍是 `["IDORIS_OMLX_API_KEY"]`，只在引擎要鉴权时出现），新增 `next.key_note` 一句说明：key 其实是可选的，运行期按 `IDORIS_OMLX_API_KEY`、`~/.omlx/settings.json` 的 `auth.api_key`（只发给 loopback 的 oMLX）、无鉴权连接的顺序取（§2.9「oMLX 密钥」）。
>
> **文档修订 1.6 勘误八（S8 预热 `POST /v1/models/warmup` 实现回填，不改 wire，2026-11-21）**：把 §2.11.2 的实现口径写进正文，只增不改契约（wire 仍是 1.2.0 → 1.3.0 随 v0.6.0 发版）：（1）**请求校验**：请求体只有 `model`（必填、非空字符串）与 `timeout_ms`（可选，整数，1000 到 300000，缺省 120000；浮点、字符串、`null` 都不是整数）。别的字段、缺 `model`、`timeout_ms` 不合法一律 400 `unsupported_field`，`reason_code` 依次是 `unsupported_parameter`、`unsupported_model`、`invalid_timeout_ms`；请求体不是 JSON 是 400 `invalid_json`，是 JSON 但不是对象是 400 `invalid_body`（同聊天）；头部错误（`invalid_privacy`、`unknown_role`、`tenant_missing`……）同聊天，顺序也同聊天：请求体先于头部，头部先于 key 范围（403）。（2）**选卡与能力**：候选与确定性选择同聊天（角色绑定、隐私、熔断的就绪快照），另有四条：订阅中转卡不参与（没有「加载」可言）；请求的能力由角色定：`idoris/embed` 是 embedding，`idoris/rerank` 是 rerank，其余（含没有角色的具体模型 id）是 chat，`X-iDoris-Capabilities` 头不起作用；能力决定哪些卡有资格（只会 chat 与 embedding 的卡不是 `idoris/rerank` 的候选，得 503 `no_eligible_candidate`，什么也不加载）；生命周期卡只需要加载，三种能力一样；resident 卡要发请求，而规范只定义了 chat 与 embedding 的最小请求，所以 `idoris/rerank` 选中 resident 卡时明确拒绝（400 `unsupported_field`，`reason_code:"unsupported_model"`，served-locality 头照常），什么也不发，不静默当成 chat；`idoris/auto` 按字面的角色绑定处理，不做意图识别。预热只服务本地免费的卡：单价非零的卡是 503 `paid_proxy_unavailable`，什么也不发。key 的范围、隐私等级、远程熔断开关与 embeddings 同一个入口：key 无权是 403，不加载任何东西；开关合上时 `X-iDoris-Privacy: any` 被钉回 `local_only`，一个字节也不会发往非本机的卡。（3）**生命周期卡**：与聊天的按需加载共用一段代码（`ensure_loaded`：读 catalog、拒绝 catalog 里没有的模型、带余量确认的 Supervisor 幂等 load）。200 里 `already_loaded` 是「调用时 Supervisor 的账本里它已经就绪」，尽力而为、不是原子读（并发的调用可能都报 `false`）；它信任账本：引擎在 iDoris 不知情时换代（例如外部引擎被用户重启）之后账本已过期，`already_loaded` 会是 `true` 而模型其实已不在引擎里，预热也不会重新加载它——这是 §2.7.6「v0.6.0 与 v0.6.1 的边界」里已经写明的同一个限制（热恢复后移 v0.6.1），预热没有引入新问题；`load_ms` 是从选定到就绪的总耗时（含读 catalog），已就绪时只有几毫秒；`state` 恒为 `ready`；`model` 是解析后的具体 id。404 `model_not_found` 是 catalog 里没有这个模型（调用方给的具体 id，或角色绑定的那个模型，同样适用）；其余后端错误沿用聊天的映射：503 `headroom_confirmation_required`，503 `local_only_unavailable` 加后端原因（`supervisor_busy`：有限退避后重试；`eviction_impossible`、`load_unconfirmed` 等：终态）。同一模型、同一卡策略、同一余量确认状态的并发预热与并发的首个聊天请求由 Supervisor 合并成**一次**引擎加载；余量确认状态不同的并发请求不合并，后到的得 503 `supervisor_busy`（同聊天）。生命周期卡的预热不记入提供方熔断：它是应用自己触发的准备动作，不是请求流量（resident 卡走代理，按代理的规则记入，见（4））。（4）**resident 卡**：发一个最小的真实请求：chat 是 `{"model":<绑定的模型，或调用方给的具体 id>,"messages":[{"role":"user","content":"warmup"}],"max_tokens":1,"stream":false}`，embedding 是 `{"model":…,"input":"warmup"}`；走聊天用的同一个代理（oMLX key 按卡识别、连接失败重试、熔断与并发上限、非本机出口计数），区别只有：请求在自己的任务里跑，响应头期限是它自己的 180 秒（不是聊天的 10 秒，不是 embeddings 的 30 秒，也不是调用方的 `timeout_ms`，见（5）），不带幂等键（每次都是真请求，不重放缓存）。上游回答的翻译：2xx 即 200（`already_loaded` 恒为 `false`：resident 卡没有账本；`load_ms` 是这一个请求的耗时）；404 是 404 `model_not_found`（上游的文字不外传）；504（响应头或正文超时）是 504 `warmup_timeout`；其余非 2xx 与聊天的 resident 路径一样原样透传，含代理自己的 502 `upstream_auth_failed` 与熔断的 503。（5）**`timeout_ms` 到点只是放弃等待**：返回 504，`error.type` 与 `error.reason_code` 都是 `warmup_timeout`，**不取消、不缩短**任何东西，客户端断开同理。生命周期卡：加载在 Supervisor 的后台任务里，与请求的 future 无关，照常跑完并清掉 `load.pending`；之后的预热看到 `already_loaded: true`，聊天不再加载，仍在加载时到达的聊天并入同一次加载。resident 卡：最小请求在它自己的任务里跑，占着代理的一个并发许可（共 32 个，所以同时挂着的预热请求总数有上限），发给上游的请求不因调用方放弃而被取消；它在上游答了或它自己的 180 秒响应头期限到点时收尾，代理照常结算熔断的准入（504 按既有规则计入提供方熔断，所以反复超时的 resident 卡会冷却）。**预热自己的 `timeout_ms` 与客户端断开不留围栏；引擎侧的超时或 panic 保持既有的围栏保护**：`timeout_ms` 大于引擎自己的加载超时（下条）时，先到点的是引擎的：结果未知，503 `local_only_unavailable` 加 `load_unconfirmed`，围栏保留，与聊天一致；加载过程中的 panic 同样是结果未知，围栏保留。`timeout_ms` 大于 resident 请求自己的 180 秒期限时，先到点的是请求自己的期限：同样是 504 `warmup_timeout`，消息里写的是实际等了多久。（6）**独立的加载超时（§2.11.2「实现前提」，已做）**：`RuntimeAdapter` 新增 `load_timeout()`（缺省 `None`，无变化）；oMLX 适配器的 `POST /v1/models/{id}/load` 用它，`status`、`unload`、`probe_ready`、`list` 与加载之后的固定状态核对、钉住仍用 `call_timeout`；Supervisor 对 `adapter.load` 的外层兜底是适配器声明的值加 30 秒（不低于通用的 30 秒），首次尝试与 OOM 重试都用它。生产（`create_adapter`）的值缺省 180 秒，环境变量 `IDORIS_LOAD_TIMEOUT_S` 可改：整数秒，1 到 3600，未设或空白用缺省，其他一律在构造时报错（启动失败，错误点名该变量），不静默用缺省。超过它的加载仍是「结果未知」（`load_unconfirmed`，围栏保留）。这条对聊天路径的按需加载同样生效（共用 Supervisor），聊天不再被 10 秒与 30 秒截断冷加载。（7）**副作用**：审计事件是一条 `audit.finalized`，`intent` 固定为 `warmup`（不取 `X-iDoris-Intent`），其余字段同其他推理路径（`http_status`、`latency_ms`……）；成功与失败都记；不写用量记录，不碰预算账本；`X-iDoris-Request-Id` 只用于审计关联，不是幂等键。请求被取消（客户端断开）时，审计中间件走不到写事件的那一行，加载或 resident 的请求却照常跑完，所以由 handler 里的取消守卫在它自己的任务里补一条：`http_status` 499、`reason` 为 `degraded: request_cancelled`（与被丢掉的流的 `degraded: stream_cancelled` 同一类），事件归属的租户按审计中间件同一个规则取自已认证的身份。完成、`timeout_ms` 到点（一个正常返回的 504）、取消三种结局各写**恰好一条**。取消发生在认证之前（还没进 handler）时没有事件：那时还没有任何东西被预热。云形态打开时，本路由与 `/v1/embeddings` 等处在同一道儿童数据守卫之后。（8）**门控与降级**：启动门控未放行时与其他数据路径一样 503 `idoris_starting`（不看请求体）；降级放行之后路由到没就绪的托管卡（lifecycle 与 resident 都一样）直接 503 `local_only_unavailable` 加该上游的 `reason`（没有稳定原因码的失败是 `supervisor_unavailable`），不碰引擎，served-locality 与 reason 头照常；别的健康的卡照常预热。
>
> **文档修订 1.6 勘误九（#633 评审的后续，不改 wire，2026-11-21）**：§2.7.8 `setup omlx --force` 再收紧四处，并记一处已知限制。（1）被保留的符号链接若指向这次要替换或删除的文件（例如 `alias.yaml -> omlx-embed.yaml`），整体 `validation_failed`，点名链接与目标：暂存目录里放的是解引用后的内容，校验会过，落盘后链接就悬空了。（2）受管名字（`omlx.yaml`、`omlx-embed.yaml`、`subscription.yaml`）上必须是普通文件：那里是目录或符号链接时 `--force` 报 `out_dir_not_empty`，不挪、不删、不替换；`setup upstream` 本来就是这个口径。（3）撞名检查：目录里有名字不同、却与这次要写的文件是同一个路径的条目，一律 `validation_failed`，说明里点名两个文件名。"同一个路径"有两种：只差 ASCII 大小写（用户的 `OMLX.yaml` 与要生成的 `omlx.yaml`），以及文件系统自己折叠成同一个文件（APFS 把 `ſubscription.yaml`（U+017F）与 `subscription.yaml` 当同一个文件，所以还比较两个路径背后的设备与 inode）。`setup upstream` 写 `<id>.yaml` 时同样检查（探测之前和落盘事务里各一次），带不带 `--force` 都拒绝，因为被保留的快照只排除名字完全相同的、真的会被替换的条目；在大小写不敏感的文件系统（macOS 默认的 APFS）上它们是同一个路径，不能悄悄换掉用户的文件，大小写敏感的文件系统上同样拒绝，让同一个目录在不同机器上行为一致。（4）`--force` 不再放行「目录非空、没有任何组件卡（`.yaml` / `.yml`）、也没有受管文件」的目录，报 `out_dir_not_empty`，说明里提示多半是 `--out-dir` 写错了；有别的卡的目录（#626 的用法）照常。**已知限制**：落盘用 `hard_link` 放入新文件，不支持硬链接的文件系统（部分网络盘、FAT/exFAT 类的卷）上 `setup` 会以 `write_failed` 失败而不是降级，目录保持原样（回滚见勘误七）；需要在这类目录里用时，先在本地目录生成再复制过去。
>
> **文档修订 1.6 勘误十（S6 Admin `upstreams` 与引擎换代回填，不改 wire，2026-11-21）**：把 S6 实现时定下的口径写进正文，只增不改契约（wire 仍 1.2.0；`CONTRACT_VERSION` 在 S11 统一升 1.3.0）：（1）**§2.11.4 的元素与错误**（详见 §2.11.4 末尾的回填）：Admin 元素是 `/health.upstreams[]` 的六个字段加 Admin 专属的只读字段 `port`、`pid`（仅 `owned`）、`restart_required`、`failure_count`、`last_failure_at_ms`，**没有任何 key、token、nonce 的位置**（schema `admin-v0-upstream.schema.json`、`admin-v0-upstreams.schema.json`，两侧 `additionalProperties:false`）；`POST` 的 202 体是**一个元素**；错误码 `upstream_not_found`（404）、`upstream_not_owned`（409，`reason_code` 区分 `not_managed` 与 `not_owned`）如规范，另有同族新增 `restart_required`（409）与 `idoris_stopping`（503）。（2）**引擎退役与换代的次序，与 Supervisor 的账本（取代勘误六（10）里「运行期引擎换代是 v0.6.1 / S10 的事」对 Admin 命令的适用；Codex 预审 1、2、4）**：S5 留给 S6 的问题是：门控放行之后 `restart` 一个自有引擎，Supervisor 对它采样的账本过期（它以为 `Ready` 的模型已经不在新进程里，共享容量账本里还占着旧进程的内存）。预审又指出，只复位账本不够，**次序**才是正确性所在：旧引擎上还有没做完的加载或聊天时，不能清围栏、不能拉起新进程，旧一代的任务也不能拿到新一代的凭证。调研结论是**不需要 S10 那种重建**（重建 actor、给围栏路径加代际）：Supervisor 新增**准入控制**（`close_admission`、`reopen_admission`、`is_quiescent`）与**原地复位** `engine_replaced`。次序（`stop`、`restart`、`start` 一个还活着却不答的引擎、外部引擎换了进程，同一套）：① **关门**：Supervisor 不再准入新的加载、卸载、聊天、目录读取（`supervisor_unavailable`）；聊天与预热的 `ensure_loaded` 走同一个入口，没有第二个；② **发布不可服务的状态**（`stop` 是 `stopped`、`restart` 是 `restarting`，数据面立即 503，不靠请求入口那一次性的状态检查），再回答 202；③ **排空**：等已经准入的加载与聊天做完，至多 `drain_timeout`（缺省 30 秒），它们拿着准入时的凭证在旧引擎上照常跑完；到点还有在途的，就**切断旧一代的凭证**（封上端点、代际前进），它们之后发的每个请求都被适配器拒绝；④ **停进程**（SIGTERM、宽限、SIGKILL，**确认**它消失）；没能确认就不清围栏、不拉起新的；⑤ **封上端点**、**清围栏**（进程确认消失之后才清：围栏说的是已经不存在的上一代）、拉起新进程，新引擎通过校验后登记新的私有 key（代际前进）；⑥ **复位账本**：只在静止时（有进行中的加载、卸载或聊天回 `Busy`、什么也不改，所以旧一代的完成消息不可能落在重建的账本上），对着新进程重新采样，和启动采样同一套规矩，共享容量账本里的占用按观测改写，围栏锁与 actor 不动；采样失败或围栏还在就把错误粘滞住，fail closed，之后的复位可以清掉；中毒的 actor 不被复位；⑦ **开门**，发布 `ready`（这之前数据面对这张卡是 503，没有请求带着过期的账本进来）。**每个准入的操作绑定准入时的引擎代际**（Supervisor 在单写者循环里读一次，用任务局部量 `pinned_generation` 带给适配器，后台任务与延迟的卸载也一样）：适配器取 key 的唯一入口核对它，代际已经不是当前的、或端点被封上，在**发送任何东西之前**拒绝，所以旧一代的任务拿不到新一代的凭证，请求体也不会发给新进程。resident 的托管卡没有 Supervisor（没有关门与排空可言）：代理在每一次发送前同样核对（封上的端点什么都不发；`POST /v1/models/warmup` 的最小请求在自己的任务里跑、调用方放弃之后照常跑完，所以它绑定准入时的代际，连接失败的重试也核对；被拒绝的发送不占提供方熔断的名额）。复位只在引擎**进程身份**变了时做（自有引擎：pid 加启动时间；外部引擎：校验通过的监听者的 pid 加启动时间，pid 被回收给另一个进程也分得出来；`start` 重新认回同一个引擎不复位，也就不会忘掉对未确认加载的记忆）；`stop` 在进程确认消失之后清空账本（不问引擎）。复位失败的引擎不放行：停掉，按一次失败算，退避之后重来；Supervisor 没有了（actor 退出或中毒）是终局。所以**放行时就绪的上游的 `start`、`restart` 照常可用**，下一次请求发现模型没加载就重新加载。**仍然冻结的只有放行时没就绪的上游**（降级放行）：完整的 Router 里根本没有它的 Supervisor，`start`、`restart` 是 409 `restart_required`，`restart_required` 字段为 true；这是 S5 起的已知限制，不是账本过期的问题（给它建 Supervisor 要重建 Router）。外部引擎：监听者没了只报 `unavailable`（端点封上、门关上，iDoris 永远不重启它也不给它发信号），**同一个进程**回来时开门、账本不动，**换了进程**（pid 或启动时间变了）走同一套次序（封上、关门、排空、复位、开门）之后才回 `ready`。不变：自有引擎的**进程自己退出**仍是退出码 5（这是 S10 的事，本修订没有碰）。（3）**S5 评审留下的三道门**：（G1）`OmlxAdapter` 不再在构造时读一次 key：引擎管理器用的适配器每个请求按登记表此刻的口径取 key（私有 key、外部引擎 `settings.json` key 的属主校验状态），引擎换了、停了、端口换了主人，已经存在的适配器不会再发旧的 key；数据面也不再用完整的 Router 创建时拍下的名单：**每个请求读 `/health` 的实时快照**判断托管卡能不能把请求交给引擎（`ready` 和「引擎活着、探测没答上来、没有确定原因的 `unavailable`」交给；`starting`、`restarting`、`absent`、`failed`、`stopped`，以及有确定原因（`auth_required`、`auth_failed`、`fence_pending`、`startup_latched`……）的 `unavailable` 直接 503 `local_only_unavailable`，原因码是该上游的 `reason`，没有稳定原因码的沿用 `supervisor_unavailable`，请求不碰那个端口，`/v1/models` 与 Admin `models` 同口径），运行期失败、被 Admin 停下、重启中、端口换了主人的卡下一个请求就 503；（G2）**封上，不撤销**（Codex 预审 3）：托管的端点在引擎管理器启动时就先封上（`EndpointKey::Blocked`：什么凭证都不发——私有 key、环境变量的 key、`settings.json` 的 key 都不发，也不退回 §2.9 的回退链；那条回退链是给没人管的端点用的）。引擎停止、正在被替换、外部引擎的监听者没了、端口上换了别的东西时都回到封上，直到下一个监听者通过校验、被登记成私有或外部的口径；v0.6.0 里托管的端点永远不会变成非托管，所以不再有撤销登记（撤销会让回退链重新适用，把用户的 key 发给之后占着这个端口、还没通过校验的别的东西）。停止自有引擎时先等进程确认消失、之后才换掉私有 key 的登记（停止的这几秒里引擎还在监听，发往它的请求仍然只用它的私有 key）；没能确认消失也封上。代理、适配器、模型列表、就绪探测都经同一个解析入口；外部引擎的监听者没了（`NoListener`）立刻封上，不等下一个周期的校验；（G3）`start` 对还活着的自有引擎（`unavailable`：活着却不答）走完整的停止路径（SIGTERM、宽限、撤销 key）再重新走启动决策，不再丢弃句柄（丢弃就是 SIGKILL，绕过停止路径）；`attempt()` 也不丢弃任何还活着的句柄。（4）**拉起日志**：管理器每次拉起引擎写一行 `拉起引擎：program=… port=… memory_guard=… model_dir=… base=…`（oMLX 会改写自己的进程标题，`ps` 里看不到这些值），只含这几个非秘密的值，私有 key（经环境变量交给引擎）一概不在其中。（5）**并发的 `restart`**（Codex 预审 5）：命令在**入队时**盖章（每个上游一个序号）；一次 `start`、`restart` 操作做完时，管理器记下此刻的序号，之后凡是入队时序号不超过它的 `restart`——它们是在这次操作进行期间到达的，管理器在排空、停进程、复位，没有去收命令——直接回 202，并入这次操作的结果，**不换第二次引擎**；`restart` 在引擎已经在（重新）启动时本来就幂等（202，并入）。（6）**启动决策里的探测不是引擎失败**（Codex 预审 6）：第一次启动之前对端口的探测，答案可能是「有东西在监听、但答不上来」（超时，或者连接被接受之后被重置——一个正在被拆掉的监听者），紧接着端口就空了（连接被拒）；监听者在属主校验之前没了；带 key 再问的时候外部引擎已经走了。这些时候**还没有哪个我们拉起的进程出过问题**：管理器回到决策的开头重来，不记入熔断窗口、不退避、不发布 `restarting`、不加 `failure_count`；一串重来整体受一个启动超时约束，到点是 `unavailable` + `startup_timeout`（不是无限循环）。真的失败（拉起的引擎没就绪就退出、复位失败）照常按次计数，熔断照常。偶发的多记一次失败的根因：第一次探测收到的是 `ECONNRESET`（连接被一个正在被拆掉的监听者接受之后又重置，机器忙、端口被别的进程刚用过时出现），S5 把任何不是「连接被拒」的传输错误都当成「有东西在监听、等它」，随后端口空了被拒，就当成了引擎失败。（7）**密钥治理审计（Codex，H1–H3）**：（H1）托管的端点在引擎管理器启动时就先登记成「封上」（先于它自己的、也先于别的卡对同一个端点的第一次探测），**终局的失败之后保持封上**（接管外部引擎时校验通过的监听者已经被登记成「外部」，它要 key 而没有、key 被拒、围栏还在……之后没有监督再去重新校验端口，那个口径不能留着）。**与托管引擎共用同一个 endpoint 的卡（伴生卡 `omlx-embed`）继承这个引擎的失败状态**：引擎管理器启动时把托管引擎的 endpoint 登记进 `HealthState`，共用它的卡在后台探测、`/v1/models`（含 Admin `models`）、预热与转发里都读到同一个状态与原因码，一个请求都不碰那个端口；不共用引擎的上游照常服务。（H2）生命周期适配器的工厂不再只看 `provider.id`：**这张卡能不能带 oMLX 的 key 用代理的同一个判据**（`idoris_upstream::omlx::key::card_may_carry_omlx_key`，`models::is_loopback_omlx` 就是它），手写的、带 `idoris_upstream` 的伪装卡得到一个没有 key、也不查登记表的适配器；能带 key 的适配器每个请求取此刻的口径与代际，**不缓存任何 key 字符串**；外部监听者没了、身份读不出来、换了，都立刻封上。（H3）自有引擎的私有 key 只发给确认过的监听者，**每一次**：等就绪的循环里每次带 key 的探测之前、监管的每个节拍里，都重新确认监听这个端口的此刻还是我们的进程（`listens_on(pid, port)`，不缓存；读不出来按 `unverifiable_retries` 重读，仍读不出来当作没有确认）；确认不了就先收回私有 key（端点封上）、发布 `unavailable` 加 `port_in_use`（数据面 503），这一轮不发探测；又确认了，把私有 key 登记回去。进程活着不等于端口还是它的：引擎可以放掉端口而不退出，别的进程接着绑上来。
>
> **文档修订 1.6 勘误十三（Codex 集成审计的三条 setup 回填，不改 wire，2026-11-22）**：（1）§2.7.8：serve 读组件目录时对目录拿**共享** `flock`，与 `idoris setup` 落盘时的独占锁是同一把：setup 落盘期间 serve 的读卡等着，读到的要么是落盘前完整的旧集合，要么是落盘后完整的新集合，不会是 setup 一个一个挪动文件的中间状态（此前启动器在 setup 中途重启 serve，serve 可能读到混合的集合并一直用着）。等锁有上限（默认 30 秒；环境变量 `IDORIS_COMPONENTS_LOCK_WAIT_MS` 毫秒数可调，诊断与测试用）：先立刻试一次，锁着就重试，每次重试之前看剩余时间、睡剩余时间与 20 毫秒里小的那个；`0` 是只试一次，没设或空白用默认，纯数字按毫秒，超过 10 分钟的值（含装不进 64 位的数字）按 10 分钟截断，其他取值（负数、小数、带单位、乱写）是启动错误，说明里点名变量与取值；超时启动失败，说明点出目录、实际等了多少毫秒，并提示是 `idoris setup` 在改写它；反过来 serve 正在读时 setup 的落盘等它读完。建议锁约束不了不遵守它的写入者（手工改文件），那一类改动不在这个保证里；目录不存在或不是目录时立刻报「不是目录」，不拿锁也不读卡（不留「先看一眼不存在、再看一眼又存在」的缝）；在 Unix 上，每一条走到读卡的成功路径都持有共享锁。serve 启动时只确定**一次**组件目录的 canonical 路径，加锁与读卡（相对锁住的目录 fd 枚举并 `openat` 读取，所以目录本身中途被改名也读的是加锁的那一个）都用这同一个结果；`components::load_components_locked_canonical` 接受调用方已确定的 canonical 路径，发布给 launcher 的启动快照应当用同一个值。**只保证当前目录**：卡文件是指向目录之外的符号链接时，serve 与 `setup` 都按校验失败处理（`validation_failed` / 启动失败，说明点名链接与它指向的位置），因为外面那个目录的锁我们拿不到；目录之内的符号链接照常按卡读取。（2）§2.7.8、§2.7.6：`setup omlx` 与 `setup upstream` 在发布（落盘）之前，对暂存目录里「保留的卡加新卡」的最终集合，除了加载器与默认卡检查，再过一遍 serve 启动时的纯配置校验（`preflight::validate_card_set`）：运行时对卡 `form` 的确定拒绝（订阅卡与 resident 的 HTTP 卡不走运行时，其余只有 `http_service` 有运行时构造器，别的形态如 `bundled_binary` 报 `has no runtime constructor`，serve 启动时用的是同一个纯函数 `runtime::check_runtime_form`）、引擎型上游的种类解析，以及托管卡的计划——endpoint 形状（`http://127.0.0.1:<端口>`）、引擎种类有对应的管理器、端口不重复、每种引擎只托管一个实例。不过就 `validation_failed`，说明是 serve 会报的那一句，目录不变。典型情形：目录里已有一张 id 与端口都不同的托管 oMLX 卡，再 `setup omlx --force --managed`，以前 setup 成功而 serve 在托管计划里退出 1，现在 setup 一侧就拒绝。校验是纯函数（`lifecycle::validate_managed_plan`），serve 的 `plan_managed` 用的是同一个，不读环境、不碰文件系统、不探测引擎；需要读环境的检查（托管引擎需要 `IDORIS_STATE_DIR` 或 `HOME`）是运行环境的事，不在其中。（3）§2.7.1、§2.7.8：路径类环境变量（含 `IDORIS_COMPONENTS_DIR`）里 `trim` 只用来判断是不是整个都是空白，非空的路径原样使用、不去掉首尾空白，所以 `setup` 输出的 `next.env.IDORIS_COMPONENTS_DIR` 原样交给 serve 就是同一个目录（路径末尾带空格也是）；整个是空白仍视为没设。`optional_env_path`（本地运行时配置、云档配置等）同此规则。（4）`setup` 对卡集合的加载与 serve 共用 mock 开关：`IDORIS_ALLOW_MOCK=1`（只有 `1` 算开）且编译时带 `dev-mock` 特性才加载 `provider.id: mock` 的卡，否则跳过；`setup` 放行的目录与 serve 在 mock 卡要不要进来上是同一个答案。

> **文档修订 1.5，2026-11-19**：只改状态说法，不改契约内容。§2.9 中的推荐接口、本地模型列表和 oMLX 密钥顺序已在 iDoris v0.5.0 实现，wire 契约 1.2.0 随该版本生效；§2.9 的下载部分仍未实现，接口形状待另行修订。
>
> **文档修订 1.4，2026-11-15**：owner 要求装了 oMLX 的机器首次启动时按硬件自动选默认 chat 模型，由 iDoris 负责推荐。新增 §2.9（本机 chat 模型推荐、本地模型类别与下载，拟议 wire 1.2.0，只新增），档位表由 owner 拍板，iDoris 与 Agent24 两个会话各确认一次。iDoris 侧推荐、本地模型列表与密钥顺序已在 v0.5.0 实现（见修订 1.5）；下载部分仍未实现。
>
> **文档修订 1.3，2026-10-28**：owner 要求 Agent24 桌面端一站式「下载 → 运行 → 连接」iDoris，这定下了 R9 的 OD-1（launcher 宿主为 agent24d）和 OD-2（由用户在桌面端同意；**已被修订 1.6 取代**：iDoris 随 Agent24 自动安装、不弹同意，大文件下载仍须用户确认，见 §6）。§2.1 第一条随之改写，新增 §2.7（launcher 托管契约，iDoris v0.4.0 起）与 §2.8（wire 契约 1.1.0）。已接受条款以正文为准。
>
> **文档修订 1.2，2026-10-15**：将 R0–R8 已接受条款合并到正文，消除“后补章节覆盖前文”的阅读方式；当时新增的 v0.3.1 范围及 R9 三项决策仍是 **OWNER REVIEW PENDING**。（其中 OD-1、OD-2 已由修订 1.3 决定，见 §2.7；fast/daily 标签重标仍待定。）
> **唯一跨仓规范权威**：本文件。iDoris 主持，Agent24 作为消费者审阅；Agent24 应保留短消费方指针、实现证据及反向硬件权威链接，不复制第二份契约。总体规划的项目里程碑仍见 [总体规划](../iDoris-总体规划.md)。

## 0. 版本、状态与证据边界

**两个独立组件，一份公开 HTTP 契约，无 crate/源码级耦合。** Agent24 管任务、动作授权、工具和运行期记忆；iDoris 管模型调用准入、推理策略、模型生命周期及其审计。规范条款不等于已实现功能；以下固定基线决定“实际”含义。

| 标识 | 含义与基线 |
|---|---|
| 文档修订 `1.5` | 文字修订；不是服务 wire version。wire 契约版本见 §2.8；§2.9 的 wire 1.2.0 随 iDoris v0.5.0 生效（下载部分除外，尚未实现） |
| 文档修订 `1.6`（已冻结，2026-11-21，Agent24 已同意） | 文字修订；不是服务 wire version。新增 §2.10、§2.11、§2.12，增补 §2.1/2.2/2.3/2.5/2.7/2.8/2.9/4.2，协商记录 R12。条款已冻结（§2.12 的预留项另待后续修订冻结）。冻结的是契约，不是 iDoris 的实现：在 v0.6.0 发版之前，不得据此宣称 iDoris 已实现，也不得当作已发布的服务行为 |
| iDoris release `v0.3.0` | 实现提交 `03c079c70611331a8c85a1666e32a75df6359d09`（下称 I0） |
| runtime `contract_version=1.0.1` | I0 的 `crates/idoris-contracts/src/version.rs::CONTRACT_VERSION`；TS `packages/contracts/src/version.ts` 同值 |
| Agent24 审计基线 | `6072fd373142eb3dbf1a6a4a3222e6eec82c74f6`（下称 A0，固定历史快照，非当前 main）；main 源码状态不等于 stable 已发给用户 |
| v0.3.1 / v0.3.2 | 已发布；launcher 托管从 v0.4.0 起，见 §2.7 |
| wire 契约 `1.3.0`（已冻结） | iDoris v0.6.0 的目标版本，内容见 §2.11，只有新增；契约已冻结。发版之前，`CONTRACT_VERSION` 仍为 `1.2.0`（v0.5.0），`/health.contract_version` 同值 |
| iDoris release `v0.5.0` | 提交 `014845472bc2d2d0efbc68d5739d293965aa00b2`（tag `v0.5.0`，wire 1.2.0）。修订 1.6 对 iDoris 现状的描述以该提交为读取基线，来自设计稿的静态阅读，未运行软件测试 |
| Agent24 IDORIS-CORE 基线 | `bf70404792dd8aecf128d26a7e3f19e737a97cff`（[#1937](https://github.com/iDoris-ai/Agent24/pull/1937) 的合并提交，2026-11-20），方案见 [IDORIS-CORE-PLAN](https://github.com/iDoris-ai/Agent24/blob/bf70404792dd8aecf128d26a7e3f19e737a97cff/docs/design/IDORIS-CORE-PLAN.md)；固定历史快照，不是当前 main |

除明确标为“提案/目标”的部分外，正文表达 R0–R8 的既有边界；§4 标明其实现差距。已有 JSON Schema 对它覆盖的 wire 结构是机器真源；未覆盖条款为文字规范，不能声称 schema 已齐全。发生实现差距时须如实失败或显示不可用，不能把目标字段当作当前 API。

**反向权威**：在同机组合中，主机事实与整机预算分配归 Agent24；公式和硬件能力计划只引用 [HW-TIERS §3.3/3.4（固定 961a6408）](https://github.com/iDoris-ai/Agent24/blob/961a64085576eec365d0442ed27d2a7422168200/docs/design/HW-TIERS-capability-hardware-matrix.md) 与 [ADR-036 提案（同一固定提交）](https://github.com/iDoris-ai/Agent24/blob/961a64085576eec365d0442ed27d2a7422168200/docs/decision.md)。A0 审计后，[#964](https://github.com/iDoris-ai/Agent24/pull/964) 已合并为 `622be5d7df0387b91028820624a778cb99cd6d43`，[#966](https://github.com/iDoris-ai/Agent24/pull/966) 已合并为 `cb11d7bc345ce8dfb5c438e4d6cf0d3663e76724`（2026-10-15 状态回读）；这不改变固定 A0，也不证明已发布给用户。HW-1 来源固定为 `f857ecfa4493b181d17a1d8e9323efde36ffe425`；其 `HardwareReport` 仅有 `profile/level/usable_bytes/reasons`，没有 `llm_allocated_bytes`。不直接依赖 agent24-hw，不照抄提案中已过时的“推荐器尚无分配额入参”。

动作授权与 Desktop 安全实现由 Agent24 的 [ADR-032/035（固定 A0）](https://github.com/iDoris-ai/Agent24/blob/6072fd373142eb3dbf1a6a4a3222e6eec82c74f6/docs/decision.md) 负责；跨仓边界以本文整合条款为准。

## 1. 总原则

| # | 原则 | 出处 |
|:---|:---|:---|
| G-1 | 按**权限域**划分，不按「智能」划分：Agent24 管动作授权，iDoris 管推理资源准入 | iDoris `ecosystem-boundaries.md` §4.1 |
| G-2 | **只有一个策略源**：推理的路由、隐私、预算、审计策略归 iDoris；Agent24 不新增此类策略，只**传递**任务画像。**澄清（R1）**：Agent24 保留的 tier 授权门、LocalOnly 硬门、按 complexity 排 tier 顺序、Served-Locality 绊线都属于**动作授权**，是与 iDoris fail-closed **同向叠加**的第二道门，**不算**第二个推理策略源；它们不涉及选模型、预算或内容过滤 | ecosystem §6 驳回条目、docs/17 D1、R1 Q-9 |
| G-3 | 进程边界即授权边界：需要排除某类 provider 时，**起不同的 iDoris 实例**，而不是让调用方带排除头 | ADR-032 §9 |
| G-4 | 隐私**单向收紧**：Agent24 声明的 privacy 只能被 iDoris 维持或收紧，iDoris 仍独立做 fail-closed 复核 | ADR-032 §9、iDoris 总体规划不变式 2 |
| G-5 | 加法优先、零回归：新能力以新端点、新 header、新字段的形式出现；破坏性变更走契约大版本 | docs/09 R1、docs/17 |
| G-6 | 不静默：不支持的字段显式拒绝；降级和实际落点在响应头里回传 | iDoris 总体规划不变式 8 |
| G-7 | 敏感开关的最终确认**留在 iDoris**，Agent24 可以发起 | 总体规划 §6.1 |
| G-8 | **按请求阶段划分职责**（v1.1，见 §1.1）。一句话：Agent24 决定「做什么、用什么工具、能不能做」；iDoris 决定「这一次模型调用用哪个模型、在哪里跑、花不花钱、记不记」。**本阶段视图不新增、也不改变 §3 的职责划分；如有冲突，以 §2–§4 为准** | R6/R7、J-1 |

### 1.1 请求阶段与负责方（G-8）

| 阶段 | 内容 | 负责方 |
|:---|:---|:---|
| S0 接入交互 | 渠道、会话 | Agent24 |
| S1 任务理解与规划 | 用户要做什么、拆成哪些步骤、选用哪些工具/技能/模块（**任务级意图**） | Agent24 |
| S2 动作授权 | tier 门、Guardian 风险闸、人工审批；可以调用 iDoris 的 `/v1/systemone` 取判定原语。**组合风险检测**（Lethal Trifecta 一类）归 Agent24，目前尚未实现，不写时间承诺（J-3） | Agent24 |
| S3 工具执行 | 非 LLM 的 HTTP、本地文件、CLI；第三方凭证经 vault 部署 (b) 注入（§2.2） | Agent24 |
| S4 发起模型调用 | 把任务映射为 role，设置 Privacy、Complexity 等请求头，产出请求并交给 iDoris | Agent24 |
| S5 模型调用内部 | **按 Agent24 声明的角色与能力需求选模型与路径**：隐私闸 → 角色/能力匹配 → 预算（只对付费候选做原子 reserve）→ 容量（加载/驱逐）→ 执行（本地运行时，或远程 + 凭证注入）→ 回传 Served-Locality 与 Record-Id。角色内可以降级；**跨角色**降级必须由 Agent24 用 `X-iDoris-Fallback` 声明，并在响应中如实回传（J-2） | iDoris |
| S6 结果返回 | 检查 Served-Locality 绊线，继续执行计划，写入记忆 | Agent24 |
| S7 记录与学习 | Agent24：事件日志 → Evolver 学技能。iDoris：审计元数据与可选轨迹 → 学路由与判定。两边以 `record_id` 关联 | 各自负责 |

**三个职责示例（含后续里程碑设计，不代表 v0.3.0 已全部实现）**
1. **总结今天的微信群聊**（敏感、只能在本机处理）。Agent24 先用本地工具取消息，再以 `idoris/daily` + `local_only` 发起调用。iDoris 过隐私闸后选本地 daily 模型，必要时驱逐空闲模型再加载；本地免费调用不涉及付费预算（仍受容量准入约束），响应回传 `loopback`。如果本地模型加载不起来，返回 **503 fail-closed**，**不会改走云端**。
2. **查资料写竞品分析报告**（公开数据，复杂任务）。Agent24 执行搜索和抓取工具，凭证由 vault (b) 注入。之后以 `idoris/deep` + `any` 发起调用。iDoris 选远程 deep 模型，对预算做原子 reserve（额度不足时仅在调用方显式声明 `X-iDoris-Fallback: next_in_chain` 后允许回落到免费候选，并如实回传降级；未声明或无可行候选则返回 402。I0 router 尚未接入此声明的预算回落，实际仍返回 402，见 §4），注入 provider 凭证（vault (a)），结算后回传 `X-iDoris-Cost-Minor`，Agent24 记入按模块的用量账本。
3. **转账或删除文件**（高风险动作）。Agent24 的 Guardian 调用 `/v1/systemone`（`local_only`），iDoris 用本地判定器返回概率，并如实回报 `judge.engine`。判定器不可用时**拒绝**（`judge_unavailable`），Agent24 转人工确认。资产类签名只能走 AirAccount + passkey（J-7）。双方以 `record_id` 与 `approval_id` 关联记录。

---

## 2. 合并后的边界契约

### 2.1 接入、生命周期与四类凭证

- **修订 1.3**：agent24d 是 iDoris 的执行宿主，负责下载、安装、生成配置、签发 key、启动和停止，契约见 §2.7。iDoris daemon 不启动 Agent24。用户也可以自己手动启动 iDoris，这条路径继续支持。
- **修订 1.6（已冻结）**：launcher 托管时，iDoris 还负责拉起并管理本地推理引擎（首个实现是 oMLX，§2.7.6；多引擎见 §2.12）；iDoris 仍不启动 Agent24。Agent24 完成 R12 的 Q7 迁移后，不再直连 oMLX，Agent24 源码里不再出现任何 oMLX 地址（Agent24 方案对所有模型服务同样适用），所有模型调用只经 iDoris。
- `IDORIS_URL` 必须显式配置，无隐含端口。D-1(a)：配置后独占 iDoris，不留直连 oMLX/Ollama 回退；缺 key、服务不可用或身份不符都明确失败。〔修订 1.6，已冻结：launcher 托管时 iDoris 绑定内核分配的动态端口（§2.7.2），agent24d 经发现文件（§2.7.3）与登记（§2.10）取得实际端口并自行构造数据面地址，所以仍然没有隐含端口；手工启动、固定端口的路径不变，Agent24 用什么开关显式给出该地址不属于本契约。〕
- 已接受完整模型是两个逻辑 provider：`idoris-local`=Local，固定 `local_only`；`idoris-any`=Remote，固定 `any`。按 URL 去重 models/容量，不能相加。D-6 两把 key 分别限定 `[local_only]` 与 `[local_only,any]`。§3 提议仅交付 local 子集，第二 provider/key 延后，不撤销 D-6。
- 模块 manifest 未声明或声明 `local_only` 时使用 `idoris-local`；只有 `remote_allowed` 才有资格进入 `idoris-any`，仍不能扩大调用方已授予的权限。
- 数据面默认必须有 virtual bearer，即使 loopback。`IDORIS_DEV_NO_KEY=1` 仅为显式 loopback 开发选项，无 key 只能走免费 loopback/local_only；错误 bearer 不能借此绕过。非 loopback 必须有有效 key。tenant 模式沿用 `X-iDoris-Tenant`；个人模式不伪造 tenant；tenant 模式的数据 key 必须绑定该 tenant_id。

| 凭证 | 权威/使用者 | 边界 |
|---|---|---|
| 数据面 virtual key `idk_…` | iDoris 签发；Agent24 内核持有 | Bearer 用于数据面；模块不接触；目标持久存储为 Keychain，不与 Admin 混用 |
| Admin session token | 每次启动的可信 launcher → iDoris、Electron main | 只经两条一次性匿名 pipe/FD（Windows 白名单 HANDLE）；不进入 argv/env/普通文件/stdout/stderr/log/renderer。〔修订 1.6，已冻结：launcher 托管路径下 Electron main 不持有 Admin token，桌面端的 Admin 操作都经 agent24d 的 `/api/v1/idoris/*`（R12-Q3）；token 另可在已通过双向证明的登记会话里由 agent24d 用 `set_admin_token` 会话内轮换（§2.10.6），这是「只经两条…」之外唯一新增的传递通道，禁入项不变〕 |
| oMLX 上游 key | iDoris → 后端 | 当前配置 `IDORIS_OMLX_API_KEY`；不是 Agent24 的 `OMLX_API_KEY`，不当 virtual key。〔修订 1.6，已冻结：iDoris 自己拉起的私有引擎另有每次拉起随机生成的私有 key，只在 iDoris 与该引擎之间，见 §2.7.7，待实测〕 |
| Agent24 daemon token | Agent24 daemon ↔ 自身客户端 | 不属于 iDoris，不送进第三方 vault |
| `instance_nonce`（修订 1.6，已冻结） | iDoris 每次启动用 CSPRNG 生成 256 位（小写 hex 64 字符）；agent24d 读 0600 发现文件取得 | 只在 iDoris 进程内存和发现文件里；不进 `/health`、Admin、日志、argv、环境变量、错误信息，也不在 UDS 上明文传输，只作 HMAC 密钥（§2.10.5）。挡的是连错实例、读不到该目录的沙箱进程冒充，**不是**对抗同 UID 恶意进程的屏障，登记方身份由 Agent24 的 `LOCAL_PEERTOKEN` 与 codesign 校验兜底 |

修订 1.6（已冻结）：上表因此多出第五行，标题里的「四类」沿用修订 1.5 的说法，冻结后再改标题，以免破坏引用锚点。`instance_id`（UUID v4，每个 iDoris 进程一个，`/health` 免鉴权公开）只是实例标识，不是凭据，用来核对「这个端口上答话的是不是登记的那个实例」，不能单独证明身份。

I0 `idoris key issue --spec-stdin` 的实际 spec 为 `owner, tenant_id?, allowed_privacy, allowed_roles, expires_at_ms?, admin_scopes`；`admin_scopes` 只能为空，未知字段拒绝。`budget_ref/expires_at` 等旧 scope 草案不是当前签发字段。scope 只能拒绝，不能放宽请求。完整角色权限才允许具体模型 ID 及 next_in_chain，最小 daily key 不能靠具体 ID 绕过限制；见 §4 安全候选的额外修复状态。

Admin v0 为独立 loopback listener，只有 `status/backends/models/runtimes/roles` 五个只读 GET 和只读控制台。Electron main 固定客户端；renderer 只得零参数 `idorisStatus()`，不得提供 URL/path/port/header/token/FD selector。沿用 ADR-035 已接受的 65-byte canonical frame（64 小写 hex + LF）、读后关闭、秘密不继承到后代；选择器是分开的 `--idoris-admin-token-fd <n>`，不是 `=n`。此处不新增 framing、bootstrap HTTP 或 Admin 写接口。无 handoff 显示 `handoff-missing`，不读 env/file 猜 token。producer launcher 尚未交付，已有 consumer 不等于组合交付。

**修订 1.6（已冻结）取代上段两处**：「只有 `status/backends/models/runtimes/roles` 五个只读 GET 和只读控制台」，以及「此处不新增 … Admin 写接口」中的「Admin 写接口」一项。Admin 仍是独立 loopback listener，仍用同一个 Admin token，既有五个只读 GET 的形状不变；另新增只读 `GET /admin/api/v1/upstreams` 和受限写接口 `POST /admin/api/v1/upstreams/{id}/start|stop|restart`（§2.11.4，owner 已定）。写接口只作用于 iDoris 自己拥有的上游进程，不给外部进程发信号；模型 load/unload（[#611](https://github.com/iDoris-ai/iDoris/issues/611)）与 §2.9 的下载仍未交付，将沿用同一套 Admin 写接口通则（§2.11.4）另行修订，预留的默认引擎切换 `make_default`（§2.12.3）同理。「不新增 framing、bootstrap HTTP」两项不变：`--launcher-stdin`（§2.7.1）是 launcher 到 iDoris 的 stdin 帧，不是 Admin 的 framing，上段的 ADR-035 65 字节帧与 `--idoris-admin-token-fd` 选择器不受影响。launcher 托管路径下 Electron main 不再持有 Admin token（R12-Q3），所以上段关于 Electron main 取 token 的 FD 交接在该路径下不再适用；ADR-035 本身是 Agent24 的内部决定，本修订不改它。§2.5「远程熔断开关」一条里的「当前 Admin v0 只读」同理，已在该处标注；§4 与 §5 表中「Admin 只读」的说法是对 I0 及历史材料的记述，不改。

### 2.2 凭证模块 vault（保留 J-4/J-5/J-7/J-8 的已接受设计，非 M4 已交付清单）

- **定位**：独立的 Rust crate，由 iDoris 开发，Agent24 作为消费方参与接口设计。一份加密存储（主密钥放在 Keychain，条目加密，只存密文），支持两种部署形态：
  - **(a) 嵌入 iDoris**：用于模型 provider 出站时注入凭证。**谁能用哪个 provider 由 iDoris 的虚拟 key 策略决定**（§2.1）。个人版仍然只有一个二进制。
  - **(b) 独立守护进程 / 出站代理**：供 Agent24 的工具、CLI、MCP 使用。**与 iDoris 网关是不同的进程/实例**。**谁能用哪把凭证由 Agent24 的 grant/审批签发调用方 token 与 scope 决定**，vault 只负责校验、注入和审计（单一策略源，G-1/G-2）。
- **覆盖范围（J-5）**：只管**出站第三方凭证**。Agent24 的内部令牌（daemon bearer、模块握手、A3 附着、Desktop host secret）和模块 actor key **不进 vault**。
- **凭证类别（J-7）**：
  - bearer 类：注入。
  - 身份/服务类签名（Nostr、SSH、HMAC/SigV4）：vault 代签，私钥不出模块。
  - **资产类**（钱包 owner key、EOA、AirAccount 签名）：只归 AirAccount（TEE KMS + passkey）。vault **不托管、不代签**；Agent24 只负责发起和过审批门，最终确认在用户的 passkey 上完成。
- **注入方式**：v1 先实现显式代理 + 请求头替换（OneCLI 模式：调用方只持有占位符）。之后补「启动时注入环境变量」模式，覆盖读环境变量的 MCP/CLI 和非 HTTP 协议。
- **fail-closed**：解析或注入失败时一律拒绝，**不回退去读环境变量**。
- **机制**：按次派发，短 TTL，落盘只存哈希；凭证优先级为 自有 key > 团队 key > 托管 key（借鉴 treg，只借鉴设计：treg 许可证有「禁止托管销售」条款）；余额不足返回结构化 402；审计只记元数据。
- **法律边界**：组织内部共享付费账号可以；**跨用户转售托管账号不做**（PGL「平台不充当资金池」）。
- **Agent24 侧前置（J-6）**：在 M4 接入 iDoris 之前，Agent24 修补 `shell_exec` 与 MCP 子进程继承环境变量的缺口（`env_clear` + 白名单，mcp.json 支持按 server 配置 env），防止 `IDORIS_API_KEY` 外泄。

**修订 1.6 增补（已冻结）**：vault 仍然没有接线，下面两件事都不经过它。（1）oMLX 上游 key 的注入，由「`provider.id` 等于 `omlx`」改为按卡的引擎种类识别：loopback 的、`engine_kind` 为 `omlx` 的卡（§2.12.1；这与 `GET /v1/models` 现有的 loopback `omlx@` 卡判据一致，v0.5.0 生成的卡因此不用改），这样 §2.3 的伴生卡 `omlx-embed` 才能带 key；**例外**：卡带 `extensions.idoris_upstream` 的一律不带（自定义上游不能靠把 `version_pin`、`provider.id` 写成 oMLX 来拿 key，勘误四）；key 的来源顺序仍是 §2.9「oMLX 密钥」，iDoris 私有引擎的 key 见 §2.7.7；其他引擎种类在 v0.6.0 不注入任何 key。（2）自定义 OpenAI 兼容上游（§2.3 的 `idoris_upstream`）在 v0.6.x 只做 loopback、不带密钥：非 oMLX 的卡没有任何凭据注入；需要密钥的上游，以及局域网、远程上游（隐私等级 `any`、egress 计数、同意流程），等 vault (a) 接线之后另行修订。

### 2.3 角色、实际模型与容量

稳定模型角色名为 `idoris/fast|daily|deep|vision|embed|rerank|decide|auto`。Agent24 决定 task→role：simple→daily，内部摘要/抽取/工具性调用→fast，complex→deep，Guardian→decide（M6 才考虑 systemone）。长上下文、工具、结构化输出走能力画像，不发明额外角色。既有语义：fast 以延迟为先（常驻 1–4B），daily 以面向人的质量为先（7–12B）；当前 catalog 标签与此有差距，重标须 R9 决策，不能用目录首项或硬编码排名补齐。

**运维 role map 目标**：仅在 HTTP 后端组件卡使用 `extensions.idoris_role_models`，由 iDoris 运维/自己的工具写配置；Agent24 只通过 HTTP 请求角色、展示实际模型，不写 YAML、不直接调加载/驱逐，也不使用库 API 旁路。每个 role（包括 `auto`）都要显式绑定具体 backend model ID；`auto` 不等于“选发现列表第一个”。缺绑定、未知/不可用模型或不符 local_only 时 fail-closed（无本地可用候选为 503），不跨角色猜测、不默认上云。候选支持范围与交付限制见 §4；不承诺订阅/进程后端也支持该扩展。

**修订 1.6 增补（已冻结）**：卡的 `extensions` 新增两个键，同 `idoris_role_models` 一样必须带 `_degradation`。`idoris_managed: {engine: <engine_kind>, mode: auto}`：该卡的引擎由 iDoris 托管（§2.7.6），v0.6.0 只有 `{engine: omlx, mode: auto}`；`engine` 是引擎种类（`engine_kind`，§2.12.1，开放集合），本版本不认识的取值按卡校验失败处理，启动失败，不静默忽略；可执行文件路径、模型目录这类宿主信息不进卡，端点取卡的 `endpoint`。`idoris_upstream: {kind: openai_compat, flavor: <引擎种类>}`：自定义 OpenAI 兼容上游（LM Studio 等），`flavor` 就是该卡的 `engine_kind`，必须与 `version_pin` 的产品名一致；`Form` 枚举与卡 schema 是冻结契约，不新增 kind，上游种类只放在这里；生成这类卡的最小版 `setup upstream` 是 v0.6.0 的承诺（形状见 §2.7.8）；需要更多形状（密钥、局域网、非 chat 角色）的版本后移，本修订不冻结。**每张引擎型卡的 `engine_kind` 恰好有一个来源**：有 `idoris_managed` 的卡取它的 `engine`；否则取 `version_pin` 里 `@` 之前的产品名（`omlx@0.6.4` 得 `omlx`，`lmstudio@unknown` 得 `lmstudio`；这与 v0.5.0 判断 oMLX 卡用的约定一致，已生成的卡不用改）；产品名不符合 §2.12.1 的格式时为 `unknown`（只作输出，不能写进 `idoris_managed.engine`，§2.12.1）；两者同时存在而不一致，卡校验失败。`setup` 生成的卡保证 `version_pin` 的产品名等于它的种类名。`embed` 角色在 HTTP 路径上由一张伴生 resident 卡承载：`setup omlx` 在给了 `--bind embed=<模型>` 时，在 `omlx.yaml` 之外再写 `omlx-embed.yaml`（没给 `embed` 绑定就不写，勘误四；`provider.id` 为 `omlx-embed`，`endpoint` 与 `omlx.yaml` 相同，能力只有 `embedding`，`load_policy` 为 `resident` 加 `keepalive.pinned: true` 加 `admission: coexist`；`extensions.idoris_role_models.embed` 绑定该具体 embedding 模型，这个绑定只在伴生卡上，主卡不再带它）。随包文件仍是 5 个，伴生卡由 `setup` 生成，并列入 `--force` 的整体替换范围（§2.7.8）。行为约定见 §2.11.3。

iDoris 拥有 catalog、推荐、实际模型身份、后端/量化选择以及分配额内的加载和驱逐。下载/预热/升级/回滚的功能归属也在 iDoris，但归属不表示已有下载服务。推荐 ID、provider ID、角色名与真实权重 ID 不可混为一谈；调用成功应使 load、上游 POST 和 `response.model` 的身份一致。

〔修订 1.6，已冻结：上段「下载/预热/升级/回滚的功能归属也在 iDoris，但归属不表示已有下载服务」中，预热有了接口 `POST /v1/models/warmup`（§2.11.2），随 iDoris v0.6.0；下载、升级、回滚仍然没有。该接口实现发版后，再以文字修订把上句的状态说法改为「预热已实现，下载仍未」。〕

`/capabilities` 用于展示、挂载期 `requires_models` 检查，**不逐次预检**（TOCTOU）；实际准入以调用结果为准。当前条目是 `{id,capability,resident,estimated_memory_gb,ctx_limit,queue_depth,admission_status}`，其中 admission 为 ready/requires_eviction/blocked。角色版 `{role,model,backend,locality,…}` 仅为目标，未实现，不据此生成 UI 假值。

### 2.4 主机事实、内存分配与费用

Agent24 拥有同机 host facts、core 占用与整机账，为 iDoris 划一个总 LLM allocation；iDoris 在该额度内用自己的全局 ledger 做模型准入，不让 Agent24 再做逐模型驱逐器。独立部署时 iDoris 可自探。不能把剩余额度冒充物理 `ram_gb`，min_ram 门仍按真实物理 RAM 判断。

已有启动输入 `IDORIS_ALLOCATED_BUDGET_BYTES` 为可选 u64 **整数字节**：缺省保留独立模式，0 是真零；畸形值启动失败；给了额度则推荐器不重复扣系统保留。内部模型 footprint 的十进制 GB 换算只在 iDoris 边界做一次。目标是推荐与加载准入均受同一额度约束；I0 只完成前者，不能声称后者已硬限。没有动态预算 HTTP API；未来预算桥/变更重启流程需单独交付。不发明当前不存在的 `agent24 hw budget` 或 `llm_allocated_bytes` 发送者。

费用账与内存账不同：iDoris 管付费 reserve/settle、价格、402 和 `X-iDoris-Cost-Minor`；Agent24 按模块计次数/token，只记录返回费用，不重算。402 为终态，不重试、不换 provider。

### 2.5 请求、隐私、响应与错误

- `POST /v1/inspect`（M5）：只做判定，返回 `{sensitivity: S0..S3, findings: [{kind, span?}], action: BLOCK|REDACT|ALLOW}`，不改写内容。
  - 契约已冻结（M5-01）：`packages/contracts/schema/inspect-request.schema.json`（`{input: string}`）与 `inspect-response.schema.json`，Rust 侧 `idoris_contracts::inspect`，两侧全部对象 `additionalProperties:false` / `deny_unknown_fields`。
  - `kind` 是封闭枚举（证件/手机/银行卡/统一社会信用代码/车牌/护照/港澳证件/邮箱/密钥类/凭据路径/`input_limit_exceeded`），新增类别属于契约变更。
  - `span = {start, length}`，按 **UTF-16 code unit** 计、半开区间 `[start, start+length)`，`length ≥ 1`；`input_limit_exceeded`（超限未扫描，按 X-1 直接 BLOCK）不带 span。
  - 响应里除枚举外没有任何字符串字段：命中原文、密钥子串不会出现在响应体，也不会经由响应进入日志。
  - 鉴权：与协议端点同一套 virtual key，但只看 key 的隐私上限，不看 role；`X-iDoris-Privacy` 缺省为 `local_only`。声明 `local_only`（或远程熔断已开启）时任何输入都判 `BLOCK`，只有声明 `any` 且 key 上限允许 `any` 才可能得到 `ALLOW`。调用方同意脱敏目前没有线上表示，因此该端点只会返回 `BLOCK`/`ALLOW`，不会返回 `REDACT`。

- **闸一在推理路径上的运行限制（I0 之后的 main 源码，不是 v0.3.0 发布能力；本节为静态源码核对，未作运行验收）**：
  - `/v1/chat/completions`、`/v1/embeddings`、`/v1/rerank`、`/v1/messages` 四个推理端点都在鉴权之后、路由策略（chat 另含意图识别）之前检查整个请求体——对象键、字符串与数字都会被提取、拼接后送入闸一判定。只要判定不是 `ALLOW`，就把这次请求钉成 `local_only`（只收紧，不放宽）；没有可用本地候选时上游收到 0 个请求，常见错误是 503 `local_only_unavailable`，也可能是 503 `no_eligible_candidate`（预算/准入/熔断排除），调用方不得假定只有单一 503 原因码。
  - 不透明内容的判定范围：闸一对上面提取并拼接的整段文本（含对象键，不限字符串值）做一次大小写不敏感的字面匹配 `data:…;base64,`，头部字符限定在 MIME token 字母表加 `/`、`;`、`=`，且要求逗号后有内容、其首字节是 ASCII 字母数字或 `+`/`/`；命中则整次请求按 S3/`BLOCK` 钉成本地。这条规则**不**声称覆盖全部 RFC 2397、浏览器或百分号编码写法。图片/音频/文件 content part（http(s) `image_url`、不带 `data:` 前缀的裸 base64 `source`/`input_audio.data`、文件 id）不会仅因其类型被钉成本地；token id 整数数组（任何长度、任何嵌套位置）也不会仅因是整数数组被钉成本地——数组里的每个数字值都按十进制文本、以 NUL 分隔后纳入检查，不代表其编码的原始内容已被还原或检查。以上都是当前实现的检测覆盖缺口，不是「已证明安全」：data URL 文本本身或解码后暴露的 PII 仍可能被其他规则独立命中并收紧，但不构成不透明性保证。需要「不透明即本地」的调用方应自行显式声明 `local_only`，不能依赖闸一兜底。
  - 配置了事件存储时，chat 与上述三个协议端点各在 `request.received` 之后、`profiled` 之前写一个 `inspected` 事件；写入失败在发送前返回 503。metadata 只有 `sensitivity`、`privacy`、`labels`（命中类别，可为空）与 `reason`；`reason` 优先级为扫描失败 `inspect_failed` > 远程熔断 `remote_kill_switch` > 内容收紧 `privacy_tightened_by_content`，不含原文或位置，也不区分「内容命中」与「不透明内容」两类收紧。
  - **远程熔断开关**：启动时 `IDORIS_REMOTE_KILL_SWITCH=1`（且只认这个精确值）让所有请求按 `local_only` 处理，同时跳过不可信远程卡的模型列举与启动探测；运行时切换依赖尚未定义的 Admin 写接口，当前 Admin v0 只读。〔修订 1.6，已冻结：Admin 在 §2.11.4 之后有受限写接口，但只限 `upstreams` 一族；熔断开关的运行时切换仍未定义，本条其余不变。〕
  - **provider 断路器**：二进制默认关闭，`IDORIS_PROVIDER_BREAKER=1` 才启用（共享 conformance 仍约定“持续 5xx 原样透传”）。启用后，OPEN 窗口内不向该 provider 发送请求，选路改试其他候选；选择阶段若最终仍无可用候选，可能返回 503 `no_eligible_candidate`，但初始快照里全部候选都被断路器排除并不等同于必然如此——`select_with_gate_fallback` 的重试快照只对非 resident HTTP 候选保留 blocked，resident HTTP 候选会被重新纳入选路，因此仍可能选中一个初始被断路器排除的 resident HTTP 候选，随后才在代理发送前被拒，返回 503 `provider_circuit_open`。调用方不得假定选择阶段与代理阶段的断路器判定之间存在单一确定性映射，也不能把这个保证推广到 Supervisor 或订阅路径。
  - 未接线：响应侧检查（`idoris-policy::credentials::response`）与凭据 broker/`secret://` 注入授权（`idoris-policy::credentials`）目前都只是纯核心，没有 HTTP/流式调用方接入；不能据此声称响应内容或已知凭据注入已在运行时受检。

- 原型请求为 `model=idoris/<role>` 加 `X-iDoris-Privacy`、`X-iDoris-Complexity: simple|complex`、`X-iDoris-Request-Id`。Intent/Capabilities/Fallback 等只按真实需求加法接入；关联 Session/Trace-Id/Request-Id 由内核生成，不信任模块。`X-iDoris-Caller` 可选且只作元数据，永不作为策略依据。近期 Agent24 接入不做流式。
- 隐私只能维持或收紧。声明角色的调用不从内容重猜 task/role；显式 auto 才委托角色选择，仍不放宽 privacy。未声明角色的其他调用方可由静态规则/判定器选角色。处理顺序：隐私 → 角色/能力 → 付费预算 → 容量 → 选择/声明的降级；预算不足不被静默回落掩盖；仅显式声明 `X-iDoris-Fallback: next_in_chain` 才允许 iDoris 回落到免费候选，并在响应中说明降级。此授权不允许 Agent24 改走另一 provider；I0 router 的实现差距见 §4。
- `X-iDoris-Served-Locality: loopback|lan|remote` 表示**推理实际位置**。仅选定后端后有此头，后端错误也带；选择前拒绝可缺失。idoris-local 只接受 loopback，缺失/lan/remote 都按错误，结果不返调用者。完整 any provider 的 lan 及缺头均保守计 remote。网关地址在 loopback 不证明推理在本机，亦不证明字节没有出网。
- 所有响应带 `X-iDoris-Record-Id`；缓存还带 `X-iDoris-Cached: true`、`X-iDoris-Origin-Record-Id` 供去重。`/health` 返回 service/version/contract_version/instance_id；开启开发免 key 时启动日志及 health 的 `dev_no_key_enabled` 会明示；活着不代表 backend/model ready。身份自报不是对抗恶意本机端点的认证。〔修订 1.6，已冻结：`/health` 在上述字段之外新增 `phase`、`ready`、`started_at_ms`、`registered`、`upstreams`（§2.11.1）。`status` 仍恒为 `"ok"`，含义仍是「进程活着并在服务」，**不是**「模型可用」；「模型可用」看 `ready`（等于 `phase=="ready"`）。`/health` 仍免鉴权，`instance_id` 仍是公开标识，`instance_nonce` 永远不出现在 `/health` 或任何响应里。〕
- 错误体采用 `{error:{type,rule_id,reason_code,evidence,remediation}}` 的结构化方向，实际各端点字段以源码/fixture 为准；不得假定所有字段处处存在。402/403/413/400 unsupported field 为终止错误，不重试、不换 provider；503 local_only_unavailable 在独占路径明确失败。rule_id/evidence/remediation 仅在管理页展示，不透给模块。
- iDoris 已有 JSON/SSE 路径与取消处理；不得把未来 Agent24 非流式首片当作禁止 iDoris SSE。已开始 token 后不重试；字段支持必须逐项证明。I0 Supervisor 路径会拒绝 max_tokens，不能宣称完整 OpenAI 兼容。

### 2.6 审计、轨迹、敏感确认与后续能力

Agent24 事件日志是 Evolver 主数据源，运行期记忆不是训练副本。双方审计不合并、不互抄，仅 record_id/trace_id 关联。默认不推送 trajectories；iDoris 用户开启 full/训练且完成确认后才考虑 ATIF v1.8 交换，命名空间分别 extra.idoris/extra.agent24，互不写入。feedback 方向已接受，Agent24 等有反馈 UI 再接。

**M5 既定决定不变**：敏感开关只由用户在 iDoris 本地控制台完成一次性确认；Agent24 可发起/展示 diff/深链接，不能代理确认。未来“有持续时间的 caller consent”仍延期，不在本轮改成已接受机制。Admin proposals/confirm、inspect、vault、kill switch、训练/晋升与 systemone 均须依各里程碑/API 单独交付，不因职责表而成为 v0.3.0 能力。本地判定器不可用须显式报错，不静默降级；判定 engine 如实返回。

### 2.7 launcher 托管契约（修订 1.3，iDoris v0.4.0 起，#588）

agent24d 托管 iDoris 时，按下表执行。表中都是已冻结的接口，改动必须走本规范的修订。（修订 1.6 的增补与被取代的句子见表后，已冻结。）

| 环节 | 契约 |
|---|---|
| 下载 | Release 资产为 `idoris-v<ver>-aarch64-apple-darwin.tar.gz` 和同名 `.sha256`。launcher 锁定 tag 和 sha256，并校验 `codesign` 的 TeamIdentifier 为 `HYAP444FN9`。只支持 v0.4.0 及以上版本 |
| 布局 | 包里恰好有 5 个文件：`idoris`、`config/{catalog,routing-policy}.yaml`、`config/components/{omlx,subscription}.yaml`。配置路径相对可执行文件所在目录解析，与 cwd 无关 |
| 组件配置 | `idoris setup omlx --endpoint <loopback url> --out-dir <dir> [--bind role=model]… [--load-mode on_demand\|resident] [--api-key-stdin] [--force]`，输出 JSON。共 11 个错误码：`unreachable`、`auth_required`、`auth_failed`、`not_omlx`、`model_not_found`、`invalid_endpoint`、`invalid_role`、`out_dir_not_empty`、`validation_failed`、`write_failed`、`invalid_api_key_input`。只会写 out-dir 及其同级的 `.idoris-setup-*` 暂存目录。`--force` 的替换要么全部成功，要么全部不生效。key 只从 stdin 读入，只用于探测。运行时的 oMLX key 仍走 `IDORIS_OMLX_API_KEY`。Agent24 不写 YAML |
| 数据 key | `idoris key issue --spec-stdin` / `key list` / `key revoke <vk_…>`，离线运行，不需要 serve 在跑，也不取实例锁，但必须和 serve 使用同一个 `IDORIS_DB_PATH`。吊销立即生效，可以在线轮换。owner 字段建议用唯一值 `agent24d:<install_id>:<nonce>`，签发结果丢失时可以按 owner 找到对应的 key 再吊销 |
| 启动 | `idoris serve --admin-token-stdin`，进程常驻前台。Admin token 由 launcher 用 CSPRNG 生成，格式为 64 位小写 hex 加 LF，共 65 字节，10 秒内写入 stdin，不得出现在 argv、环境变量、文件或日志里 |
| 就绪 | 轮询 `GET /health`：`status=="ok"`，`version` 符合锁定的版本，`contract_version` 的 major 为 1，并且进程仍然存活。stdout 不属于契约。启动前应确认 oMLX 可达 |
| 停止 | SIGTERM 和 SIGINT 都会优雅停机，总时长约 15 秒（10 秒加上回收 runtime 子进程的 5 秒）。launcher 应先发 SIGTERM，17 秒后仍未退出再发 SIGKILL |
| 退出码 | 0 表示正常；1 表示启动失败或停止失败（停止超时会输出 `[idoris] 停止失败：…`）；3 表示同一数据库已有实例在运行（按 canonical 路径判断，符号链接视为同一个数据库） |
| 不承诺 | 自动升级、一致备份和回滚（M-IDL2 再做）；aarch64 以外的平台 |

**修订 1.6 增补（已冻结；iDoris v0.6.0 目标）。** 上表各行的既有承诺全部保持。下面是增补，按上表的行对应；被取代的句子保留在上表原文里，在这里逐条注明。本修订引用设计稿 §3.2.0 的决策时一律写作「设计稿 Dn」，它与本规范既有的 D-1…D-7 是两套编号。

| 上表的行 | 修订 1.6 增补 | 被取代的句子 |
|---|---|---|
| 启动 | 新增 `idoris serve --launcher-stdin`（§2.7.1）；`--admin-token-stdin` 与 65 字节旧帧不变，二者互斥 | 无 |
| 就绪 | 两级就绪（§2.7.4）：「iDoris 可用」是本行判据加 Admin 可答，「模型可用」另看 `/health.phase` | 「启动前应确认 oMLX 可达」：对声明了托管的卡不再适用，iDoris 自己探测、拉起或报因（§2.7.6） |
| 停止 | 自有引擎一并停止，分配在原 17 秒预算内（§2.7.5） | 无 |
| 退出码 | 新增 4、5，并增加 1 与 3 的触发条件（§2.7.5） | 无，0、1、3 的既有含义不变 |
| 组件配置 | `setup omlx` 扩展（§2.7.8）；11 个既有错误码的语义不变 | 无 |
| 布局 | 随包仍恰好 5 个文件，伴生卡等由 `setup` 生成（§2.7.9） | 无 |
| 不承诺 | 更新（§2.7.9） | 无，只增加不承诺项 |
| （新增） | 动态端口（§2.7.2）、发现文件（§2.7.3）、登记（§2.10）、引擎生命周期（§2.7.6，首个实现是 oMLX）、引擎私有 key（§2.7.7）、多引擎预留（§2.12） | — |

#### 2.7.1 `--launcher-stdin` 帧（已冻结）

`idoris serve --launcher-stdin`，与 `--admin-token-stdin` 互斥，同时给出是用法错误，退出码 1。旧 flag 与旧 65 字节帧完全不变。

- 字节层：一行 UTF-8 JSON，以 LF 结尾，总长（含 LF）≤ 4096 字节，10 秒内读完。读到第一个 LF 就停，之后不再读 stdin。launcher 写完就关闭 stdin 是预期行为，**stdin 读到 EOF 不代表 launcher 已经不在**，iDoris 不据此判断孤儿，孤儿只看登记会话（§2.10.8）。读取位置与旧帧相同：端口绑定之后、对外服务之前，所以帧无效时在服务任何请求之前以退出码 1 退出。

| 字段 | 类型 | 必填 | 约束 |
|---|---|---|---|
| `v` | integer | 是 | 必须等于 1 |
| `admin_token` | string | 是 | 64 位小写 hex，同 `--admin-token-stdin` 的 token（上表「启动」行）；会话内可被 `set_admin_token` 替换（§2.10.6） |
| `discovery_path` | string | 否 | 绝对路径，≤ 1024 字节，不含 NUL，规则见 §2.7.3；缺省则不写发现文件 |
| `register_socket` | string | 否 | 绝对路径，≤ 103 字节（macOS `sun_path` 为 104 字节含 NUL），不含 NUL。**出现则 `discovery_path` 必填**，否则 agent24d 拿不到 `instance_nonce`；缺省则不登记 |
| `orphan_timeout_s` | integer | 否 | 0 到 86400；0 或缺省 = 不因「无登记会话」退出（§2.10.8）。**非 0 则 `register_socket` 必填**，否则永远登记不上，到时间就会自行退出。Agent24 取 900 |
| `instance_nonce` | string | 否 | 64 位小写 hex；缺省由 iDoris 生成，生产用法不给这个字段，只给确定性测试用 |
| `engine` | object | 否 | `{"executables"?: {"<engine_kind>": 绝对路径, …}, "stop_on_exit"?: "owned" 或 "never"}`。`executables` 的键是引擎种类（§2.12.1），v0.6.0 只认 `omlx`，其他键按 `field_invalid`（`engine.executables`）拒绝；值是 launcher 已校验过的可执行文件，视为可信，不再校验团队。未知子字段同样拒绝；缺省 `stop_on_exit` 为 `"owned"`（§2.7.6）。加引擎种类只是 `executables` 多一个键，不改这个字段的形状 |

- 解析严格：未知字段一律拒绝。launcher 与 iDoris 是锁版本一起发的，字段拼错而静默失效，比直接失败更危险（这与 §2.10 的 UDS 帧「忽略未知字段」是两种有意的不同）。任何错误都只输出固定文案 `[idoris] 启动失败：launcher 帧无效（<原因码>[：<字段名>]）`，退出码 1，**绝不回显字段值**（不得把解析库的默认错误信息带出，里面会有输入片段）。原因码取值：`frame_too_long`、`frame_not_json`、`unsupported_frame_version`、`field_invalid`、`unknown_field`、`timeout`。
- 已否决的做法：argv 传路径（`ps` 可见，且 `parse_args` 是精确匹配）、环境变量（会被 iDoris 自己的子进程继承）、替换旧 65 字节帧（破坏 ADR-035 与既有测试）。

示例（一行，token 只是占位，行末是一个 LF 字节）：

```
{"v":1,"admin_token":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","discovery_path":"/Users/x/.agent24/run/idoris.json","register_socket":"/Users/x/.agent24/run/idoris-register.sock","orphan_timeout_s":900}
```

#### 2.7.2 动态端口（已冻结）

- launcher 托管路径下，数据面与 Admin 都可以绑定端口 0：`IDORIS_PORT=0` 与 `IDORIS_ADMIN_PORT=0` 被放行（v0.5.0 的解析会拒绝 0）；原有 1 到 65535 的取值和固定端口路径不变。端口 0 由内核分配，iDoris 以绑定之后的 `local_addr()` 为准；两个监听器仍然只绑 loopback。
- 实际端口只经发现文件（§2.7.3）和登记帧（§2.10.3）传出。stdout 不属于契约（上表「就绪」行），动态端口下不得靠它取端口；请求值 `IDORIS_PORT` 是 0，也不是端口。
- 不承诺 `idoris admin <资源> --token-stdin` 在动态端口下自动从发现文件取端口（§2.7.9）。

#### 2.7.3 发现文件（已冻结）

发现文件是 iDoris 就绪后写给 launcher 的一份实例快照，**只是提示，不是事实来源**：同一个数据库只能有一个实例，由实例锁保证（退出码 3），孤儿接管不会因为发现文件而起出第二个实例。

- **路径**：launcher 在帧里给绝对路径（Agent24 约定 `<data_dir>/run/idoris.json`）。iDoris 没有默认路径，帧里没有 `discovery_path` 就不写：iDoris 不知道 Agent24 的数据目录，在自己的状态目录放默认文件会产生第二个事实来源。
- **路径校验**（都在写任何东西之前，失败即退出码 1，此时尚未对外服务）：（1）绝对路径、≤ 1024 字节、不含 NUL；（2）`parent = dirname(path)`，要求 `canonicalize(parent)` 与 `parent` 逐字节相等，即中间不允许有符号链接，launcher 必须传 canonical 路径（macOS 的 `/var` 实为 `/private/var`，传 `/var/folders/…` 会被拒）；（3）以不跟随符号链接的方式打开 `parent`，属主必须等于当前有效 uid，且组与其他人不可写（`mode & 0o022 == 0`，建议 0700）；（4）目标若已存在，必须是属主正确的普通文件（不是符号链接、目录、FIFO、socket），否则拒绝并退出，不删除；（5）iDoris 不创建 `parent`，由 launcher 负责，避免在错误位置建出权限不对的目录。
- **内容**：紧凑的一行 JSON 加 LF，≤ 4096 字节，UTF-8，权限 0600。

```
{"schema":1,"service":"idoris","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","instance_nonce":"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f","pid":41234,"data_port":51234,"admin_port":51235,"version":"0.6.0","contract_version":"1.3.0","started_at_ms":1763625330123,"launch":{"db_path":"/Users/x/.agent24/idoris/idoris.sqlite3","state_dir":"/Users/x/.agent24/idoris/state","components_dir":"/Users/x/.agent24/components/idoris-config","allocated_budget_bytes":17179869184,"engines":[{"id":"omlx","engine_kind":"omlx","managed":true},{"id":"omlx-embed","engine_kind":"omlx","managed":false}]}}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `schema` | integer | 固定 1 |
| `service` | string | 固定 `"idoris"` |
| `instance_id` | string | UUID v4，36 字符，等于 `/health.instance_id` |
| `instance_nonce` | string | 64 位小写 hex，**秘密**（§2.1） |
| `pid` | integer | iDoris 进程号 |
| `data_port`、`admin_port` | integer | 1 到 65535，取自绑定之后的 `local_addr()`，不是请求值 |
| `version`、`contract_version` | string | 等于 `/health` 同名字段 |
| `started_at_ms` | integer | iDoris 开始 `run()` 时的 Unix 毫秒（墙钟）。时间字段名用 `started_at_ms`，不用 Agent24 方案里的 `started_at`（R12-Q2） |
| `launch` | object，可选 | 启动输入快照：`db_path`（canonical 锁基路径）、`state_dir`、`components_dir`（canonical）、`allocated_budget_bytes`（u64 或 null）、`engines`（数组，启动时从组件卡读到的引擎型上游的静态清单，元素 `{id, engine_kind, managed}`：`id` 是上游 id，`engine_kind` 见 §2.12.1，`managed` 同 `/health.upstreams[].managed`；没有引擎型上游时为空数组；不含状态，也不含默认引擎，这两样读 `/health` 与 Admin）。供 agent24d 判断「这个孤儿的启动输入还是不是我要的」：不一致就重启，不接管 |

不含 `pid_start`：iDoris 取自己的内核启动时间需要不安全代码，而 workspace 禁用 unsafe。消费者自己读内核启动时间，与 `started_at_ms` 粗略比对（几秒内）即可；pid 复用由登记（HMAC）与 `/health.instance_id` 兜底。

- **原子写法**（设计稿 D3）：路径校验通过后，以 `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` 打开 `parent` 得到 `parent_fd`；此后创建临时文件、`fsync`、重命名、删除**全部基于 `parent_fd`**（`openat`、`renameat`、`unlinkat`），不再按路径字符串操作，否则校验与重命名之间 `parent` 被换成符号链接，重命名会落到别处。在 `parent_fd` 下创建不可预测名字的临时文件 `.idoris-discovery.<pid>.<16 位随机 hex>.tmp`（`O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`，权限 0600，并 `fchmod` 到 0600 以不受 umask 影响），写入并 `fsync`，`renameat` 覆盖目标，再尽力 `fsync(parent_fd)`；失败路径 `unlinkat` 临时文件。读者要么看到旧文件，要么看到完整的新文件。不用 `F_FULLFSYNC`：重启后 pid 本来就没了，不需要掉电后文件仍在。
- **何时写**：同时满足（a）已持有实例锁，（b）数据面与 Admin 都已绑定，（c）服务已启动且 `/health` 能答（含 `phase:"starting"`）。登记（§2.10）在写文件之后。launcher 要求了发现文件而写失败，就是启动失败（退出码 1，固定文案），不允许「跑着但找不到」。
- **自愈**：每 10 秒（与是否已登记无关）`lstat` 目标并核对 `instance_id`，缺失或不一致就重写，`rm -rf` 误删可以恢复（收到 `instance_conflict` 之后不再重写，§2.10.4）。只比对 `instance_id`；目标是符号链接、目录或属主不对时只报错，不删除、不覆盖（§2.10.10）。
- **何时删**：优雅退出（SIGTERM、SIGINT、孤儿超时、退出码 5 与 `instance_conflict` 的路径）时，在监听关闭之后、实例锁释放之前，**仅当文件里的 `instance_id` 等于自己**才删；读、比对、删除在同目录旁路锁文件 `<发现文件>.lock` 的独占 flock 下完成。SIGKILL、panic、掉电不删，即「崩溃残留」。
- **残留的识别**（消费者侧，建议顺序）：读文件（≤ 4 KiB、普通文件、属主正确、`schema==1`）；`kill(pid, 0)` 存活、uid 相同、可执行路径等于安装路径；`GET /health`（不带任何凭据，2 秒）返回 `service=="idoris"` 且 `instance_id` 相等；LISTEN 属主核对；最后以登记为准（§2.10.9）。任何一步失败即视为陈旧，pid 已死才可以删文件。删除残留的发现文件时，同样要持有旁路锁 `<发现文件>.lock` 并复核 `instance_id`（Agent24 2026-11-20 已确认，R12 已确认项 4）。
- **iDoris 自己遇到陈旧文件**：实例锁是唯一事实来源。拿到锁的新实例把已有文件视为陈旧并覆盖；没拿到锁（退出码 3）的实例**不碰**文件，因为它属于还活着的那个实例。

#### 2.7.4 两级就绪（已冻结）

托管引擎冷启动可能超过 launcher 现有的就绪窗口（例如 oMLX：Python 加 MLX 加载加扫描模型目录），所以「iDoris 可用」与「模型可用」分开判断（R12-Q4）：

| 级 | 判据 | 用途 | 时限 |
|---|---|---|---|
| iDoris 可用 | 上表「就绪」行的判据（`status=="ok"`、`version` 符合锁定版本、`contract_version` 的 major 为 1、进程仍存活），并且 Admin `GET /admin/api/v1/status` 以 Admin token 能答 200 | Agent24 发布网关槽位（发布之前还须通过 §2.10.9 的核对） | 沿用 launcher 自己的就绪窗口（Agent24 现为 30 秒，属 Agent24 内部参数，不在本契约内冻结），超时按启动失败处理 |
| 模型可用 | `/health.phase=="ready"`，即 `ready==true`（§2.11.1） | Agent24 的 chat 放行 | `starting` 最长到引擎启动超时（oMLX 默认 90 秒，各引擎种类各有默认值），期间界面显示「模型服务启动中」，**不得**因此把 iDoris 判为失败并重启 |

- `/health.status` 仍恒为 `"ok"`，只表示进程活着并在服务（设计稿 D8）；`phase` 与 `ready` 才表示模型可用。「iDoris 可用」之后、「模型可用」之前，数据面除 `/health` 外的路径返回 503 `idoris_starting`（§2.11.1）。
- Agent24 侧承诺（Agent24 2026-11-20 已确认，含 R12 已确认项 1 的细化）：chat 只有在 `phase=="starting"`，或有上游处于 `restarting` 时才有界等待，上限 120 秒，超时向调用方返回 503 加 `retry_after`（这是 Agent24 自己的响应，不是 iDoris 的）。`phase=="degraded"` 且所有上游的 `reason` 都是终态原因（`engine_not_found`、`auth_required`、`auth_failed`、`budget_zero`）时不等待，立即返回 503 并带上该 `reason`，界面按 `reason` 引导用户；`engine_not_found` 引导安装引擎，是哪一种引擎看同一个上游元素里的 `engine_kind`（§2.11.1）。

#### 2.7.5 停止顺序与退出码（已冻结）

**停止**：SIGTERM 与 SIGINT 仍走同一条优雅路径，总预算不变（iDoris 自己约 15 秒，即排空 10 秒加销毁 runtime 5 秒；launcher 17 秒后才 SIGKILL）。收到信号后并行执行：（1）向 agent24d 发 `bye`（仅限优雅停止——信号触发或退出码 5——且存在已 `ack` 会话，至多 0.5 秒，§2.10.8）；（2）停止自己拥有的引擎（SIGTERM 整个进程组，8 秒宽限，之后 SIGKILL）；（3）HTTP 排空（沿用现有 10 秒上限）。然后：（4）删发现文件（监听关闭之后、实例锁释放之前，仅当 `instance_id` 匹配，§2.7.3）；（5）销毁 runtime（Drop 会 SIGKILL 剩余进程组，是最后一道兜底）；进程退出时内核释放实例锁。孤儿超时、退出码 5 与 `instance_conflict` 的退出（§2.10.4）走同一条路径。`engine.stop_on_exit` 为 `"never"` 时跳过（2），它只对 SIGTERM、SIGINT 的正常退出（退出码 0）有效；退出码 4、5，以及因 `instance_conflict` 退出的 3，一律停止自己拥有的引擎。

**退出码**（0、1、3 的既有含义不变，其中 1、3 增加了触发条件；新增 4、5）：

| 码 | 含义 | launcher 行为 |
|---|---|---|
| 0 | 正常停止（SIGTERM、SIGINT） | 按预期 |
| 1 | 启动失败或停止失败。修订 1.6 增加：launcher 帧无效、发现文件路径校验或写入失败 | 按崩溃处理，退避重启 |
| 3 | 同一数据库已有实例在运行。修订 1.6 增加：登记被 agent24d 以 `instance_conflict` 拒绝，即 agent24d 已有另一个健康实例（§2.10.4；定义是 Agent24 2026-11-20 确认的，iDoris 侧处理 Agent24 已同意冻结） | 不要重启：前一种情形去接管那个实例，后一种情形 agent24d 已在监管它 |
| 4（新） | 孤儿超时：launcher 给了 `orphan_timeout_s`，超过该时间没有已 `ack` 的登记会话（§2.10.8），主动优雅停止 | 按预期停止，**不计入**崩溃熔断（R12-Q5） |
| 5（新） | iDoris 自己拉起（`owned`）的引擎失效，v0.6.0 无法在进程内恢复，优雅停止以让上层重启（§2.7.6） | **计入**崩溃熔断（R12-Q5），重启后走全新的启动门控 |

修订 1.6 勘误（不改 wire）：退出码 3、4 的停止过程中，HTTP 排空失败（超时或服务器报错）时，仍以 3、4 退出，另记一行排空失败日志；只有信号（SIGTERM、SIGINT）触发的停止遇到排空失败，才以 1 退出。

#### 2.7.6 引擎生命周期（首个实现是 oMLX）（已冻结）

**范围与已定决策。** 「引擎」指 iDoris 经 HTTP 对话的本地推理服务进程，种类用 `engine_kind` 区分（§2.12.1，首个实现是 `omlx`），「模型」指引擎里的权重。本节的流程是**通用的**：引擎管理器按「引擎实例」建模，每个实例带自己的 `engine_kind`、状态机、所有权记录与日志，并按种类分派到该种类的「配方」（怎么探测、怎么发现与校验可执行文件、怎么拉起、是否支持私有 key）。v0.6.0 只实现 `omlx` 配方（本节标明「oMLX 配方」的段落）；以后加引擎种类只是增加配方，不改本节的通用字段与流程（owner 2026-11-20 的多引擎需求，§2.12）。现有 Supervisor 管模型（load、unload、chat），引擎管理器在它之下，只管进程；`OmlxAdapter` 继续用 HTTP 与 oMLX 引擎对话，不变。owner 已定：（设计稿 D2a）adopt 优先，找不到才拉起，拉起的实例是 iDoris 私有的；（设计稿 D2b）v0.6.0 的引擎由用户自装，iDoris 探测并报 `engine_not_found`，由 Agent24 界面引导安装，附带或按需下载引擎以后再议。**只对声明了 `extensions.idoris_managed` 的卡生效**（§2.3）；没有托管卡时完全走 v0.5.0 的现有路径，行为不变。

**启动决策。** 对每张托管卡，在创建 Supervisor 之前依次：

1. 读所有权记录（见下）。这一步必须先于探测：`kill -9` 之后遗留的自有引擎仍在监听，先探测会把它当成外部引擎，或者因为它的私有 key 而得到 401。记录存在且进程身份全部吻合，就接管自己遗留的引擎（`owned=true`，沿用记录里的 key，进入第 2 步用该 key 确认就绪）；记录存在但进程不吻合，丢弃记录；吻合但存在 `load.pending` 围栏时按下文处理。
2. 按该种类配方的判据探测卡的 `endpoint`（oMLX 配方见下），key 按配方的顺序解析（oMLX 是 §2.9「oMLX 密钥」，接管了遗留引擎时用记录里的 key）。判据通过：就绪；接管的遗留引擎 `owned=true`，其余是外部引擎，`owned=false`，**iDoris 永远不给外部引擎发信号，退出时也不停它**。401/403 且没有可用 key：`unavailable`，`reason:"auth_required"`（或 `auth_failed`），**不再拉起第二个**。连接被拒：进入第 3 步。其他，即端口被不是这种引擎的服务占用（含形状不符）：`failed`，`reason:"port_in_use"`。
3. 按配方发现并校验可执行文件（oMLX 配方见下）。找不到：该上游 `absent`，`reason:"engine_not_found"`，iDoris 本身继续服务；`/health` 与 Admin 的同一个上游元素里的 `engine_kind` 说明是哪种引擎没找到，由 Agent24 界面据此引导用户安装那一种（§2.11.1）。
4. 按配方拉起（oMLX 配方见下）并等就绪：探测同第 2 步，启动超时（oMLX 默认 90 秒）内轮询，间隔从 250 毫秒退避到 1 秒。

**oMLX 配方 · 识别与探测。** `GET /v1/models`（key 按 §2.9「oMLX 密钥」的顺序）返回 200 且 JSON 里有 `data` 数组，就算就绪，空列表也算（模型目录可能是空的）；判据同 `setup omlx`，另要求 `/v1/models/status` 与 `/api/status` 也是 oMLX 的形状，形状不符按「端口被不是这种引擎的服务占用」处理。oMLX 没有 `/health`。

**oMLX 配方 · 可执行文件的发现与校验。** 发现顺序：（1）stdin 帧的 `engine.executables.omlx`（launcher 已校验，视为可信）；（2）`/Applications/oMLX.app/Contents/MacOS/omlx-cli`；（3）`~/Applications/oMLX.app/Contents/MacOS/omlx-cli`。**不使用 PATH 查找**：Agent24 启动 iDoris 时把 PATH 固定为 `/usr/bin:/bin:/usr/sbin:/sbin`，Homebrew 路径本来就不在里面；`~/.omlx/bin/omlx`、`/opt/homebrew/bin/omlx`、`/usr/local/bin/omlx` 只是指向 app 内 `omlx-cli` 的包装或链接，不作为发现目标。校验：`canonicalize` 之后必须是普通可执行文件；位于 `.app` 内时用 `/usr/bin/codesign --verify --strict` 并要求 `TeamIdentifier` 为 `PSK5Q5T46L`；launcher 显式给的路径不再校验团队。以上路径与团队标识来自设计稿附录 A 在 oMLX 0.6.4 上的只读实测，其他版本未实测，oMLX 换签名团队会使校验失败。**oMLX 配方 · 落盘行为与 iDoris 自有 base**（oMLX 0.6.4 的行为，来自设计稿 §2.7.3）：`omlx serve` 只要收到 `--host`、`--port`、`--model-dir`、`--memory-guard-gb` 任何一项，就把它们写入 `<base>/settings.json`；`<base>` 依次取环境变量 `OMLX_BASE_PATH`、oMLX.app 的 bootstrap 文件（`~/Library/Application Support/oMLX/base-path` 的内容）、`~/.omlx`。iDoris 每次拉起都会传 `--host` 与 `--port`，若不指定 base，就会改写用户自己的 `~/.omlx/settings.json`（或 bootstrap 指向的目录）。所以：（1）iDoris 拉起 oMLX **必须使用自有 base**：`OMLX_BASE_PATH` 指向 `<IDORIS_STATE_DIR>` 下的专用目录（实现的目录名是常量 `BASE_DIR_NAME`，即 `<IDORIS_STATE_DIR>/omlx-base`），目录权限 0700、属主为当前用户、不是符号链接。核对方式是对路径做 `lstat`（不跟随链接）：存在、是目录、不是符号链接、属主是当前用户、组与其他人没有任何权限，每次拉起前各核对一次；iDoris 不持有该目录的 fd（oMLX 之后按路径自己重开，持有 fd 也挡不住那个窗口）；（2）没有配置出这个目录，或校验不过，**拒绝拉起**（`reason:"base_unavailable"`），不回退到 `~/.omlx`，也不读取 bootstrap 文件；（3）托管引擎的模型目录取 iDoris 运行用户的 `~/.omlx/models`（与 §2.9 的本地模型列表是同一个目录），拉起时显式传 `--model-dir <该目录>`；`setup` 的离线 `--bind` 核对同一个目录（§2.7.8）；不新增帧字段、环境变量或 `setup` 旗标（2026-11-21，S4 验收定案；取代此前「由部署方显式给出模型目录，不给就用自有 base 下的默认模型目录」）。`--model-dir` 只会被写进 iDoris 自有 base 的 `settings.json`，不改动用户目录里的任何设置。自有 base 下的 `models/` 起初是空的，只有不传 `--model-dir` 时才是引擎的模型目录；托管总是传，所以引擎看得到用户已有的模型。oMLX 把 `--model-dir` 的值按逗号拆成多个目录，所以模型目录路径**不得含逗号**：`setup` 在托管时校验该路径是绝对路径且不含逗号，违反报 `validation_failed`（`~/.omlx/models` 取不到，即 HOME 未设置或不是绝对路径，同样报该错误），排在找端口、找引擎之前；（4）用户 oMLX 设置里的 `skip_api_key_verification`、子 key（sub keys）等只存在于用户自己的 base，**不影响 iDoris 私有引擎**：私有引擎只读自有 base 的设置，鉴权只认 iDoris 传入的私有 key（§2.7.7）。iDoris 不读、不校验自有 base 里的 `settings.json`；自有 base 由 iDoris 创建，其中 `skip_api_key_verification` 与 sub keys 取默认值，iDoris 自己不写它们（静态阅读 oMLX 0.6.4 的结论见 §2.7.7）；（5）落盘的设置跨启动生效：这次传过 `--model-dir` 或 `--memory-guard-gb`，下次不传，引擎用的仍是落盘的值，不传不等于恢复缺省。iDoris 的约定是每次拉起都显式传 `--host`、`--port`、内存护栏（有额度 `--memory-guard-gb`，没有额度 `--memory-guard balanced`）与 `--model-dir`，所以这四项落盘的永远是**本次**的值，不会沿用上一次的（勘误六；此前的约定只在有额度、部署方显式给了才传，会悄悄沿用旧值）；同一状态目录下的多个 oMLX 实例要各用各的 base（共用会互相覆盖 `settings.json`，还共用缓存与日志）；（6）**base 之外的写入**：oMLX 0.6.4 的 `serve` 启动时还会按 `HOME` 写 `~/.omlx/bin/omlx-cluster-python`（让集群里别的 Mac 经 SSH 找到解释器的辅助启动文件，不是设置），`OMLX_BASE_PATH` 管不到；iDoris 接受这一点，视为已知行为，不为此改写 `HOME`，因此不承诺 oMLX 只写 base。

**oMLX 配方 · 启动命令**（flags 已对照 oMLX 0.6.4 的 `omlx serve --help`，其他版本未实测）：

| 项 | 取值 |
|---|---|
| 可执行文件与参数 | 上面解析出的 `omlx-cli`；`serve --host 127.0.0.1 --port <卡 endpoint 的端口>`；设置了 `IDORIS_ALLOCATED_BUDGET_BYTES` 且换算后（GiB，即 2^30 字节，向下取整）≥ 1 时再加 `--memory-guard-gb <整数>`，**没有设置额度时改传 `--memory-guard balanced`**（oMLX 自己的缺省档位：设置了又去掉额度之后，落盘的自定义上限不会留下来）；再加 `--model-dir <模型目录>`（上面（3）：iDoris 运行用户的 `~/.omlx/models`，每次拉起前再取一次 S4 的 `managed_models_dir()`，托管总是带，路径含逗号一律拒绝）。**这两项每次拉起都传** |
| 环境 | `env_clear`，只放 `PATH=/usr/bin:/bin:/usr/sbin:/sbin`、`HOME`、`TMPDIR`、`LANG`、`LC_ALL`、`LC_CTYPE`、`TZ`、`OMLX_API_KEY`（§2.7.7）、`OMLX_BASE_PATH`（iDoris 自有 base，必放，见上）。iDoris 自己的 `IDORIS_*` 一个都不传给引擎 |
| 工作目录与 stdio | 工作目录为状态目录；stdin 为 null；stdout 与 stderr 追加到 `<IDORIS_STATE_DIR>/logs/<engine_kind>.log`（oMLX 即 `omlx.log`，超过 10 MiB 滚动，保留 3 份） |
| 进程组 | 独立进程组；停止时 SIGTERM 整个组，8 秒宽限后 SIGKILL |

oMLX 配方：`--memory-guard-gb` 让 oMLX 自己的内存上限与 iDoris 的额度账本一致，这是 iDoris 托管比外部引擎更稳的一个具体理由；oMLX 把该值当正浮点数，传 0 会启动即退出；iDoris 按 GiB 向下取整，偏保守，额度不足 1 GiB 时**拒绝拉起**（`reason:"budget_zero"`），因为什么都装不下。不使用 `--api-key` 命令行参数（argv 同 UID 可见）。

**所有权记录与孤儿引擎。** iDoris 被 SIGKILL 时来不及停引擎，引擎成为孤儿，继续占着端口和内存，所以拉起成功后写所有权记录 `<IDORIS_STATE_DIR>/engines-owned/<engine_kind>/<endpoint 编码>.json`（文件 0600、目录 0700，原子写法同 §2.7.3；按引擎种类分目录，同一种类的多个实例各一份）：

```json
{"schema":1,"engine_kind":"omlx","pid":51234,"port":19731,"started_at":"Fri Nov 20 15:14:05 2026","executable":"/Applications/oMLX.app/Contents/MacOS/omlx-cli","api_key":"<引擎私有 key>","launched_by_instance":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11"}
```

`api_key` 只有支持私有 key 的引擎种类才有（§2.7.7）。`started_at` 是 `ps -o lstart= -p <pid>` 的原文（用 `ps` 子进程取是实现细节，契约只要求下面的四项身份校验）。新 iDoris 启动时：pid、启动时间、命令、监听端口四项全部吻合才算同一个进程，就接管，沿用记录里的 key，不重启引擎，已加载的模型继续在内存里；读不出来不算不吻合（不发信号，记录保留）；记录存在但进程不吻合（pid 被复用或已死）则丢弃记录。吻合但存在 `load.pending` 围栏时，引擎可能正处于加载中：**对自己拥有的引擎，杀掉整个进程组、确认已消失、清围栏、重新拉起**。这是托管比外部引擎多出来的一项能力，也是 Agent24 的「`kill -9` iDoris 后自动恢复」在加载进行中被杀时能过的前提；外部引擎的围栏保持原样，仍需人工处理，因为没有证据证明加载已经停止。`engine.stop_on_exit` 为 `"never"` 时，正常退出也保留引擎和记录，由下一个 iDoris 接管（适合 iDoris 升级重启时不想重新加载大模型）；默认 `"owned"`，退出时停掉自己拉起的引擎，避免孤儿占内存（`"never"` 与退出码 4、5 的关系见 §2.7.5）。

**状态机、退避与就绪探测。** 状态即 `/health.upstreams[].state`（§2.11.1）：`starting` 到 `ready`；`ready` 探测失败或进程退出到 `restarting`（退避 1、2、4 秒起翻倍，封顶 30 秒，±20% 抖动；只适用于自有引擎，外部引擎探测失败只报 `unavailable`，iDoris 不重启它）；5 分钟内失败超过 5 次到 `failed`（`reason:"crash_loop"`），之后每 60 秒半开重试一次，不永久放弃（与 Agent24 方案 §3.1 同一套参数，两层口径一致）；Admin 显式停止到 `stopped`；找不到引擎到 `absent`。就绪探测由配方定义（oMLX 配方的判据见上）。

**与 Supervisor、围栏、`/health` 的关系。** 下面以 oMLX 引擎为例：它的模型级生命周期由 Supervisor 管理；没有模型级生命周期的 resident 型引擎（如伴生卡、自定义上游）不经 Supervisor，也就没有这个问题。Supervisor 在创建时只对引擎采样一次，失败就把启动错误粘滞住，之后 list、status、chat、load、unload 一直返回该错误，必须重启 iDoris（v0.5.0 的行为，所以现在要求「先起 oMLX 再起 iDoris」）。因此托管时必须「引擎就绪之后才创建 Supervisor」，而引擎可能要 90 秒才好，期间 iDoris 要能答 `/health`。做法是可替换的服务体：数据面与 Admin 两个监听器照旧最先绑定，各自服务一个可原子替换的 Router。（1）启动期服务「starting 体」：`/health` 返回 `phase:"starting"`，Admin `status` 可用，数据面其余路径 503 `idoris_starting` 并带 `Retry-After`；（2）引擎管理器按上面的启动决策把引擎带到就绪；（3）用真实的服务状态构建完整 Router 并替换，此时 Supervisor 面对的是一个已就绪的引擎，不会再锁住启动错误。没有托管卡时跳过（1）到（3）。托管引擎终局失败（`absent`、`failed`、`unavailable`）时**门控不被它拖住**：照常替换，降级放行（勘误六（8）至（10）），`/health` 用 `phase:"degraded"` 与 `upstreams[].reason` 说明原因；之后它冻结，要重启 iDoris。**门控未成功期间，同一进程里其他卡的可用性（S5 回填）**：数据面除 `/health` 外对**所有**卡一律 503 `idoris_starting`，包括非托管的 resident 卡：完整的 Router 是一次整体替换，没有「先放行一部分卡」的中间状态；Admin 同理，`GET /admin/api/v1/status` 之外的 `/admin/api/v1/*` 在通过认证之后也是 503 `idoris_starting`（没有 token 仍是 401）。门控在**所有**托管上游都「落定」时放行（落定 = `ready`，或不会自己好的终局；`starting`、`restarting` 不算）：Supervisor 表是整体创建的，所以要等每个托管引擎都有了结论；到了启动超时（管理器启动之后一个 `startup_timeout`）还没落定的算落定，放弃它（`failed` + `startup_timeout`）。放行时在同一把锁里再确认一次所有托管上游仍然落定，不是就回去再等。没就绪的托管上游不建 Supervisor，路由到它的请求直接 503 带它的 `reason`。非托管的 resident 卡的探测器在门控完成之后才启动（它们的状态在那之前是 `starting`）。

**v0.6.0 与 v0.6.1 的边界。** v0.6.0 只做启动门控加拉起、停止、重启、孤儿接管（设计稿 3a）。运行期引擎换代（崩溃重启、外部引擎被用户重启）之后，Supervisor 的账本已过期，它以为 `Ready` 的模型其实已不在引擎里；热恢复（重建加替换，含围栏代际，设计稿 3b）后移 v0.6.1（**Admin 发起的** `restart`、`stop` 之后的 `start` 不属于这里：引擎进程是我们自己换的，Supervisor 原地复位，不需要重建，见勘误十（2））。v0.6.0 的兜底（设计稿 D4 的 B）：iDoris 自己拥有的引擎失效且无法在进程内恢复时，iDoris 优雅停止并以退出码 5 退出，由 agent24d 重启，重启后走全新的启动门控。外部引擎被用户重启这件事 v0.5.0 就存在，本修订不引入新问题，只在这里写明。退出码 5 的精确触发条件（S5 回填，launcher 不需要区分）：**门控完成之后，自有引擎的进程退出即退**（不经 `restarting` 退避：进程内恢复本来就做不到，等只会拖长不可用）；进程活着而探测失败（例如 oMLX 加载大模型时 `/v1/models` 变慢）只把状态报成 `unavailable`，**不杀引擎、不退出**，探测恢复后回到 `ready`；门控完成之前的失败（引擎提早退出、等就绪超时、端口被占）走 `restarting` 与熔断（1、2、4 秒起翻倍，封顶 30 秒，±20%；5 分钟内失败超过 5 次到 `failed` + `crash_loop`，之后每 60 秒半开重试一次），不是退出码 5；外部引擎的失效从不触发退出码 5（只报 `unavailable`，iDoris 不重启它）。进程退出在每秒一次的存活检查里被发现，不等 5 秒的探测周期。配置性的失败（`budget_zero`、`base_unavailable`、模型目录取不到、围栏清不掉、`engine_not_found`）不会自己好，不重试，等 `start`（Admin，S6）。

**停止、重启与启动**（经 Admin，§2.11.4）。`stop`：向自己拥有的引擎进程组发 SIGTERM，8 秒宽限后 SIGKILL，状态 `stopped`；对 `owned=false` 的上游一律拒绝（409 `upstream_not_owned`），iDoris 不去停别人的进程。`restart`：`stop` 之后重新走启动决策，对 `owned=false` 的上游同样拒绝。`start`：从 `stopped`、`failed`、`absent` 或 `unavailable` 重新走启动决策，已经 `ready` 时幂等。**门控放行之后**（S6，勘误十）：引擎换了一个进程，Supervisor 的账本在发布 `ready` 之前复位，下一次请求重新加载模型；放行时没就绪而冻结的上游 `start`、`restart` 是 409 `restart_required`；`stop` 在进程确认消失之后才撤销私有 key，并清空账本；对还活着的自有引擎 `start` 走完整的停止路径，不丢弃句柄。

#### 2.7.7 引擎私有 key（已冻结；oMLX 配方，设计稿 D10 待实测，结论回填）

- 做法（oMLX 配方；其他引擎种类各自声明是否支持私有 key 以及怎么传，不支持的种类没有这种隔离，不影响其他设计）：iDoris 自己拥有的引擎，每次拉起时由 CSPRNG 生成一个随机 key，经环境变量 `OMLX_API_KEY` 交给引擎子进程（不用 `--api-key`，argv 同 UID 可见），并记入 0600 的所有权记录（§2.7.6）。别的进程不带这个 key 访问该引擎得到 401，「所有模型调用只经 iDoris」才成为技术约束，而不只是约定。iDoris 内部 oMLX key 的解析仍是单一入口：私有引擎的 key 优先，其次才是 §2.9「oMLX 密钥」的顺序；外部（adopt）引擎不适用，仍按 §2.9 的顺序。该 key 只在 iDoris 与该引擎之间，不是 virtual key，也不会交给 Agent24。**实现（S5）**：key 解析的单一入口（`omlx::key`）按目标 URL 查进程内的「被引擎管理器接管的端点」表：自有引擎的端点只用私有 key（不看环境变量与 `settings.json`），引擎停止、失效或换代时撤销登记，旧 key 不会留在进程里；外部（接管）引擎的端点上，环境变量的 key 照旧，`settings.json` 的 key 只在端口属主校验通过之后才给（#619）；没有登记的端点（非托管的卡）按 §2.9 的顺序，与 v0.5.0 一致。向自有引擎发私有 key 之前，先确认监听该端口的是这个引擎进程（拉起之后、封印所有权记录之前也一样）：端口被别的东西占着时，key 与探测都不会发给它。**边界（设计稿 D1，与 §2.1 对 `instance_nonce` 的说法相同）：**私有 key 同时存在于引擎子进程的环境变量、所有权记录的 `api_key` 字段和 iDoris 进程内存三处，同 UID 的进程能读 0600 文件，macOS 上同 UID 也能用 `ps -E` 之类读到进程环境。所以它只挡读不到该目录的沙箱进程和不知情的其他客户端，**不挡同 UID 的恶意进程，不能当作对抗同 UID 的隔离**；验收项写作「不知情的其他客户端被拒」，不写「任何进程都无法直连引擎」。
- **待实测，结论待回填**：环境变量 `OMLX_API_KEY` 与 iDoris 自有 base 下 `settings.json` 里的 `auth.api_key` 同时存在时 oMLX 认哪个（用户 base 里的设置私有引擎根本不读，§2.7.6；iDoris 自己不往自有 base 写 `auth.api_key`，所以通常两者不会同时存在），尚未验证（设计稿起草时没有起引擎做实测）。验证办法：在一个空闲端口临时起 `omlx serve`，设环境变量，另用一份 settings.json，分别带两个 key 请求 `/v1/models`，看谁被接受，不加载任何模型。通过则启用上述做法；`settings.json` 优先则私有 key 不生效，退回现有 key 顺序，失去隔离，其余设计不变，并把本节改成「不隔离」。回填格式：结论（环境变量优先或 `settings.json` 优先）、oMLX 版本、日期。
- 静态结论（修订 1.6 勘误，仍待实测）：静态阅读 oMLX 0.6.4，`settings.py` 的载入顺序是文件、环境变量、命令行，所以环境变量 `OMLX_API_KEY` 优先于文件里的 `auth.api_key`；但 `skip_api_key_verification` 与 sub keys 只来自文件，环境变量盖不掉，所以私有 key 的隔离依赖自有 base（§2.7.6）：自有 base 由 iDoris 创建，这两项为默认值。这只是静态结论，没有运行过真实的 oMLX。
- 结论：**待实测**。

#### 2.7.8 `setup omlx` 的扩展（已冻结）

解除 Agent24 connect 流程对「oMLX 必须在线」的依赖，并让 Agent24 里不再出现任何 oMLX 地址（R12-Q7）。上表「组件配置」行的命令形状保持有效。`setup omlx` 是 oMLX 专用的 setup，名字与既有的 11 个错误码不变；其他引擎种类的 setup 属预留（§2.12.1）：每种需要托管的引擎各有一个 `setup <engine_kind>`，不托管的 OpenAI 兼容引擎由 `setup upstream`（§2.3，最小版 v0.6.0）统一生成，不做通用的 `setup engine --kind`。新增：

```
idoris setup omlx --out-dir <dir> [--endpoint <url> | --auto] [--managed] [--managed-port <n>]
                  [--bind role=model]… [--load-mode on_demand|resident] [--api-key-stdin] [--force] [--no-probe]
```

- `--auto`：只读探测候选端点 `127.0.0.1` 的 8088、8000、8001（Agent24 现在在自己代码里探测的同一组，迁入 iDoris 之后 Agent24 才能真正删掉所有 oMLX 地址；多个同时可达时按这个顺序取第一个形状是 oMLX 的）。**不做候选列表接口**（Agent24 2026-11-20 已确认，R12 已确认项 2）：桌面端只显示结果（输出 JSON 的 `mode`、`endpoint`、`engine.installed`），本节不再补字段。找到则等同 `--endpoint <找到的>`（adopt，卡不带 `idoris_managed`）；没找到则必须同时带 `--managed` 才继续，否则报 `unreachable`（保持现状）。**`--auto` 可以带 `--api-key-stdin`**（S5 落地 [#619](https://github.com/iDoris-ai/iDoris/issues/619) 之后放宽，取代此前「互斥」的口径）：候选端口是猜的，所以探测先发匿名请求；候选对匿名请求回 401 或 403 而调用方给了 key 时，先确认现在监听这个端口的进程是同一个用户的、签名团队是 oMLX 的（`PSK5Q5T46L`，`lsof` 找监听者，`codesign` 读进程正在运行的代码），通过了才用**新开的连接**带 key 再探一次；没通过（别的用户、没有签名、别的团队、读不出身份、看不到监听者）就**不发 key**，当作「要 key 而没法给」，报 `auth_required`，错误 JSON 带 `endpoint`。`--endpoint <url> --api-key-stdin` 是调用方明确指定的端点，不做属主校验，行为不变。`--auto` 带 key 而候选属主验过、引擎却拒绝这个 key 时报 `auth_failed`（带 `endpoint`）。候选对匿名请求回 401 或 403（要 key），而没有给 key（或给了没通过校验），也没有别的候选合格时，报 `auth_required`，错误 JSON 带 `endpoint`（该候选的地址），调用方据此加 `--api-key-stdin` 或改用 `--endpoint`；带 `--managed` 时同样报该错误，不转去托管，以免在一个可能是 oMLX 的服务旁边再起一个（2026-11-21，S4 验收定案）。
- `--managed`：卡带 `extensions.idoris_managed`，端点取 `--managed-port`；缺省时在 19000 到 19999 里挑一个此刻空闲的端口写进卡（私有端口，不占 oMLX 官方默认的 8000，避免用户之后启动 oMLX.app 时端口冲突）。`--managed` 与 `--auto` 联用且找到了外部 oMLX 时，本修订取保守的一边：adopt 外部引擎且不托管（2026-11-21，S4 验收已确认）。托管要定位模型目录（§2.7.6（3））：HOME 取不到、或该路径含逗号时报 `validation_failed`，排在找端口、找引擎之前。
- `--no-probe`（`--managed` 隐含）：不连引擎。`--bind role=model` 改为对照 `~/.omlx/models` 目录校验（`idoris models local` 已能离线读取）；托管引擎拉起时显式传的 `--model-dir` 就是这个目录（§2.7.6（3）），所以核对的与引擎实际看得到的一致，`--bind embed=bge-m3-mlx-fp16` 在引擎没起时也能写。
- 新增错误码，现有 11 个不变：`engine_not_installed`（`--managed` 时找不到引擎可执行文件）、`port_in_use`（`--managed-port` 指定的端口已被占用）、`model_not_installed`（离线绑定时 `~/.omlx/models` 里没有该模型）。
- 输出 JSON 增加 `mode`（`"adopt"` 或 `"managed"`）与 `engine`（`{engine_kind, installed, executable, version?}`，`engine_kind` 是引擎种类，`setup omlx` 恒为 `"omlx"`）；既有字段（含 `endpoint`）不变。伴生 resident 卡 `omlx-embed.yaml` 在给了 `--bind embed=<模型>` 时随 `omlx.yaml` 一并写入，并列入 `--force` 的整体替换范围（§2.3）；输出 JSON 新增 `companion_card_file`（`"omlx-embed.yaml"`），只在生成了伴生卡时出现。
- Agent24 侧承诺（R12-Q7）：删除 `omlx_probe`，候选与配置全部改为调用 `idoris setup omlx --auto`（不做候选列表接口，已确认）；之后 Agent24 源码不再出现任何 oMLX 地址。
- 不承诺 / 以后修订（Agent24 2026-11-20 已确认，R12 已确认项 3）：`setup omlx` 的可执行文件路径参数（如 `--omlx-executable`）。v0.6.0 不随包附带 oMLX.app，引擎可执行文件只按默认位置发现（§2.7.6，oMLX 配方）；以后要随包时另行修订，届时与帧字段 `engine.executables.omlx` 对称地加上。

**`setup upstream`（最小版，v0.6.0 承诺；Agent24 的 IC-5 用它生成 LM Studio 等卡）。**

```
idoris setup upstream --kind openai_compat --endpoint <loopback-url> --out-dir <dir>
                      --bind role=model [--id <provider-id>] [--default]
```

- **只 loopback**：`--endpoint` 的主机必须是回环地址（`127.0.0.0/8`、`::1` 或字面量 `localhost`），否则拒绝，不写卡。局域网与远程上游需要隐私等级、egress 计数与同意流程，不在 v0.6.0（§2.2 修订 1.6 增补）。**实现口径（回填）**：v0.6.0 只接受 `127.0.0.1`、`[::1]`、`localhost` 三种写法：卡契约声明 `locality: loopback` 的卡，endpoint 主机只能是这三种（`idoris-policy` 的回环主机白名单），`127.0.0.2` 这样的写法写出来的卡在启动时就加载不了，所以在 `setup` 里就按 `invalid_endpoint` 拒绝；`127.0.0.0/8` 里的其余地址随卡契约的白名单以后一并放宽。`--endpoint` 的协议是 http 或 https，不含路径（写 `http://127.0.0.1:1234`，不带 `/v1`）、凭据、查询与片段。
- **无 key**：不接受任何密钥参数，生成的卡不带凭据；需要密钥的上游等 vault 接线之后另行修订。
- **chat，可选 embedding**：`--bind` 接受 chat 类角色与 `embed`（勘误五），至少要有一个 chat 角色；给了 `embed` 绑定时仍是**一张**卡，能力为 `[chat, embedding]`（单数 `embedding`），`idoris_role_models.embed` 绑定该模型，resident；没给时能力只有 `chat`。`rerank`、`vision` 仍不接受。这种卡不生成伴生卡（上游卡本来就是 resident），也永远不带 oMLX 的 key（§2.3）。
- **探测**：写卡之前 `GET <endpoint>/v1/models`（不带凭据），要求 200 且 JSON 里有 `data` 数组；连不上或形状不符就报错，不写卡。
- 生成的卡是 resident 的 HTTP 卡，`extensions.idoris_upstream: {kind: openai_compat, flavor: openai_compat}`（§2.3），`engine_kind` 为 `openai_compat`；`--id` 缺省值、`--default` 的卡面效果（`extensions.idoris_default`，选择语义仍是预留，§2.12.3）和输出 JSON 的字段，以 v0.6.0 发版时的实现为准并回填本节，既有 `setup omlx` 的错误码与形状不受影响。
- **回填（取自 v0.6.0 的实现，2026-11-21；规范没写处取最保守的一边，上面各条不变）**：
  - **卡面**：`privacy_class: local_only`、`allowed_egress: [loopback]`、`fail_closed`、`fallback_policy: fail_closed`、免费（`cost` 为 0）、能力只有 `chat`、`load_policy` 为 `resident` 加 `keepalive.pinned: true` 加 `admission: coexist`；`version_pin` 为 `openai_compat@unknown`（产品名等于种类名，§2.3）；`extensions` 带 `idoris_upstream`、`idoris_role_models` 与 `_degradation`。除这些标注与 `version_pin` 外，与 Agent24 在 v0.5.0 上手写并实测的 LM Studio 卡逐字段相同，所以行为等价；差别只有 `engine_kind`：这张卡是 `openai_compat`，手写卡写 `lmstudio@0` 得到的是 `lmstudio`。
  - **`--out-dir`** 是已有的组件目录（不存在就创建），写 `<id>.yaml` 这一个文件，目录里别的文件原样不动；落盘前把「现有的卡加新卡」放进暂存目录，用服务启动时同一个加载器校验整个目录：别的文件里已有同一个 `provider.id`、现有的卡本身加载不了，都不写，`--out-dir` 不变。目录不必为空，这一点与不带 `--force` 的 `setup omlx` 不同；带 `--force` 的 `setup omlx` 同样保留别的卡（勘误七），所以两个命令谁先谁后都行，此前「必须先 `setup omlx`」的说法作废。
  - **`--id`**：卡的 `provider.id`，也是文件名 `<id>.yaml`。取值 `[a-z0-9][a-z0-9_-]{0,47}`，不得是 `omlx`、`omlx-embed`、`mock`、`subscription`、`idoris-cloud`（前两个是 `setup omlx` 的卡的 id，不能被占用：v0.5.0 按 `provider.id == "omlx"` 注入 oMLX 的 key；后三个在加载时有特殊含义：`mock` 被跳过，`subscription` 有固定的卡边界，`idoris-cloud` 是云端档合成卡的 id）。缺省 `<种类>-<端口>`，如 `openai_compat-1234`：同一个端点每次得到同一个 id。
  - **`--force`**：同名卡（`<id>.yaml`）已经在时才需要，替换的只有这一个文件，且它得是普通文件；别的文件里已经有同一个 `provider.id` 时，`--force` 也不能覆盖（`id_conflict`）。同一条命令加 `--force` 重新生成，文件逐字节相同。
  - **`--default`**：卡带 `extensions.idoris_default: true`（布尔真；不给就没有这个键）。至多一张卡带它：目录里已经有别的默认卡时拒绝（`validation_failed`，说明里点名那张卡），要换默认，先把旧的默认卡不带 `--default` 重新生成（`--force`）。选择语义仍是预留（§2.12.3）：v0.6.0 的路由不读这个键，带不带它，请求落到哪张卡都一样。
  - **`--bind`**：至少一个 chat 类角色 `fast`、`daily`、`deep`、`decide`、`auto`，另可加 `embed`（勘误五；只绑 `embed` 没有 chat 角色报 `invalid_role`）；`vision`、`rerank` 报 `invalid_role`；同一个角色只能绑一次（命令行是用法错误，库入口报 `invalid_role`）。所绑模型要在探测到的列表里，否则 `model_not_found`。
  - **输出 JSON**：`setup omlx` 的全部字段，另加 `id`、`card_file`（`<id>.yaml`）、`default`（这次写出的卡有没有带 `idoris_default`）。`mode` 恒为 `"adopt"`，`auth` 恒为 `"none"`，`engine` 是 `{engine_kind: "openai_compat", installed: false, executable: null}`（这种上游没有可执行文件的概念，是否可用看探测，不看 `installed`）。
  - **错误码**：沿用 `invalid_endpoint`、`invalid_role`、`unreachable`、`auth_required`（上游对不带凭据的探测回 401 / 403）、`model_not_found`、`out_dir_not_empty`（`--out-dir` 不是目录）、`validation_failed`、`write_failed`；新增 `not_openai_compatible`（`GET /v1/models` 的响应不是 200 加带 `data` 数组的 JSON，每一项要有字符串 `id`，响应体上限 2 MiB，不跟随重定向）、`id_conflict`（同名卡已在而没有 `--force`，或别的文件里已有同一个 `provider.id`）、`invalid_id`。用法错误（缺必选旗标、`--kind` 不是 `openai_compat`、没有 `--bind`、出现本命令没有的旗标如 `--api-key-stdin`）与 `setup omlx` 一样，退出码 1、stderr 给用法说明、stdout 为空。

#### 2.7.9 布局与不承诺（已冻结）

- 布局：随包仍恰好 5 个文件（上表「布局」行不变），Agent24 的锁文件里逐个列出的 5 个文件不变；伴生卡 `omlx-embed.yaml` 等都由 `setup` 生成在 `--out-dir`，不是随包文件。
- 不承诺（在上表「不承诺」行之外增加）：iDoris 下载或安装任何引擎（v0.6.0 的 oMLX 由用户自装；附带、按需下载以后再议）；`setup omlx` 的可执行文件路径参数（以后修订，§2.7.8）；多引擎的实现（预留，§2.12）；运行期引擎换代后的热恢复（v0.6.1，v0.6.0 以退出码 5 兜底）；`setup upstream` 最小版之外的形状（密钥、局域网与远程、embedding 等非 chat 角色，后移）；Admin 的模型 load/unload（#611）、§2.9 的下载（后移 v0.6.1）；动态端口下 `idoris admin <资源> --token-stdin` 自动从发现文件取端口；embeddings 并入 Supervisor 的内存账本（§2.11.3，设计稿 E2，以后再做）。

### 2.8 wire 契约 1.1.0（iDoris v0.4.0）

只有新增，1.0.x 的调用方不受影响。版本兼容规则：minor 版本只增加内容，不改变已有语义；破坏性变更会升 major。

- `GET /capabilities` 的条目可能带三个新字段：`degraded_alternative`（只出现在 `requires_eviction` 条目上，`{id, capability, estimated_memory_gb, quant, reason}` 或 `null`）、`degraded_alternatives`（只出现在 `blocked` 条目上，是数组）、`headroom_breach_option`（`{id, quant, estimated_memory_gb, ctx_limit}`，整份快照最多一个条目带这个字段，条目 id 唯一）。`reason` 是稳定的代码，例如 `fits_without_eviction`。解析时看哪个条目带了 `headroom_breach_option`，不要靠 `resident:true` 来定位。
- 用户选「清理后用最优模型」或「降级」时，直接请求对应的模型 id 即可，不需要新接口。
- **1GB 余量**：设置了 `IDORIS_ALLOCATED_BUDGET_BYTES` 时，生命周期加载默认最多用到额度减 1GB。`POST /v1/chat/completions` 带上 `X-iDoris-Headroom-Breach: confirmed`（只接受这一个值），表示用户已确认，这一次加载可以用满全部额度。确认只对单次请求有效，不会被记住。它是 Agent24 问过用户之后代为发出的约定，不是权限边界。
- 没带确认、而这次加载只有用掉余量才放得下时，返回 HTTP 503，`error.type` 与 `error.reason_code` 都是 `headroom_confirmation_required`，此时应询问用户。
- 生命周期繁忙时（包括另一个请求正在以不同的确认状态加载同一个模型），返回 HTTP 503，`error.type` 是 `local_only_unavailable`，`error.reason_code` 是 `supervisor_busy`。必须按 `reason_code` 区分，有限退避后用同一个请求头重试即可，不要询问用户。
- 带了确认也放不下（超过整个额度）时，`error.type` 是 `local_only_unavailable`，`reason_code` 不是 `headroom_confirmation_required`，为终态，不应弹出确认。其它 503 按原规则处理。
- `remediation` 是给人看的说明文字，不属于契约，不得用于匹配。

**修订 1.6 增补（已冻结）：预热与余量确认。** `POST /v1/models/warmup`（§2.11.2）复用本节的约定：同一个请求头 `X-iDoris-Headroom-Breach: confirmed`，同一套 503 `reason_code`（`headroom_confirmation_required`、`supervisor_busy`，其余 `local_only_unavailable` 为终态）；确认仍只对单次请求有效，也仍是 Agent24 问过用户之后代为发出的约定，不是权限边界。预热是用户在设置里选大模型时最自然的触发点，所以 Agent24 已有的确认交互（ID-HR，R10）原样适用，仍按 `type` 与 `reason_code` 判定，不匹配文字。预热属于应用自己的流程，走数据面加数据 key；`unload`、固定（pin）、带策略的显式 load 属于运维操作，归 Admin（§2.11.4），两者不重叠。

### 2.9 本机 chat 模型推荐、本地模型类别与下载（wire 1.2.0，2026-11-15 双方确认；iDoris 侧推荐、列表与密钥顺序已在 v0.5.0 实现；下载待实现）

只有新增，1.1.x 的调用方不受影响。本节落地前，§2.3「下载/预热的功能归属在 iDoris，但归属不表示已有下载服务」仍然成立。

**分工**：iDoris 负责「这台机器的 chat 用哪个模型」——档位表、量化选择、是否已安装、内存是否放得下、下载与加载。Agent24 负责首次启动时调用、把用户的选择保存为默认 chat 模型、Settings 的模型选择器，以及 iDoris 未安装时回落到 `Qwen3-8B-4bit`。

**档位表（owner 2026-11-15 拍板，按物理内存）**

档位按 `host.ram_bytes`（物理内存字节数）落在哪个左闭右开区间判断，单位是 GiB（1 GiB = 1073741824 字节；Apple 标称的「16GB」就是 16 GiB）。模型体积与内存估算（`size_bytes`、`est_memory_bytes`）用十进制字节。每台机器恰好落在一行：

| `ram_bytes` 区间 | 档位推荐（oMLX 目录名） | `alternatives` | 档位 `reason` |
|---|---|---|---|
| [0, 16 GiB) | `Qwen3-8B-4bit`，**仅当它放得下**；放不下则 `model_id: null` | 无 | `tier_below_16gib` |
| [16 GiB, 48 GiB) | `Qwen3-8B-4bit` | 无 | `tier_16_48gib` |
| [48 GiB, 64 GiB) | `Qwen3.8-27B-OptiQ-4bit` | `Qwen3-8B-4bit` | `tier_48_64gib` |
| [64 GiB, ∞) | `Qwen3.8-27B-OptiQ-4bit` | `Qwen3.6-35B-A3B-OptiQ-4bit`、`Qwen3-8B-4bit` | `tier_64gib_plus` |

（owner 原话「≤8GB 不推荐本地 chat；16/24/32GB → 8B；48GB+ → 27B；64GB+ 可选 35B-A3B」。8 GiB 机器在第一行按放不下返回 `null`；18 / 36 GiB 等落在第二行。）

档位按真实物理 RAM 判断（与 §2.4 的 min_ram 门一致）。「放得下」（`fits`）另按 iDoris 的内存公式判断：设置了 `IDORIS_ALLOCATED_BUDGET_BYTES` 时按该额度，否则按 iDoris 自探的可用 GPU 内存。

**回落规则**：档位推荐的模型放不下时，按 `Qwen3.6-35B-A3B-OptiQ-4bit` → `Qwen3.8-27B-OptiQ-4bit` → `Qwen3-8B-4bit` 的顺序，从档位推荐的下一个起找第一个放得下的作为 `model_id`，此时 `reason` 为 `fallback_does_not_fit`；一个都放不下时 `model_id: null`，`reason` 为 `insufficient_memory`。`alternatives` 只列档位表里该行的备选，每项各自带 `fits`。

内存依据（实测，2026-11-15）：`Qwen3.8-27B-OptiQ-4bit` 权重 20.68 GB，加 8k 上下文 KV（全注意力上界约 2.1 GB）与约 1 GB 开销，约 23.8 GB；24 GiB 机器可用约 15.8–16.8 GB，32 GiB 约 21.1–22.4 GB，均放不下。

**`GET /v1/recommendation?role=chat`**（只读，需要数据 key，与 `/v1/models` 同级鉴权）

- 查询参数：`role`，本版只支持 `chat`，其他值返回 400，`error.reason_code` 为 `unsupported_role`。
- 响应 200：
  ```json
  {
    "role": "chat",
    "model_id": "Qwen3-8B-4bit",
    "installed": true,
    "size_bytes": 4620000000,
    "est_memory_bytes": 6100000000,
    "fits": true,
    "reason": "tier_16_48gib",
    "host": {"ram_bytes": 17179869184, "allocated_budget_bytes": null},
    "alternatives": [
      {"model_id": "…", "installed": false, "size_bytes": null, "est_memory_bytes": null, "fits": true, "reason": "…"}
    ]
  }
  ```
- `model_id` 是 oMLX 目录名（即 `/v1/chat/completions` 的 `model` 应填的值），可为 `null`。`installed` 表示 `~/.omlx/models/<model_id>` 已存在且完整。`size_bytes` 为磁盘/下载体积（未安装时为预计下载体积），`est_memory_bytes` 为加载后的内存估算；两者未知时都为 `null`（不用 0），`alternatives` 里的同名字段相同。`reason` 只取上面档位表与回落规则里的代码（`tier_below_16gib`、`tier_16_48gib`、`tier_48_64gib`、`tier_64gib_plus`、`fallback_does_not_fit`、`insufficient_memory`），供程序判断；给人看的说明另放 `remediation`，不属于契约。
- 本接口不加载、不下载、不改任何状态。Agent24 保存用户的选择，iDoris 不记忆。

**`idoris recommend --role chat --json`**（CLI，同一响应形状）

- 只需要 iDoris 可执行文件：不开数据库、不取实例锁、不启动 runtime、不需要 serve 在跑，也不需要完整 setup。自探硬件，读取 `~/.omlx/models` 判断是否已安装；不连接 oMLX，因此不读取任何 oMLX 密钥（下面「oMLX 密钥」一节只适用于 iDoris 连接 oMLX 的场景）；可用 `--allocated-budget-bytes <u64>` 覆盖额度。
- 退出码：0 成功（含 `model_id: null`）；1 失败（stderr 一行说明）。

**`GET /v1/models/local`**（只读，需要数据 key）

- 列出 `~/.omlx/models` 下的模型：`{"models": [{"omlx_id": "Qwen3-8B-4bit", "kind": "chat", "size_bytes": 4620000000, "catalog_id": "qwen3-8b"}]}`。
- `kind` 取值：`chat`、`vision`、`embedding`、`rerank`、`image`、`ocr`、`tts`、`unknown`。来源优先级：iDoris catalog（`config/catalog.yaml`，新增 `omlx_id` 与 `kind`）→ 按模型目录的 `config.json`（`architectures` / `model_type` 等）推断 → 推断不了为 `unknown`。`catalog_id` 不在 catalog 时为 `null`。
- Agent24 的 chat 选择器只列 `kind` 为 `chat` 或 `vision` 的条目；`unknown` 不列入。
- 同形状 CLI：`idoris models local --json`，运行条件与 `idoris recommend` 相同（只读文件系统，不连接 oMLX、不读密钥）。HTTP 版同样只读 `~/.omlx/models`，不为列表去调用 oMLX。

**下载（归 iDoris）**

- iDoris 负责下载与校验，拒绝下载内存放不下的模型（`fits: false`）。大文件下载必须由用户在 Agent24 界面确认后才开始，iDoris 不自动下载。
- 接口形状（下载进度、取消、错误码）在 iDoris 实现时另行修订本节，双方确认后再冻结；本版只冻结归属与「必须用户确认」这一条。Agent24 负责向用户说明体积、征得同意、显示进度。

**oMLX 密钥**

- 优先级：`IDORIS_OMLX_API_KEY`（§2.7，保持不变）→ `~/.omlx/settings.json` 的 `auth.api_key` → 无鉴权连接。
- `settings.json` 的 key 只发给 loopback 的 oMLX（base URL 非 loopback 时视为没有）；文件必须是常规文件（符号链接不算），key 取 trim 后的值，含内部空白或控制字符视为没有。环境变量的 key 维持原行为。
- 只从文件读取，不打印、不写日志、不进 argv 或响应体。不硬编码任何默认值（Agent24 侧同样不再假定默认 key）。
- oMLX 开了鉴权而两处都没有 key 时，iDoris 按 401 处理并在 `remediation` 说明，不重试。

**Agent24 的调用顺序**：iDoris serve 在跑且有数据 key → 走上面的 HTTP 接口；否则本机有 iDoris 可执行文件 → 走 CLI（`idoris recommend` / `idoris models local --json`）；都没有 → 回落 `Qwen3-8B-4bit`，选择器按 oMLX `/v1/models` 列出（不按 `kind` 过滤；Agent24 内部按名称启发式排除图像 / OCR / 语音类，属 Agent24 内部行为，不进本契约）。

**Agent24 侧的 oMLX 密钥**：与 iDoris 同一顺序的后两步——`~/.omlx/settings.json` 的 `auth.api_key` → 无鉴权；不再假定任何默认 key。

**不承诺**：`chat` 以外角色的推荐（embedding / vision / image 以后另行修订）；自动升级已安装的模型；Linux / x86。

**修订 1.6 增补（已冻结）**：Agent24 在 R12-Q7 中承诺删除 `omlx_probe`，候选与配置全部改为调用 `idoris setup omlx --auto`（§2.7.8），之后不再直连 oMLX。迁移完成后，上文「Agent24 的调用顺序」末句里「都没有 → 回落 `Qwen3-8B-4bit`，选择器按 oMLX `/v1/models` 列出」的直连回落，以及「Agent24 侧的 oMLX 密钥」一段，都只对迁移前的 Agent24 版本有效；迁移后 iDoris 不可用时 Agent24 的降级行为由 Agent24 自己定义，不属于本契约。多引擎时推荐按默认引擎可用的模型来给（预留，§2.12.5），v0.6.0 只有 oMLX，行为不变。下载部分的归属与状态不变。

### 2.10 登记协议 v1（修订 1.6，已冻结）

本节所有条款（含全部子节）已冻结。

iDoris 是客户端，在 launcher 给的 Unix domain socket 上向 agent24d 登记，之后定期发心跳。它让 agent24d 知道「这个端口上跑的是哪个实例、是否健康」，也让 iDoris 知道有没有 launcher 还在。帧风格与 Agent24 已有的 `enroll.sock` 一致（一行一帧、单帧 ≤ 16 KiB、服务端先发 challenge、逐帧复核 `LOCAL_PEERTOKEN`），agent24d 可以复用同一套帧读取与复核代码。字节级测试向量见 §2.10.5。

**活性只看 `/health`，心跳只用于发现连接断开并重新握手。** 登记连接静默、心跳停止，都不是判卡死的依据：被 `kill -STOP` 的进程与「agent24d 自己卡住」无法区分。卡死判定与重启只由 agent24d 对 `/health` 的探测决定（探测间隔与超时是 Agent24 内部参数，不在本契约内冻结）。登记失败（连不上、身份或证明被拒、版本不兼容等）永远不使 iDoris 退出；仅有的两个例外是：launcher 显式给了非 0 的 `orphan_timeout_s`（§2.10.8），以及 agent24d 回 `instance_conflict`（§2.10.4）。

#### 2.10.1 传输与帧

- 传输：AF_UNIX、SOCK_STREAM，路径来自 launcher 帧的 `register_socket`（§2.7.1）。同一时刻 iDoris 只持有一个连接。
- 帧：一行一个 UTF-8 JSON 对象，以 LF 结尾，**每帧（含 LF）≤ 16384 字节**，不含 CR 和 BOM。超长、非 UTF-8、非对象、空行一律按 `bad_frame` 处理并关闭连接。接收方忽略未知字段（前向兼容），必填字段严格校验，`v` 必须等于 1。本协议最大的帧也只有约 350 字节。
- 选 NDJSON 而不是长度前缀：与 `enroll.sock` 一致，全是文本，用 `socat` 可以直接调试，没有二进制载荷。
- 两条与 `LOCAL_PEERTOKEN` 语义有关的义务：iDoris 在收到 `challenge` 之前不写任何字节（提前到达的字节可能被 agent24d 按 `identity_rejected` 拒绝并关闭连接）；UDS 的 fd 带 close-on-exec，不得被任何子进程（含引擎进程）继承。

#### 2.10.2 时序与计时

agent24d 先说话，iDoris 收到 `challenge` 之前不写任何字节：

```
iDoris                                   agent24d
  | -------- connect(register_socket) ----> |   (2 秒超时)
  | <-------- challenge ------------------- |   (5 秒内必须到)
  | --------- register -------------------> |   (收到 challenge 后 1 秒内发出)
  | <-------- ack ------------------------- |   (5 秒内)
  | --------- heartbeat seq=1 -------------> |   (ack 后立即发，之后每 heartbeat_ms)
  | <-------- heartbeat_ack seq=1 ---------- |   (5 秒内)
  | ...                                      |
  | --------- bye (优雅停止时) -------------> |
```

| 项 | 值 |
|---|---|
| 连接超时 | 2 秒 |
| 等 `challenge` | 5 秒 |
| 收到 `challenge` 后发出 `register` | 1 秒内 |
| 等 `ack` | 5 秒 |
| 心跳间隔 | `ack.heartbeat_ms`，缺省 10 秒，±10% 抖动 |
| 等 `heartbeat_ack` | 5 秒 |

#### 2.10.3 消息与字段

| `op` | 方向 | 何时 | 必填字段（除 `v`、`op`） |
|---|---|---|---|
| `challenge` | agent24d → iDoris | 连接建立后，agent24d 先发 | `conn_id`、`nonce` |
| `register` | iDoris → agent24d | 收到 `challenge` 后 1 秒内 | `nonce`、`instance_id`、`pid`、`version`、`contract_version`、`data_port`、`admin_port`、`started_at_ms`、`phase`、`resume`、`proof` |
| `ack` | agent24d → iDoris | 校验通过后 5 秒内 | `session_id`、`proof`；`heartbeat_ms` 可选 |
| `heartbeat` | iDoris → agent24d | `ack` 之后立即发，之后每 `heartbeat_ms` | `seq`、`phase`、`ready` |
| `heartbeat_ack` | agent24d → iDoris | 收到 `heartbeat` 后 5 秒内 | `seq` |
| `set_admin_token` | agent24d → iDoris | 仅在 `ack.proof` 已通过的会话上（§2.10.6） | `token` |
| `admin_token_ack` | iDoris → agent24d | iDoris 接受并替换 token 之后 | 无 |
| `bye` | iDoris → agent24d | 优雅停止时，尽力发送，至多 0.5 秒（§2.10.8） | `reason` |
| `error` | 双向 | 之后关闭连接 | `code`、`message` |

`challenge`：`conn_id` 为 16 个小写 hex，`nonce` 为 32 个小写 hex（128 位，同 `enroll.sock` 的 challenge）。

```
{"v":1,"op":"challenge","conn_id":"0123456789abcdef","nonce":"00112233445566778899aabbccddeeff"}
```

`register`：

```
{"v":1,"op":"register","nonce":"00112233445566778899aabbccddeeff","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","pid":41234,"version":"0.6.0","contract_version":"1.3.0","data_port":51234,"admin_port":51235,"started_at_ms":1763625330123,"phase":"ready","resume":false,"proof":"386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884"}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `nonce` | string | 原样回显 `challenge` 的 `nonce`；缺失或不符 = `identity_rejected` |
| `instance_id`、`pid`、`version`、`contract_version`、`data_port`、`admin_port`、`started_at_ms` | 同发现文件 | 服务端应与发现文件交叉核对；`pid` 只供日志与交叉核对，**不作身份依据** |
| `phase` | string | `starting`、`ready` 或 `degraded`，同 `/health.phase` |
| `resume` | boolean | 本进程此前已登记过（这是重连）则为 true，仅供日志与统计 |
| `proof` | string | 64 位小写 hex，见 §2.10.5 |

`ack`：

```
{"v":1,"op":"ack","session_id":"a1b2c3d4e5f60718293a4b5c6d7e8f90","heartbeat_ms":10000,"proof":"099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967"}
```

`session_id` 为 32 个小写 hex，是会话标识，不是凭据。`heartbeat_ms` 是心跳间隔，缺省 10000，iDoris 夹到 2000 到 60000。`proof` 是**服务端对 iDoris 的证明**（双向握手，§2.10.5）：读得到发现文件才算得出来。iDoris 校验失败就关闭连接，不发心跳，按退避重连（§2.10.7），仍继续服务。

`heartbeat` 与 `heartbeat_ack`：`seq` 是 u64，从 1 起每次加 1，`heartbeat_ack` 必须回显同一个 `seq`；`phase`、`ready` 同 `/health`，只是信息，不是活性依据。

```
{"v":1,"op":"heartbeat","seq":1,"phase":"ready","ready":true}
{"v":1,"op":"heartbeat_ack","seq":1}
```

`bye` 与 `error`（`message` ≤ 200 字符，不含秘密）：

```
{"v":1,"op":"bye","reason":"shutdown"}
{"v":1,"op":"error","code":"identity_rejected","message":"…"}
```

#### 2.10.4 版本协商与错误码

- **版本协商**：`v` 必须是 1；服务端不支持就回 `unsupported_version` 并关闭。以后升 `v` 属于破坏性变更，必须走本规范的修订。`contract_version` 是 HTTP wire 版本，服务端按它的锁（major 为 1，加 Agent24 要求的最低 minor）判断兼容，不兼容回 `contract_incompatible`。
- **会话规则**：iDoris 只处理 `challenge`、`ack`、`heartbeat_ack`、`error`、`set_admin_token` 五种服务端帧，其他 `op` 与未知字段一律忽略；服务端正常关闭连接（EOF）时 iDoris 立即重连（§2.10.7）。
- **错误码**（`error.code`）：

| `code` | 谁发 | 含义 | iDoris 收到后 |
|---|---|---|---|
| `identity_rejected` | agent24d | 登记方身份校验未过（`LOCAL_PEERTOKEN`、codesign，或 `register.nonce` 缺失、不符，或提前写了字节） | 关闭连接，60 秒后重连 |
| `proof_invalid` | 双向 | agent24d 发：`register.proof` 不符；iDoris 发：`ack.proof` 不符 | 收到时关闭连接，30 秒后重连；iDoris 自己发出后关闭连接，按默认退避重连 |
| `unsupported_version` | agent24d | 不支持 `v` | 关闭连接，60 秒后重连 |
| `contract_incompatible` | agent24d | `contract_version` 的 major 不是 1，或低于 Agent24 要求的最低 minor | 关闭连接，60 秒后重连 |
| `bad_frame` | 双向 | 帧不合规 | 关闭连接，按默认退避重连 |
| `timeout` | agent24d | 等待 iDoris 的帧超时 | 关闭连接，按默认退避重连 |
| `busy` | agent24d | 并发已满 | 关闭连接，5 秒后重连 |
| `instance_conflict` | agent24d | agent24d 已有一个 `ack` 过、心跳正常的会话（`instance_id`=A），并且 A 的 `/health` 仍以 `instance_id`=A 应答，这时 `instance_id`=B 来登记：两个活实例同时出现，拒绝后来者，不顶掉现有会话。A 已经失联（心跳超时，或 `/health` 不通、`instance_id` 不符）时，B 正常登记，A 视为陈旧（Agent24 2026-11-20 已确认，§2.10.9） | **不重连**：优雅停止，退出码 3（§2.7.5；处理与理由见下） |
| `internal` | 双向 | 内部错误 | 关闭连接，按默认退避重连 |

iDoris 自己发 `error` 只用 `bad_frame`（服务端帧不合规）、`proof_invalid`（`ack.proof` 校验失败）、`internal`，而且只在已经收到 `challenge` 之后才发，尽力发送、不等应答，之后关闭连接。等 `challenge`、`ack`、`heartbeat_ack` 超时时 iDoris 不发 `error`，直接关闭连接：收到 `challenge` 之前它不写任何字节。agent24d 收到任何 `error` 都应结束该会话。

**`instance_conflict` 之后 iDoris 的处理（iDoris 补，Agent24 已同意冻结）**：iDoris 不重连，也不继续服务，走正常的优雅停止路径（停止自己拥有的引擎、排空，发现文件仅当 `instance_id` 是自己的才删，§2.7.5），退出码 3，日志写 `idoris: 登记被 agent24d 以 instance_conflict 拒绝（已有另一个健康实例），已停止`（写 stderr）；收到之后不再自愈重写发现文件；没有已 `ack` 的会话，所以不发 `bye`。选「退出」而不是「继续服务并按退避重试」，理由有四：（1）能同时出现两个活实例，说明它们用的不是同一个数据库（同一个数据库已被实例锁挡在退出码 3），所以后来者的数据 key、预算账和审计与 agent24d 已签发的 key 对不上，就算先到的 A 之后失联，让 B 顶上也只会得到一片 401，后来者没有候补价值；（2）agent24d 不知道 B 的端口，也不会用它，继续服务只是白占内存和它托管的引擎，重试只是在 A 健康期间反复被拒；（3）B 若继续存活，两个实例共用同一个发现文件路径时，各自每 10 秒的自愈会互相覆盖，agent24d 可能读到 B 的 `instance_nonce` 而使 A 的重新握手失败，B 退出才会停止这种覆盖；（4）退出码复用 3：launcher 对 3 的既有处理（不重启）正是此时要的，Agent24 不需要新增退出码，也不必重开已确认的退出码表。A 之后失联时，agent24d 本来就会按监管策略重启或接管，B 提前退出不影响恢复。同一个 `instance_id` 的重连不是冲突，新会话替换旧会话，`resume` 为 true。

#### 2.10.5 `proof`：不让秘密上线（HMAC-SHA256）与测试向量

`register.proof = HMAC-SHA256(key, transcript)`。`key` 是 `instance_nonce` 解码后的 **32 个原始字节**，不是 64 个 hex 字符的文本。`transcript` 是下列各项按顺序拼接的 ASCII 字符串，项与项之间只有一个 LF（0x0A）、**没有空格**、末尾没有 LF，数字用十进制、无前导零：`idoris-register/v1`（字面量）、`conn_id`、`challenge_nonce`、`instance_id`、`pid`、`data_port`、`admin_port`、`started_at_ms`。

`ack.proof = HMAC-SHA256(key, transcript)`，同一个 `key`，`transcript` 的各项为：`idoris-ack/v1`（字面量）、`conn_id`、`challenge_nonce`、`session_id`，同样以单个 LF 分隔、末尾没有 LF。

- `conn_id` 与 `challenge_nonce` 每个连接都新，所以不能重放；端口与 pid 进了 transcript，中途不能篡改；服务端用常量时间比较。冒充的服务端只能看到公开信息（`instance_id`、端口、pid），拿不到 `instance_nonce`。
- 双向：`register.proof` 是 iDoris 对服务端的证明（我就是写了这份发现文件的实例），`ack.proof` 是服务端对 iDoris 的证明（我读得到发现文件）。
- 边界：`instance_nonce` 在 0600 文件里，挡不住同 UID 的恶意进程；它挡的是（1）连错实例、读到陈旧文件，（2）读不到该目录的沙箱进程（例如 Agent24 的模块沙箱）冒充。真正的身份闸门是 Agent24 对登记方的 `LOCAL_PEERTOKEN` 与 codesign 校验（§2.10.9），两者叠加。

**测试向量**（两端实现都必须得到同样的值；下面 transcript 里的 `\n` 表示 LF 一个字节，不是反斜杠加 n）。输入均为 ASCII：

```
instance_nonce  = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f   (hex 解码后 32 字节作为 HMAC key)
conn_id         = 0123456789abcdef
challenge nonce = 00112233445566778899aabbccddeeff
instance_id     = 3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11
pid=41234  data_port=51234  admin_port=51235  started_at_ms=1763625330123
session_id      = a1b2c3d4e5f60718293a4b5c6d7e8f90
```

`register` 的 transcript（137 字节，末尾无 LF）：

```
idoris-register/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\n3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11\n41234\n51234\n51235\n1763625330123
```

`proof` = `386409d010127b68de12aab15ca786c9abac147add07e228191e56dbb05d7884`

`ack` 的 transcript（96 字节，末尾无 LF）：

```
idoris-ack/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\na1b2c3d4e5f60718293a4b5c6d7e8f90
```

`proof` = `099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967`

可用 Python `hmac.new(bytes.fromhex(nonce), transcript, hashlib.sha256).hexdigest()` 复算；起草本节时已用这段代码核对上面两个值。

**`error` 帧不带认证（修订 1.6 勘误）**：`error` 帧没有 proof，不防伪造。socket 所在目录的完整性由 Agent24 负责（目录权限与属主，不让别的用户替换 socket）；同 UID 的进程不在威胁模型内（与 §2.1 对 `instance_nonce` 的边界一致）。

#### 2.10.6 会话内轮换 Admin token：`set_admin_token`

问题：接管孤儿 iDoris 的新 agent24d 没有旧的 Admin token。办法是 agent24d 生成新 token，经登记会话设给 iDoris，旧 token 立即失效，再由 agent24d 自己使用。

```
{"v":1,"op":"set_admin_token","token":"<64 位小写 hex>"}
{"v":1,"op":"admin_token_ack"}
```

- 只在 `ack.proof` 已通过的会话上接受；在 `ack` 之前或 `ack.proof` 未通过的连接上收到 `set_admin_token`，一律不处理并关闭连接。
- `token` 的校验同 launcher 的 token（64 位小写 hex）；每 10 秒至多接受一次。校验失败或超频：iDoris 不替换，token 保持原值，先尽力发 `error`（`code` 取 `bad_frame`）再关闭该会话。
- 替换是进程内存里的原子替换：之后任何带旧 token 的 Admin 请求得到 401，已经在处理的请求不受影响。
- 这是 §2.1 里 Admin token「只经两条一次性匿名 pipe/FD」之外唯一新增的传递通道。ADR-035 的约束不变：token 不进 argv、环境变量、文件、日志、`error.message`。
- Agent24 侧：接管后新 agent24d 生成新 token，用本帧设给 iDoris；token 之后只在 agent24d 内部使用，桌面端的 Admin 操作都经 agent24d 的 `/api/v1/idoris/*`，Electron main 不持有 token（R12-Q3）。

#### 2.10.7 失联判定、重连与退避

| 项 | 值 |
|---|---|
| iDoris 判失联 | 连续 3 次心跳未应答（每次 5 秒内没有应答算一次，约 25 秒，§2.10.10），或任何读写错误，或 EOF |
| 服务端判失联（建议） | 3 倍 `heartbeat_ms`（缺省 30 秒）没有收到任何帧 |
| 重连退避 | 第 1 次立即；之后 0.25、0.5、1、2、4、8 秒，再之后固定 10 秒；每次 ±20% 抖动；会话持续满 60 秒后清零 |
| 特殊原因的固定等待 | `identity_rejected`、`unsupported_version`、`contract_incompatible`：60 秒；`proof_invalid`：30 秒；`busy`：5 秒；`instance_conflict` 不重连，iDoris 退出（§2.10.4） |
| 日志 | 只记状态变化，以及首次失败和之后每第 10 次失败，不刷屏（Agent24 会把子进程输出管道读走，别写满） |

- 退避上限取 10 秒而不是常见的 30 秒：本机 UDS 连接几乎没有成本，不存在惊群，而 agent24d 被 launchd 重启之后希望尽快重新握手，延迟上限就是退避上限。
- **agent24d 拒绝或不在时，iDoris 继续服务，不因为登记失败而退出**（`instance_conflict` 与孤儿超时除外，§2.10.4、§2.10.8）：`/health.registered` 为 false，数据面与 Admin 不受影响，按上表退避重试；agent24d 被重启之后，iDoris 重连、重新握手，`resume` 为 true。

#### 2.10.8 孤儿策略与 `bye`

- **孤儿计时**：launcher 在帧里给了非 0 的 `orphan_timeout_s` 才启用（Agent24 取 900，R12-Q5）。从「没有已 `ack` 的登记会话」开始计时（包含进程启动后一直没登记上的情形，以及握手阶段，§2.10.10），任何一次成功的 `ack` 把计时清零。超过 `orphan_timeout_s` 就走优雅停机（§2.7.5），退出码 4，日志写 `idoris: 无启动器登记超过 N 秒，已停止`（写 stderr）。这条补上的缺口是：agent24d 被永久卸载或停掉之后，孤儿 iDoris 连同它托管的引擎和已加载的模型会一直占着内存。手工运行（帧里没给）不受影响。
- **`bye`**：优雅停止（由信号 SIGTERM、SIGINT 触发，或自有引擎失效以退出码 5 停止），并且当时存在已 `ack` 的登记会话时，iDoris 在该会话上尽力发 `bye`，至多 0.5 秒，不等应答，让 agent24d 区分「预期停止」与「崩溃」，其他退出路径不发（§2.10.10；孤儿超时与 `instance_conflict` 按定义没有已 `ack` 的会话），对应 Agent24 方案里卡死与崩溃分开记录的要求。`reason` 本版只定义 `shutdown`，具体退出原因以退出码为准（§2.7.5）。

#### 2.10.9 登记方校验与核对顺序（Agent24 承诺，R12-Q1，2026-11-20）

登记方不看 pid 数字。agent24d 的核对顺序如下，都是 Agent24 侧的承诺：

1. **登记方身份**：用 `LOCAL_PEERTOKEN`（每帧复核）取得对端，校验 codesign：identifier `ai.idoris.cli`，Team `HYAP444FN9`。这与 iDoris 发布 CLI 的签名一致（hardened runtime）；iDoris 保持该标识符稳定，改动须同步 Agent24 的允许清单，也就须走本规范的修订。iDoris 建议的等价要求串：`identifier "ai.idoris.cli" and anchor apple generic and certificate leaf[subject.OU] = "HYAP444FN9"`，并建议再按安装路径核对可执行文件。
2. **端口属主**：经 `LOCAL_PEERTOKEN` 校验过的那个进程，确实持有 `data_port` 与 `admin_port` 的 LISTEN。
3. **实例标识**：`GET /health`（**不带任何凭据**）返回的 `instance_id` 等于登记的 `instance_id`。
4. **最后才发数据 key**：以上全部通过之后，才把数据 key 发给该数据面地址并发布网关槽位。`/health` 免鉴权，带上 key 反而可能把 key 发给占着端口的冒充者，所以 `/health` 请求永不带 key。
5. **开发构建**：只有 agent24d 本身是非打包的开发构建，并且显式设置了 `AGENT24_DEV_ALLOW_UNSIGNED_IDORIS=1`，才接受未签名的登记方；打包版一律拒绝。iDoris 不提供任何绕过。
6. **单实例会话**（Agent24 2026-11-20 已确认，R12 已确认项 4）：agent24d 同一时刻只持有一个健康会话。已有一个 `ack` 过、心跳正常的会话（`instance_id`=A），并且 A 的 `/health` 仍以 `instance_id`=A 应答时，另一个 `instance_id`=B 来登记，回 `instance_conflict`，拒绝后来者，不顶掉现有会话。A 已经失联（心跳超时，或 `/health` 不通、`instance_id` 不符）时，B 正常登记，A 视为陈旧；删除 A 留下的发现文件时，同样要持有旁路锁 `<发现文件>.lock` 并复核 `instance_id`（§2.7.3）。同一个 `instance_id` 重新登记（`resume:true`）不是冲突，新会话替换旧会话。

另有一点：新 agent24d 没有旧 nonce，`instance_nonce` 从发现文件读，不经 stdin 传递（nonce 由 iDoris 生成，设计稿 D1）。

#### 2.10.10 计时与边界口径（冻结时补入）

前面各节没有写清、实现必须做出取舍的地方，在此定为规范，实现与测试以本节为准；与前文重叠处以本节为准，前文已同步。

1. **心跳未应答的计数**：每个 `heartbeat` 发出后 5 秒内没有收到对应应答，算一次未应答；连续 3 次判失联，关闭连接并重连。按默认 10 秒的心跳间隔，第 1、2、3 个心跳分别在 0、10、20 秒发出，第 3 个在 25 秒超时，所以约 25 秒判失联（不是 30 秒）。有效应答（见下一条）把计数清零。
2. **`seq` 应答是累积的**：`heartbeat_ack` 的 `seq` 等于已发出且尚未被确认的某个 `seq` 时，确认该 `seq` 及之前所有未确认的心跳。过期（已被确认）、重复、以及从未发出过的 `seq`（大于已发出的最大 `seq`）一律忽略，既不算应答，也不算错误。
3. **固定等待替代当次退避**：收到 `identity_rejected`、`proof_invalid`、`busy`（以及 §2.10.7 表里的 `unsupported_version`、`contract_incompatible`）时，等待时间取表里的固定值（60、30、5 秒）：只替代这一次的退避等待，**不推进退避计数，也不加抖动**；等待结束后重连，退避序列从原来的位置继续。
4. **EOF 与退避清零**：服务端正常关闭（EOF）之后立即重连，这一次就是退避的第 1 次；一次会话持续满 60 秒才把退避计数清零，不足 60 秒的会话结束后，退避继续按序列上升。
5. **`instance_conflict` 的退出条件**：只有在 iDoris 已经发出 `register` 之后收到 `instance_conflict`，才令进程退出（退出码 3，§2.10.4）；发出 `register` 之前收到的不令进程退出。
6. **`set_admin_token` 的限频是进程级的**：每个进程每 10 秒至多接受一次，跨会话、跨重连合计，重连不重置。非法 `token`（不是 64 位小写 hex）：iDoris 回 `error`（`code` 取 `bad_frame`）并关闭该会话，原 token 继续有效；超频的处理同 §2.10.6。
7. **孤儿计时的起点**：从「没有已 `ack` 的登记会话」起算，**包含握手阶段**：连接、等 `challenge`、发出 `register`、等 `ack` 的这段时间都照常计时，只有一次成功的 `ack` 才清零（§2.10.8）。
8. **发现文件的自愈**：只比对 `instance_id`（不比对别的字段）；`lstat` 目标发现是符号链接、目录或属主不对时，只报错（写日志），不删除也不覆盖，也不当作「缺失」去重写（§2.7.3）。
9. **坏帧**：含 CR 或 BOM 的帧同超长、非 UTF-8 一样是坏帧（§2.10.1）。收到 `challenge` 之前出现任何坏帧，iDoris 关闭连接且不回任何字节（含 `error`），这与「收到 `challenge` 之前不写任何字节」一致。
10. **`bye` 的发送条件**：只在优雅停止（信号 SIGTERM、SIGINT，或退出码 5）、且当时存在已 `ack` 的会话时发送；其他退出路径不发（§2.10.8）。
11. **退出日志写 stderr**：孤儿超时退出（§2.10.8）与 `instance_conflict` 退出（§2.10.4）各一行日志，都写 stderr，不写 stdout。

### 2.11 wire 契约 1.3.0（修订 1.6，已冻结）

本节所有条款（含全部子节）已冻结。只有新增，1.2.x 的调用方不受影响；版本兼容规则同 §2.8：minor 版本只增加内容，不改变已有语义。`CONTRACT_VERSION` 在 iDoris v0.6.0 发版时升到 `1.3.0`（Rust、TS 与 conformance 三处一致，`pnpm check:contract-drift` 校验）；发版之前 `/health.contract_version` 仍是 `1.2.0`，调用方不得据本节假定新增字段已经存在。

#### 2.11.1 `/health` 新字段

保持不变（Agent24 的就绪校验依赖它们）：`status`（恒为 `"ok"`，语义仍是「进程活着并在服务」）、`service`、`version`、`contract_version`、`instance_id`、`components`、`dev_no_key_enabled`。新增：

```json
{"status":"ok","service":"idoris","version":"0.6.0","contract_version":"1.3.0","instance_id":"3f0c6a52-6a0e-4c0e-9a6e-0d2f1c7a9b11","components":2,"dev_no_key_enabled":false,
 "phase":"ready","ready":true,"started_at_ms":1763625330123,"registered":true,
 "upstreams":[{"id":"omlx","engine_kind":"omlx","state":"ready","managed":true,"owned":true},
              {"id":"omlx-embed","engine_kind":"omlx","state":"ready","managed":false,"owned":false}]}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `phase` | string | `starting`：已绑定、已在答 `/health`，但启动门控未完成，且仍在进行（正在探测、拉起引擎或等待它落定）；`ready`：**门控已完成（完整 Router 已替换，§2.7.6）、至少一个 upstream 为 `ready`，且没有托管上游处于终态**；`degraded`：进程正常，但没有就绪的模型服务（门控已完成而没有 upstream 为 `ready`），**或有托管引擎处于终态（`absent`、`failed`、`unavailable`、`stopped`）**：门控不被它拖住，放行之后别的上游照常服务，路由到该托管卡的请求直接 503 并带它的 `reason`（§2.7.6 勘误六）。数据面除 `/health` 外只在门控完成之后才放行，所以 `phase:"ready"` 时 chat 不会因门控而 503；仅有某张 resident 卡探测为 `ready` 而门控未完成，不是 `ready`。没有托管卡时门控视为立即完成 |
| `ready` | boolean | 等于 `phase=="ready"` |
| `started_at_ms` | integer | 同发现文件（§2.7.3） |
| `registered` | boolean | 当前是否有已 `ack` 的登记会话；没有 launcher 时恒为 false |
| `upstreams` | array | 上游列表：Supervisor 管理的卡与 resident 的 HTTP 卡。订阅中转这类进程卡是否列入待定，S1 实现前由 iDoris 确认；`components` 仍计所有卡 |
| `upstreams[].id` | string | 卡的 `provider.id` |
| `upstreams[].engine_kind` | string | 引擎种类，开放集合（§2.12.1）：小写字母、数字、下划线，首字符为字母，≤ 32 字符；v0.6.0 的引擎型上游是 `omlx`，无法识别时为 `unknown`。**每一项都有**；消费者对不认识的取值必须容忍，原样显示，不当错误，也不假定它的行为。来源规则见 §2.3 |
| `upstreams[].state` | string | `ready`、`starting`、`unavailable`、`restarting`、`failed`、`stopped`、`absent`；状态机见 §2.7.6 |
| `upstreams[].reason` | string，可选 | 稳定原因码：`engine_not_found`、`port_in_use`、`auth_required`、`auth_failed`、`startup_timeout`、`crash_loop`、`fence_pending`、`budget_zero`、`base_unavailable`、`startup_latched`。给人看的文字另放，不属于契约 |
| `upstreams[].managed` | boolean | 卡声明了由 iDoris 管理引擎（`extensions.idoris_managed`，§2.3） |
| `upstreams[].owned` | boolean | 引擎进程是 iDoris 自己拉起的；外部引擎、被接管的外部引擎为 false，iDoris 永远不会给它发信号 |

- 示例里的伴生卡 `omlx-embed` 与 `omlx` 共用同一个 endpoint，但不带 `idoris_managed`，所以 `managed` 与 `owned` 都是 false，状态由探测器给出，`engine_kind` 仍是 `omlx`（来源是 `version_pin`，§2.3）；引擎的启停只经托管卡 `omlx`，对 `omlx-embed` 的 `stop`、`restart` 会被拒绝（§2.11.4）。
- **`/health` 必须 O(1)，不做任何上游 I/O，响应 ≤ 200 毫秒。** 状态来自后台探测器与引擎管理器写入的快照。Agent24 每 5 秒探测、2 秒超时、连续 3 次超时就判卡死并 SIGTERM（Agent24 方案 §3.2，Agent24 内部参数），一个会被慢上游拖住的 `/health` 会造成误杀与重启风暴。
- 由 Supervisor 管理的卡，状态取自 Supervisor，带 200 毫秒超时；Supervisor 返回启动错误时报 `unavailable` 加 `reason:"startup_latched"`，**不能**因为 `/v1/models` 能通就报 `ready`（聊天实际是 503）。resident 卡由探测器周期探测：`GET /v1/models`，key 按 §2.9「oMLX 密钥」的顺序解析，间隔 5 秒、2 秒超时；401/403 报 `auth_required` 或 `auth_failed`。
- 调用方按 `state` 判可用性，按 `reason` 判原因，不要假定二者一一对应；`reason` 出现时 `state` 不是 `ready`。已定的配对：`engine_not_found` 对应 `absent`，`startup_latched` 与 `auth_required`、`auth_failed` 对应 `unavailable`，`crash_loop`、`port_in_use` 对应 `failed`；其余原因码的配对以实现为准。
- **`engine_not_found` 总是和同一元素里的 `engine_kind` 一起出现**，说明是哪一种引擎没找到：界面据此引导用户安装那一种（`engine_kind` 为 `omlx` 就引导安装 oMLX），不用猜。
- **启动期行为**（§2.7.6）：`phase:"starting"` 期间，数据面除 `/health` 外的路径返回 503，`error.type` 与 `error.reason_code` 都是 `idoris_starting`（写法同 §2.8 的 `headroom_confirmation_required`），带 `Retry-After`；Admin 的 `GET /admin/api/v1/status` 照常可用。

#### 2.11.2 `POST /v1/models/warmup`

数据面、数据 key，不是 Admin：

```
POST /v1/models/warmup
Authorization: Bearer idk_…
X-iDoris-Privacy: local_only            (缺省 local_only)
X-iDoris-Request-Id: <可选>
X-iDoris-Headroom-Breach: confirmed     (可选，只接受这一个值，语义同 §2.8)
Content-Type: application/json

{"model":"idoris/daily","timeout_ms":120000}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `model` | 是 | 角色别名（`idoris/<role>`）或具体模型 id，选择与角色绑定逻辑同聊天 |
| `timeout_ms` | 否 | 1000 到 300000，缺省 120000；未知字段一律 400 `unsupported_field`（G-6「不静默」） |

- 鉴权与 key 范围同聊天：聊天在该模型上会被拒的 key，预热也被拒。
- 选卡同聊天。**生命周期卡（目前只有 oMLX）**：复用聊天路径里的加载前置（取 catalog、加载前占用检查、带余量确认的 load），加载完成即返回，**不生成任何 token**。**resident 卡**：没有生命周期可管，按能力发一个最小的真实请求（chat 用 `max_tokens:1`，embedding 用输入 `"warmup"`），调用方最多等 `timeout_ms`（请求自己的期限与到点之后的处理见勘误八（5））。
- 200 响应带 `X-iDoris-Record-Id` 与 `X-iDoris-Served-Locality`：

```json
{"model":"<解析后的具体 id>","provider_id":"omlx","state":"ready","already_loaded":false,"load_ms":4210}
```

- 错误沿用聊天的信封与代码（§2.5、§2.8）：400 `invalid_json`、`unsupported_field`；401；403；404 `model_not_found`；503 `headroom_confirmation_required`（界面询问用户，确认后带 `X-iDoris-Headroom-Breach: confirmed` 重发）；503 `local_only_unavailable` 且 `reason_code:"supervisor_busy"`（有限退避后重试）；503 `local_only_unavailable` 的其他原因（终态，例如超过整个额度）；**新增** 504 `warmup_timeout`（超过 `timeout_ms`，加载可能仍在引擎里继续，不等于失败；`error.type` 与 `error.reason_code` 都是 `warmup_timeout`，写法同 §2.8）。
- 副作用：不进用量与预算记录（本地免费）；配置了事件存储时写一条 `warmup` 审计事件；幂等；同一模型的并发预热由 Supervisor 现有的「同 id、同策略」合并处理。
- **实现前提**（不是 wire 字段）：把 oMLX 的 load 调用超时从一般管理调用超时里分离出来。现在 load 与其他管理调用共用 `call_timeout`（默认 10 秒），Supervisor 对 load 另包一层 30 秒；超时按「结果未知」处理，并留下 `load.pending` 围栏。需要新增独立的 `load_timeout`，缺省 180 秒、可配置，只作用于 load 这一次调用，其他管理调用不变；否则冷加载大模型会被当作结果未知，首次启动预热反而让 iDoris 不可用。Qwen3-8B 冷加载实测 3.8 秒（`spike/u0/U0-LOG.md`），§2.9 档位表在 48 GiB 以上推荐的 27B 冷加载时间未实测。
- **实现回填**（修订 1.6 勘误八，文档头）：上面各条的实现口径：请求校验的 `reason_code`、能力的确定（含 `idoris/rerank` 只在 resident 卡上被拒绝）、resident 最小请求的确切形状与上游回答的翻译、`already_loaded` 与 `load_ms` 的含义、`timeout_ms` 与引擎加载超时的关系（`timeout_ms` 与客户端断开不取消、不留围栏，引擎侧的超时或 panic 保持既有的围栏保护）、resident 最小请求自己的 180 秒期限、取消时的审计、`load_timeout` 的取值与环境变量 `IDORIS_LOAD_TIMEOUT_S`。

#### 2.11.3 `/v1/embeddings`：`idoris/embed` 与 key 范围

路由、鉴权、透传和 `local_only` 语义 v0.5.0 就已存在，缺的是默认安装下 oMLX 卡走不通：`/v1/embeddings` 只在 resident 卡里选候选，而默认 oMLX 卡是 `on_demand`，候选为空，返回 503。v0.6.0 用 §2.3 的伴生 resident 卡解决，行为约定如下：

- **请求写 `model:"idoris/embed"`，数据 key 的 `allowed_roles` 含 `embed`。** Agent24 的本地数据 key 角色为 `daily`、`deep`、`embed`（R12-Q6，最小权限，不发全角色 key）。写具体模型名（如 Agent24 现在的 `bge-m3-mlx-fp16`）会被当作无角色请求，无角色请求要求 key 具备全部角色，否则 403 `role_forbidden`。
- `local_only`：缺省即 `local_only`；只有可信本地卡（loopback、`local_only`、egress 只有 none 或 loopback）有资格；响应带 `X-iDoris-Served-Locality: loopback`，Agent24 的 embedding 客户端要求这个值。远程或局域网卡必须 `X-iDoris-Privacy: any` 且 key 允许 `any`；付费卡直接 503 `paid_proxy_unavailable`。多张 embedding 卡走现有的确定性选择管线，不做负载均衡。
- 已知限制（要写进发行说明）：（a）resident 路径绕过 Supervisor，embedding 模型的内存不在 iDoris 账本里，由 oMLX 自己的内存上限兜底（oMLX 配方的托管引擎有 `--memory-guard-gb`，§2.7.6）；（b）冷加载 embedding 模型可能超过代理的 10 秒 header 超时，所以首次启用向量召回前应先调预热（§2.11.2），iDoris 把 embeddings 路径的上游响应头超时放宽到 30 秒（已实现，实现参数，不是 wire 契约；聊天、rerank、messages 仍是 10 秒）；（c）Agent24 一批最多 64 条，响应远小于代理的 8 MiB 上限。
- 以后再做：给 Supervisor 加 embed 命令（带在途计数、驱逐感知、容量记账），账本最准确但改动大，v0.7 以后（设计稿 E2）。
- **v0.5.0 的过渡写法**（不属于 wire 1.3.0，现在就能用）：`setup omlx --load-mode resident --bind embed=bge-m3-mlx-fp16`，2026-11-20 实测返回 200；代价是整张 oMLX 卡变成 resident，chat 失去生命周期内存管理。v0.6.0 起改用伴生 resident 卡，chat 保持 `on_demand`。

#### 2.11.4 Admin：`upstreams` 读与 `start|stop|restart` 写

Admin 从「只有只读接口」改为「有受限写接口」（§2.1 已标注被取代的句子，owner 已定）。路径前缀与既有五个资源相同，鉴权同 Admin token：

| 方法与路径 | 说明 |
|---|---|
| `GET /admin/api/v1/upstreams` | 只读；响应 `{"upstreams":[…]}`，元素字段同 `/health.upstreams[]`（含 `engine_kind`，§2.11.1） |
| `POST /admin/api/v1/upstreams/{id}/start` | 从 `stopped`、`failed`、`absent` 或 `unavailable` 重新走启动决策（§2.7.6） |
| `POST /admin/api/v1/upstreams/{id}/stop` | 停止 iDoris 自己拥有的引擎，状态 `stopped` |
| `POST /admin/api/v1/upstreams/{id}/restart` | 先 `stop`，再重新走启动决策 |

**Admin 写接口通则**（#611 的模型 load/unload、§2.9 的下载与预留的默认引擎切换 `make_default`（§2.12.3）另行修订时沿用，本版不冻结它们的路径与字段）：

- 方法 POST，不需要请求体；路径形如 `/admin/api/v1/<资源>/{id}/<动作>`；鉴权同其余 Admin 路由（Admin token）。
- 幂等：目标状态已达成时，重复请求仍返回 202 与当前状态。
- 返回 **202** 加该资源的当前状态对象（字段同对应的 GET）；动作可能异步完成，调用方轮询 `GET` 或看 `/health` 到终态。
- 只作用于 iDoris 自己拥有或托管的对象：`owned` 为 false 的上游，`stop` 与 `restart` 一律 409 `upstream_not_owned`，`managed` 为 false 的上游，`start` 同样 409 `upstream_not_owned`；iDoris 不给别人的进程发信号。`id` 不存在则 404 `upstream_not_found`。各错误体字段以实现为准（§2.5）。

**实现回填（S6，2026-11-21，勘误十；只增不改上面的冻结条款）。**

- **元素**（`GET` 的 `upstreams[]`，也是 `POST` 的 202 体；wire 仍 1.2.0）：`id`、`engine_kind`、`state`、`reason`（可选）、`managed`、`owned` 与 `/health.upstreams[]` 同源同口径（托管上游的值来自引擎管理器发布状态时同时写 `/health` 的那一次），另有 Admin 专属的只读字段，**只增不改**：`port`（卡的 endpoint 里的端口；没有对应的卡就没有）、`pid`（只有 `owned` 才有）、`restart_required`（布尔，每个元素都有：放行时没就绪而冻结的上游为 true，见勘误十）、`failure_count`（托管上游才有：状态进入 `restarting` 或 `failed` 的次数，iDoris 启动以来累计，`unavailable` 不算）、`last_failure_at_ms`（有过失败才有；墙钟 Unix 毫秒）。整数范围两侧一致（Rust 类型、JSON Schema、TypeScript 生成类型）：`port` 1 至 65535，`pid`、`failure_count` 0 至 4294967295（`u32`），`last_failure_at_ms` 0 至 9007199254740991（JavaScript 的安全整数上界，超过它的值 JSON 里读不回来）。**没有任何 key、token、nonce 的位置**：schema `admin-v0-upstream.schema.json`（元素）与 `admin-v0-upstreams.schema.json`（`{"upstreams":[元素…]}`）两侧都是 `additionalProperties:false`，`state`、`reason` 是封闭枚举（与 §2.11.1 一致）。顺序同 `/health.upstreams`。启动门控期间 `GET` 与三个 `POST` 在通过认证之后一律 503 `idoris_starting`（§2.7.6，没有 token 仍是 401）。
- **错误体**是统一的 `{error:{type,rule_id,reason_code,evidence,remediation}}`：404 `upstream_not_found`（`type` 与 `reason_code` 同名）；409 `upstream_not_owned`，`reason_code` 是 `not_managed`（卡没有声明托管：resident 卡、`setup upstream` 的卡、伴生卡，三个动作都是）或 `not_owned`（托管卡但引擎不是 iDoris 拉起的：外部引擎、没有引擎，`stop` 与 `restart`）；409 `restart_required`（同族新增：放行时没就绪而冻结的上游的 `start`、`restart`）；503 `idoris_stopping`（同族新增：引擎管理器已在停止，命令没有受理）。判定的先后：id → `managed` → 归属（`stop`、`restart` 要 `owned`）→ 管理器（它回答 `restart_required`）；所以 `owned` 为 false 的上游，`restart` 一律是 `upstream_not_owned`（上面「一律」），`restart_required` 只出现在冻结上游的 `start` 上（冻结的上游没有进程，`owned` 恒为 false）。
- **幂等与异步**：202 在命令被受理时就回答，体是**受理之后**的元素，而且受理之前已经把状态换成了「不可服务」的：`start` 受理时是 `starting`；`restart` 受理时是 `restarting`，从受理一直到新引擎就绪都是（Agent24 对 `restarting` 有界等待）；这是操作员要的重启，**不记失败**：不进熔断窗口，`failure_count` 不加；`stop` 受理时是 `stopped`（数据面立即 503，Supervisor 不再准入新的加载与聊天）。引擎还握在手里的几步（排空在途的加载与聊天、SIGTERM 与宽限）里，元素的 `owned` 仍是 true、`pid` 仍在：`stopped` 加 `owned: true` 是「已经不服务、进程在退出」，进程确认消失之后才是 `owned: false`、没有 `pid`。没能确认进程消失（读不出它的身份、不能给它发信号）时不能说它停了：收尾是 `unavailable` + `port_in_use`，围栏不动，端点保持封上。202 不是完成：`stop` 看 `owned` 变成 false，`restart` 看 `pid` 换了并且状态回到 `ready`。已经 `stopped` 的 `stop` 是 202；已经 `ready` 的 `start` 是 202；已经在（重新）启动的 `restart` 是 202（并入正在进行的那一次：并发的两个只换一次引擎，见勘误十（5））；并发的两个 `stop` 都是 202，引擎只停一次。没有请求体，有也不读。
- **`start`、`stop`、`restart` 在门控放行之后的语义**见勘误十（2）：放行时就绪的上游，`restart`、`stop` 之后的 `start` 换一个新的引擎进程，Supervisor 的账本在发布 `ready` 之前复位；冻结的上游 `start`、`restart` 是 409 `restart_required`；次序是关门、发布不可服务的状态、排空、停进程、封上端点、复位、开门；`stop` 在进程确认消失之后才换掉私有 key 的登记（端点封上，不是撤销）并清空账本。
- **审计**：引擎管理器对每个受理的命令留一行日志（上游 id、命令名、结果；没有 token、key；不认识的 id 不进日志）。Admin 没有速率限制（没有这个习惯：Admin 只在 loopback，由持有 token 的本机调用方使用）。

#### 2.11.5 工件与验收

本节没有 JSON Schema（§4.2）：`/health` 新字段的形状由 conformance 用例锁定，warmup 的状态码矩阵和 Admin `upstreams` 由各自的集成测试覆盖，都在实现片里补，不在本修订里创建。发版时 `CONTRACT_VERSION` 升到 `1.3.0`。

### 2.12 多引擎（修订 1.6，预留，v0.6.0 只实现 oMLX；待后续修订冻结）

owner 2026-11-20 追加：iDoris 以后要支持多个引擎（oMLX、LM Studio、Ollama、llama.cpp、mlx_lm 等），引擎之间可以切换，默认引擎可以指定、可以修改。v0.6.0 只实现 oMLX，但契约按多引擎设计，以后加引擎时不改已冻结的字段。本节分两类：**进入 wire 1.3.0 的**只有「现在不定下就会返工」的那几项，它们已经写在 §2.2、§2.3、§2.7、§2.11 的对应位置，§2.12.6 的总表逐项指出，和修订 1.6 的其余部分一样已冻结；**其余都是预留**，待后续修订冻结，v0.6.0 不实现，也不得当作当前 API（同 §0 末句）。本节写出方向和理由，目的是让 v0.6.0 的实现不返工。

#### 2.12.1 引擎、引擎种类与配方（`engine_kind` 进入 wire 1.3.0，已冻结；其余预留，待后续修订冻结）

- **引擎种类 `engine_kind`**：开放集合的字符串，小写 ASCII 字母、数字、下划线，首字符为字母，≤ 32 字符（`^[a-z][a-z0-9_]{0,31}$`）。上面的格式规则只是语法；另有一组**保留名**，拼写现在定下，不接受别名：`omlx`（v0.6.0 唯一的实现）、`lmstudio`、`ollama`、`llama_cpp`、`mlx_lm`、`openai_compat`（不属于任何具体产品的 OpenAI 兼容服务，只探测、不托管）。保留名不等于已实现。`unknown` 是第七个特殊值，满足格式，但**只由 iDoris 在输出里使用**（`/health`、发现文件、Admin），表示无法识别种类；它**不是可配置的取值**，不能写进 `idoris_managed.engine`（写了按卡校验失败处理），也不会出现在 `version_pin` 之外的任何输入里。
- **开放集合规则**：消费者对不认识的取值必须容忍：原样显示，不当错误，不假定它的行为。加一种引擎只是加一个取值，不改任何已冻结字段的形状。iDoris 自己遇到本版本不认识的 `idoris_managed.engine`，按卡校验失败处理，启动失败，不静默忽略。
- **引擎与上游**：上游是组件卡在 `/health` 与 Admin 里的呈现，id 是 `provider.id`。一个引擎实例（种类加 endpoint）可以对应多张卡：主卡承载引擎的生命周期与聊天类能力，伴生卡（§2.3）共用同一个引擎，没有自己的生命周期。
- **`engine_kind` 的来源**（每张引擎型卡恰好一个，规则见 §2.3）：有 `idoris_managed` 的卡取它的 `engine`；否则取 `version_pin` 里 `@` 之前的产品名（`omlx@0.6.4` 得 `omlx`），不符合上面的格式则为 `unknown`；两者同时存在而不一致，卡校验失败。
- **配方**：每种引擎种类有一套配方：怎么识别并探测（是不是这种引擎、是否就绪、是否要鉴权）、怎么发现并校验可执行文件、怎么拉起、是否支持私有 key、模型级适配器怎么对话。v0.6.0 只有 `omlx` 配方（§2.7.6、§2.7.7）。加一种引擎需要且只需要：一个新的 `engine_kind` 取值（保留名已定）、一套配方、该种类的 setup 子命令（需要托管时）、launcher 帧 `engine.executables` 里的一个新键（需要托管时）；**不需要**改 `/health`、Admin、发现文件、登记协议、launcher 帧里任何已有字段。
- **setup 的形态（预留，二选一已定）**：每种需要托管（拉起与停止）的引擎各有一个 `setup <engine_kind>` 子命令；`setup omlx` 的名字、形状和既有的 11 个错误码保持不变；不托管的 OpenAI 兼容引擎统一由 `setup upstream`（§2.3，最小版 v0.6.0）生成；**不做**通用的 `setup engine --kind`。理由：（1）`setup omlx` 的形状与错误码已冻结，而且是 oMLX 专用语义（`not_omlx`、`--managed` 等），保持原样就不需要别名与兼容规则；（2）各引擎的探测方式、可托管的程度、要不要模型路径参数差别很大，通用命令要么变成「随种类变化的 flag 矩阵」，要么丢掉各自的错误码；（3）多数引擎（LM Studio、Ollama 的兼容端点、llama-server 等）只需要登记一个 loopback 端点，由 `setup upstream` 一个命令覆盖，不会出现每种引擎一个命令的局面。

#### 2.12.2 引擎登记：同时知道多个引擎（预留，待后续修订冻结）

- 引擎登记就是组件目录里有哪些引擎型卡：iDoris 启动时读取，对每张引擎型卡建立一个引擎条目。一个 iDoris 可以同时知道多个引擎实例，同种类的也可以有多个（例如两个端口上的 LM Studio）。每个实例独立：状态机、所有权记录、日志、Admin 的 `upstreams/{id}`（路径按上游 id 寻址，与引擎种类无关，多引擎下保持不变）。v0.6.0 里组件目录本来就能装多张卡，引擎型的只有一个 oMLX 实例。
- **实现要求**（不是 wire 字段）：v0.6.0 的引擎管理器按「引擎实例」建模，实例带 `engine_kind`，按它分派到配方，不得写成「全局只有一个 oMLX」；这样加引擎种类不用重构。
- 增删引擎：预留阶段是 `setup` 写卡后重启 iDoris 生效；免重启的热登记（Admin 写接口）留待后续修订。
- 内存额度 `IDORIS_ALLOCATED_BUDGET_BYTES` 是整机分给 iDoris 的总额度，**不按引擎拆分**。多个引擎同时驻留模型时的统一账本与跨引擎驱逐属于预留，在第一个多引擎版本之前要定下；v0.6.0 里 Supervisor 的账本只覆盖 oMLX，切换时的影响见 §2.12.4。

#### 2.12.3 默认引擎（预留，待后续修订冻结）

- **含义**：默认引擎是一个上游 id（引擎主卡的 `provider.id`），表示调用方没有另行指定时、聊天类角色优先使用的引擎。它只是选择阶段的**优先项**，排在隐私、角色与能力、预算、容量这些闸门之后，不放宽任何闸门，也不改变候选资格：默认引擎合格就先选它；不合格（未就绪、没有该角色的绑定、被闸门排除）时，沿用现有的确定性选择顺序选下一个合格候选，不为此新增例外。「默认」不是「独占」。
- **指定（静态）**：（a）`setup <engine_kind>` 的 `--default`，在该卡上写 `extensions.idoris_default: true`，至多一张卡带它，两张以上卡校验失败；（b）launcher 帧 `engine.default_upstream`（上游 id），由 Agent24 在每次启动时按它保存的用户选择下发，优先于（a）。`default_upstream` 指向的上游不存在或没有聊天能力时，iDoris 照常启动，按没有指定处理并记一行日志，不因此失败（用户可能已经删掉那个引擎）。都没有指定时：只有一个具备聊天能力的引擎型上游，它就是默认；有多个而没有指定，则没有默认，选择顺序沿用现有规则。v0.6.0 的 `--launcher-stdin` 不接受 `default_upstream`，`setup omlx` 也没有 `--default`；加上它们时 iDoris 与 launcher 一起升版本。
- **修改（运行期）**：选定 `POST /admin/api/v1/upstreams/{id}/make_default`。理由（对比 `PUT /admin/api/v1/upstreams/default`）：（1）它属于 §2.11.4 的同一族写接口：POST、不需要请求体、返回 202 加该上游的当前状态、幂等、同一个 Admin token，Agent24 与测试夹具只多一条同形的路径，也不用再冻结新的请求体形状；（2）`PUT /admin/api/v1/upstreams/default` 与 `provider.id` 恰好叫 `default` 的上游同路径，而 id 是自由字符串，`{id}/make_default` 的动作段取自封闭集合，不会撞名；（3）错误沿用 §2.11.4 同一族的码：id 不存在是 404 `upstream_not_found`，不具备资格（不是引擎型上游，或没有聊天能力，例如伴生卡）是 409 `upstream_not_eligible`（同族新增）；（4）对当前默认再调一次，仍是 202 加当前状态。代价是没有「取消默认」：默认只会换到另一个引擎，不会变回「没有」，要回到「没有」就去掉静态指定后重启，这对偏好型的默认可以接受。修改不要求目标处于 `ready`（允许先选后装），响应里的 `state` 与 `reason` 让调用方决定要不要提示用户。
- **不持久化**：iDoris 不记忆用户的选择（同 §2.9：Agent24 保存，iDoris 不记）。运行期的修改在重启后回到静态指定，Agent24 保存用户的选择，并在每次启动时经（b）下发。这样不用让 iDoris 去写 launcher 与 Agent24 拥有的配置，也只有一处事实来源。

#### 2.12.4 切换的语义（预留，待后续修订冻结）

- **只影响之后的新请求**：每个请求在选择阶段读取一次默认引擎。`make_default` 返回 202 之后才开始选择的请求用新的默认，此前已经选定引擎的请求不受影响。
- **在途请求**：不中断、不重试、不迁移，在原引擎上跑完，`response.model` 如实反映原引擎；已经派发给原引擎、在它的生命周期队列里等待加载的请求同样留在原引擎；已开始 token 后不重试（既有规则）。预热同理：切换前发出的预热在原引擎上完成，所以 Agent24 应当先切换、再预热。
- **内存里已加载的模型**：切换本身不加载、不卸载、不停止任何东西。旧引擎里的模型保留到它自己的空闲回收（oMLX 的 `idle_ttl_s`）、显式 `stop`（自有引擎，§2.11.4）或 `unload`（#611，另行修订）。理由：切换只是路由偏好，副作用越小越好，一次点击不应触发大模型的卸载与加载。
- **切换与内存**：新默认引擎的首次请求走它自己的容量准入（含 §2.8 的 1GB 余量确认）。旧引擎的驻留模型仍占着整机内存，iDoris 不会为新引擎去驱逐另一个引擎里的模型（跨引擎账本是预留，§2.12.2），放不下就按既有的 503 规则失败。Settings 在切换后可以提示用户停止旧引擎（Admin `stop`，仅自有引擎）。
- **`/health.phase`**：以 §2.11.1 的定义为准：门控已完成、至少一个上游 `ready` 且没有托管上游处于终态是 `ready`，不看默认引擎；托管的默认引擎处于终态（降级放行，§2.7.6 勘误六）时是 `degraded`，别的上游仍照常服务，请求按 `upstreams[]` 与角色路由。Agent24 的「模型可用」门按 `phase` 判（§2.7.4）：`degraded` 而另有上游 `ready` 时怎么放行，由 Agent24 按 `upstreams[]` 决定，本节不替它定。
- **落点如实回传**：默认引擎不合格时请求会落到别的引擎（§2.12.3），`response.model` 已是具体模型 id；另预留响应头 `X-iDoris-Upstream`（实际服务的上游 id，名字待定），让调用方发现「没有落在默认引擎」（G-6「不静默」）。需要「默认引擎不可用就失败」的严格模式时，建议用请求头显式声明（类似 `X-iDoris-Fallback`），不做成全局开关；是否需要由 Agent24 提出。

#### 2.12.5 与角色绑定、§2.9 推荐的关系（预留，待后续修订冻结）

- `idoris_role_models` 是**每张卡自己的**：角色到该引擎里具体模型 id 的映射。`idoris/<role>` 解析时默认引擎的卡排第一，所以**同一个角色名在切换默认引擎之后会指向另一个具体模型**（`response.model` 随之变）；默认引擎没有该角色的绑定时，该卡不是候选，落到其他引擎的绑定（同样如实回传）。
- **`embed`、`rerank` 不随默认引擎变化**：它们的候选顺序不受默认引擎影响，沿用现有的确定性选择，由各自的绑定决定。理由：embedding 模型决定向量空间，切换引擎而悄悄换了模型，会让 Agent24 已存的向量全部失效。要换 embedding 模型，须显式改绑定（`--bind embed=…`），并由 Agent24 重建索引。
- **§2.9 的推荐按默认引擎可用的模型来给**：`GET /v1/recommendation?role=chat` 与 `GET /v1/models/local` 默认针对默认引擎，并预留查询参数 `engine=<上游 id>`（切换前预览另一个引擎）和响应里的 `engine: {id, engine_kind}`；`idoris recommend`、`idoris models local` 的命令行对应预留 `--engine`。§2.9 的档位表给的是 oMLX 目录名，只对 `engine_kind` 为 `omlx` 的引擎有意义；没有目录映射的引擎种类返回 `model_id: null`、`reason:"engine_not_supported"`。v0.6.0 里这些都不变（只有 oMLX），也不发出新字段。
- 切换之后，Agent24 保存的默认 chat 模型（§2.9）是原引擎的具体模型 id，可能不存在于新引擎，Settings 应当在切换后重新取推荐与模型列表。

#### 2.12.6 Agent24 Settings 的读接口与总表（已冻结；其中标「预留」的待后续修订冻结）

Agent24 经 agent24d 的 `/api/v1/idoris/*` 访问 Admin（R12-Q3）。Settings 需要的读接口：

| 需要 | 接口 | 状态 |
|---|---|---|
| 引擎列表（种类、状态、是否托管、是否自有） | Admin `GET /admin/api/v1/upstreams`，以及免 token 的 `/health.upstreams`，元素都带 `engine_kind`（§2.11） | wire 1.3.0，v0.6.0 实现 |
| 当前默认引擎 | 同一元素里新增布尔字段 `default`：至多一个 true，没有默认时全为 false | 预留 |
| 每个引擎的模型 | Admin `GET /admin/api/v1/models`（Admin v0 已有，按 `provider_id` 分组）；`GET /v1/models/local` 的 `engine` 参数 | Admin v0 已有；`engine` 参数预留 |
| 每个引擎已绑定的角色（切换前提示缺哪些） | 同一元素里新增 `roles`（角色名数组） | 预留 |
| 本版本支持哪些引擎种类（「添加引擎」用） | 只读路由，名字待定 | 预留 |
| 切换默认引擎 | `POST /admin/api/v1/upstreams/{id}/make_default`（§2.12.3） | 预留 |

哪些进入 wire 1.3.0，哪些只是预留：

| 项 | 状态 |
|---|---|
| `engine_kind`：取值规则、保留名、开放集合的容忍规则（§2.12.1） | wire 1.3.0 |
| `/health.upstreams[].engine_kind` 与 Admin `upstreams` 的元素（§2.11.1、§2.11.4） | wire 1.3.0 |
| `engine_kind` 的来源规则，`idoris_managed.engine` 的取值就是 `engine_kind`（§2.3） | wire 1.3.0 |
| `engine_not_found` 与同一元素里的 `engine_kind` 同时出现（§2.11.1） | wire 1.3.0 |
| 发现文件 `launch.engines`（§2.7.3） | wire 1.3.0 |
| launcher 帧 `engine.executables`，键为 `engine_kind`（§2.7.1） | wire 1.3.0 |
| `setup omlx` 输出的 `engine` 对象里的 `engine_kind`（§2.7.8） | wire 1.3.0 |
| 所有权记录按种类分目录并带 `engine_kind`，日志按种类命名（§2.7.6） | 内部格式，随 v0.6.0 |
| 多个引擎实例同时登记、独立管理（§2.12.2） | 预留 |
| 默认引擎：`idoris_default`、`engine.default_upstream`、`make_default`、元素 `default`（§2.12.3） | 预留 |
| 切换的语义、响应头 `X-iDoris-Upstream`（§2.12.4） | 预留 |
| 推荐与本地模型列表的 `engine` 参数、`engine_not_supported`（§2.12.5） | 预留 |
| 元素 `roles`、引擎种类清单路由（本节上表） | 预留 |
| `setup upstream` 最小版（§2.7.8） | wire 1.3.0 之外的 CLI 承诺，v0.6.0 |
| `setup <engine_kind>`（§2.12.1） | 预留 |
| 免重启的热登记、跨引擎的内存账本（§2.12.2） | 预留 |

## 3. 拟议 v0.3.1 local_only 数据面切片（待联合验收）

1. 用户独立准备并启动 iDoris/后端/已安装权重；数据面与 launcher 独立。Agent24 显式配置 loopback IDORIS_URL 与 iDoris 签发的 local_only 数据 key，目标安全存储为 Keychain，不把 Admin token 当数据 key。未决定 keychain service/account、轮换 CLI 或期限默认值。
2. 配置后独占 idoris-local，无 oMLX/Ollama 旁路；只发约定角色和三个头，非流式，云端补位默认关闭。auto 不作为默认猜测；具体模型 override 仍受 key 权限约束。
3. iDoris 运维明确绑定 HTTP role→真实模型；iDoris 推荐并负责 load/evict。Desktop 展示绑定/可用性及真实 response.model/Record-Id/locality，缺模型显示不可用，安装流程见 R9。不能把静态推荐表、GUI 高亮或无行为 Download 按钮当已安装/已选中证据。
4. 每次结果必须通过 locality 绊线；负对照（无 key、缺本地、缺头/lan/remote、不兼容字段）明确失败且远程/直连后端计数为零。预算推荐和实际并发加载限制分别验收。
5. 未有配对 launcher 时 Admin 状态卡可继续 unavailable，数据面可独立验收。Admin 组合验收仍是完整 M4 readiness 的独立缺口；文档完成不授权启动 M5。M4 接入前置（main/IDORIS_PORT/响应头/health 身份）不等于完整用户路径完成。

**目标而非实现**：max_tokens 兑现及非法值校验、chat/load timeout 分离、角色版 capabilities、绑定模型可用性检查、结构化 ready/instance 校验与启动失败清理。未来字段、超时数值、ready 行格式和 runtime 版本均不得在本修订中当作冻结 API；不得照设计审计草稿新增 FD framing、write API、downloader 或角色标签。

## 4. 实际/候选/目标与工件映射

**预算回落实现差距（B1）**：R6/R7 已接受“显式声明 Fallback 可回落到免费候选并回传降级”。I0 `idoris-policy::pipeline::budget_stage` 有 `allow_fallback()` 与 `Degradation::BudgetFallback` 分支及测试，但 `idoris-router/src/dispatch.rs` 传入 `PolicyCtx { budget: None }`，实际报价后原子 reserve 失败直接返回402。因此 I0 router 在有无声明时都仍为402，不能把 policy 核心的已测能力写成运行时已接线；031验收要如实验证该差距，不能反转既有规范。

以下源码及测试名称按 I0/A0 固定对象静态核对；**本次未运行软件测试**，已有测试只证明对应层，不代表跨仓组合验收。

| 事实/差距 | 精确源码、工件与已有测试 | v0.3.1 所需证据 |
|---|---|---|
| wire 仍 1.0.1；health 含身份 | I0 `crates/idoris-contracts/src/version.rs`；`crates/idoris-router/src/lib.rs::health` / `health_has_the_pr46_shape_and_a_record_id_header`；`conformance/tests/response-headers.test.ts` | 明示支持版本与身份错误负对照，不猜 1.2.0 |
| 默认 virtual key 与开发限制 | I0 `crates/idoris-router/src/auth.rs::enforce_scope`、`key_issue.rs::IssueSpec`；`src/virtual_key_wiring_tests.rs::{scope_and_fallback_fail_403_before_any_upstream_call,dev_no_key_rejects_any_paid_remote_and_invalid_bearer_before_egress}` | 真实 daemon/CLI/Desktop/module 各路径 scope 校验 |
| role 被接受但未接真实模型 | I0 `crates/idoris-router/src/dispatch.rs::{candidate,backend_model_id}`：role 请求用 provider ID；`src/lib.rs::chat_completions_rejects_unknown_role` 仅证明未知 role 拒绝 | 有绑定 daily 成功，load/chat 同 ID；未绑定 auto/deep 503、零上游调用 |
| allocation 仅进入推荐 | I0 `crates/idoris-router/src/bin/idoris.rs::run`、`src/capabilities.rs::LiveCapabilitiesProvider`；`crates/idoris-recommender/src/recommend/resident.rs`；`src/runtime.rs::spawn_with_factory` 仍取默认 budget | 0/有限字节并发 load 的实际计数/ledger，不能用推荐结果替代 |
| capabilities 为旧条目 | I0 `crates/idoris-router/src/capabilities.rs::CapabilityEntry`；`tests/capabilities.rs::injected_provider_returns_top_level_array_with_reference_shape_and_record_id` | 角色版是未来字段，不冒充现有 schema |
| Admin 只读，已有 intake | I0 `crates/idoris-router/src/admin.rs::authenticated_admin_router_exposes_only_the_five_read_routes`；A0 `apps/desktop/src/main/idoris-admin.ts::{parseAdminTokenFd,parseTokenFrame,readAdminTokenFromFd}` | 可信 producer、错误 FD/继承/超时安全组合证据仍缺 |
| A0 未接 iDoris 数据面/预算发送 | A0 `rust/crates/agent24-models/src/router.rs::ModelRouter::from_env` 为 oMLX/Ollama；`apps/desktop/src/main/backend-manager.ts` 启 agent24d | 真实 provider 接线、独占、locality 绊线；修改 OMLX_URL 不是正式集成 |
| max_tokens/timeout 不匹配 | I0 `crates/idoris-router/src/lib.rs` unsupported_parameter 分支、`crates/idoris-upstream/src/omlx/mod.rs::OmlxAdapterConfig`（共用 call_timeout）；A0 `rust/apps/agent24d/src/model_callback.rs`、`rust/apps/agent24-documents/src/extract/run.rs`；iDoris FU-206/FU-25（`docs/agent/tasks.md`） | 参数保留到上游、非法值拒绝、长生成不被 load timeout 截断；仍是目标 |
| readiness 尚非结构化协议 | I0 `crates/idoris-router/src/bin/idoris.rs::run` 只打印 listening 文本；`tests/cli.rs::occupied_admin_port_fails_before_data_health_is_served` | 不发明 IDORIS_READY 为当前输出 |

### 4.1 I0 之后的源码与候选（不是 v0.3.0 发布能力）

| 候选 | 固定提交 | 证据范围/限制 |
|---|---|---|
| [#507](https://github.com/iDoris-ai/iDoris/pull/507) onboarding | `543b008121760f9b3eb1264c25803b5ca38cf6fc`（OPEN；该head已获PRD批准，后续base/head移动须复审） | `docs/安装与首次调用-v0.3.0.md`、`scripts/test_onboarding_smoke.py`：iDoris 归档 fixture，非 Agent24 真实进程联调 |
| [#508](https://github.com/iDoris-ai/iDoris/pull/508) key ceiling | `1c9c592ffb8f8226e873cfbbc242abf2fe24e747`（已合并至 `959337d`） | `crates/idoris-router/src/lib.rs`、`src/virtual_key_wiring_tests.rs`：防 personal key 提升与鉴权前事件泄露；已经进入 main，但不属于 I0 发布二进制，补丁版仍需完整验收 |
| [#510](https://github.com/iDoris-ai/iDoris/pull/510) role binding | `6a9b37737d807983f78d55d719326f9771f183b8`（OPEN；该head已获PRD批准，后续base/head移动须复审） | `crates/idoris-router/src/dispatch.rs::role_models` 与 `src/role_binding_tests.rs`：显式运维 map/auto、缺失 fail-closed；HTTP 多协议与 lifecycle 共享具体目标；无映射 fixed subscription 保留既有角色资格，执行身份仍固定；不是 Agent24 已联调的证据 |
| [#511](https://github.com/iDoris-ai/iDoris/pull/511) 精确容量基础 | `0c0d34347436f73f6816e161af8a687248666761`（已合并至 `9395a96760f450f7f88f916137659c05290f2008`） | `crates/idoris-backend/tests/runtime_exact_charge.rs`；已进入main，非I0产物，仍不等于启动预算已接线。 |
| [#512](https://github.com/iDoris-ai/iDoris/pull/512) 启动预算接线 | `f1da0c7aca7fa0c0f4204f09790a1c141fd48682`（OPEN；新head待复审） | `crates/idoris-router/tests/runtime_exact_growth.rs`；候选真实runtime额度接线，尚未合并。 |

**2026-10-15 后续状态**：#511 已以精确head批准、9绿CI、CLEAN普通合并为 main `9395a96760f450f7f88f916137659c05290f2008`；#512已转base main并同步到 `f1da0c7aca7fa0c0f4204f09790a1c141fd48682`，需新head复审。#479 nullable cap修复e3426f6已批准，父层安全修复同步后当前73051b1仍须以新head门禁为准。当前状态以GitHub为准，这些不是I0产物内容。

### 4.2 Schema 清单（I0 实际只有 15 份）

- 核心 9 份：`task-profile`、`tenant`、`deploy-mode`、`provider`、`component-card`、`load-policy`、`routing-policy`、`adapter-manifest`、`training-sample`，路径均为 `packages/contracts/schema/<name>.schema.json`。
- 另有 `role.schema.json` 与五份 `admin-v0-{status,backends,models,runtimes,roles}.schema.json`。Admin roles schema 不是角色版 capabilities。
- 机器一致性入口：`crates/idoris-contracts/tests/contract_drift.rs`、`tests/parity.rs`（均相对该 crate）；Admin wire 另由 `crates/idoris-router/src/admin.rs` 的 `*_wire_shape_matches_the_shared_v0_contract` 测试核对。它们不证明未存在的 schema。
- **不存在/未交付的目标工件**：request/response-headers、health、error-body、virtual-key、capabilities 角色版、feedback、systemone request/response、inspect、trajectory、通用 admin proposals/confirm schemas。旧表的“新增/修订”是待办，不是“七份 schema 已发布”。未来只为实际消费的接口补工件与 conformance，另行审阅，本文不创建 schema。
- **修订 1.6 增补（已冻结）**：本修订新增的 launcher 帧、发现文件、登记协议帧、`/health` 新字段、`POST /v1/models/warmup` 和 Admin `upstreams` 都没有 JSON Schema，以文字规范为准，登记协议另附测试向量（§2.10.5）；`/health` 仍无 schema，新增字段的形状只靠 conformance 用例锁定。机器真源仍只限于已有 schema 覆盖的 wire 结构。

## 5. 历史承接与被取代条款

历史保留用于追溯，不能构成第二份当前契约。以下既有细则继续保留而非退役：未声明角色时回传所选角色；requires_models可声明角色名；外部Jev仅用于允许离机的请求；修订遵循“提议→双方同意→升版本”。这里只恢复规范承接，不宣称相应目标工件已全部实现。

| 历史材料 | 继承/更正 |
|---|---|
| [R1 全文](negotiation/R1-Agent24-答复.md)、下方 R0–R8 | D-1..D-7、J-1..J-8 保留；原 §3.12/3.13 的内容已合入 §2，不再后文覆盖前文 |
| I0 [原 v1.1 全文](https://github.com/iDoris-ai/iDoris/blob/03c079c70611331a8c85a1666e32a75df6359d09/docs/interfaces/iDoris-Agent24-边界与接口规范.md) | 旧“每次预检 capabilities”改为展示/挂载检查；Evolver 读 iDoris 导出改为 Agent24 自己事件日志；loopback 默认免 key 被 R8 取代 |
| [旧 09](../09-对Agent24的需求.md)、[旧 17](../17-模型能力设计-Agent24协作草案.md)、[旧 handoff](../agent/handoff-agent24.md) | 已废弃历史；模型/推荐归属继续有效，旧端口、TS smoke、旧模型排名不是 Rust release 验收 |
| [Agent24 旧入口路由设计（A0）](https://github.com/iDoris-ai/Agent24/blob/6072fd373142eb3dbf1a6a4a3222e6eec82c74f6/docs/iDoris-integration-and-entry-router.md)、[ADR-032 集成材料（A0）](https://github.com/iDoris-ai/Agent24/blob/6072fd373142eb3dbf1a6a4a3222e6eec82c74f6/docs/design/INTEGRATION-AGENTEAR-IDORIS.md) | 继承 LocalOnly/动作授权/Served-Locality/不拥有 daemon；“尚无入口/未合并”的历史文字过期；Agent24 短指针由其 owner 独立修改 |
| [M4 landing handoff](../agent/HANDOFF-2026-10-11-M4-LANDING-TAIL.md) | Admin load/unload 未交付，Admin v0 只读；不是全量 Admin API 的完成声明 |
| 总体规划 §4.2/§6.1 | 本次只纠正默认 key 与 Admin v0 只读；保留未来管理面设计及历史链接 |

## 6. R9 与剩余决定

见 [R9 协商提案](negotiation/R9-v1.2-协商提案.md)。launcher 宿主（OD-1：agent24d）和同意后安装执行方式（OD-2：由用户在桌面端同意）已由修订 1.3 决定，契约见 §2.7，以 §2.7 为准（**OD-2 被修订 1.6 取代**：owner 2026-11-20 定 iDoris 为 Agent24 必选核心，随 Agent24 安装包自动安装，不再单独征求用户同意；模型、引擎等大文件下载仍须用户确认，不受影响）；fast/daily 标签重标仍 **OWNER REVIEW PENDING**，未经决定不新增角色标签或 M5 consent 机制。数据面可以独立于 launcher 推进；完整 readiness 仍需组合与干净环境验收。

## 7. 协商记录（历史原样保留）

| 轮次 | 日期 | 发起 | 内容 | 结论 |
|:---|:---|:---|:---|:---|
| R0 | 2026-09-27 | iDoris | 本草案 v0.1 发给 Agent24（agent24-13）review | Agent24 已确认收到，正在对照代码与 ADR-032/ME4-S2/A3 起草 Q-1…Q-9、§3 逐条意见与 ATIF 对齐评估；需 jason 拍板的点将单列；同意把 ADR-032 §9「一行都没合并」标为过期 |
| R1 | 2026-09-27 | Agent24 | 答复全文见 [`negotiation/R1-Agent24-答复.md`](negotiation/R1-Agent24-答复.md)。<br>• Q-9：同意模型管理全部归 iDoris。<br>• 更正两处事实：L3 从未实现；v0.1 漏列 Desktop 的 oMLX 管理。<br>• 分歧一处：Evolver 的数据源。<br>• §3 逐条意见。<br>• 另列 D-1…D-7 请 jason 拍板。 | — |
| R2 | 2026-09-27 | iDoris | **全部采纳** R1 的意见，包括撤回「Evolver 读 iDoris 导出」。<br>• 新增 §3.12。<br>• 回答 fast/daily 的区分标准。<br>• D-1…D-7：iDoris 同意 Agent24 的推荐选项（均为 (a)）。 | **待 jason 拍板 D-1…D-7**；拍板后转 JSON Schema，定稿 |
| R3 | 2026-09-27 | jason | **D-1…D-7 全部选 (a)**：配置 IDORIS_URL 后独占（不留直连 oMLX 的通道）；等 M4 前置完成再接入；Evolver 主数据源是 Agent24 事件日志；敏感确认只在 iDoris 控制台完成；Desktop main 进程直连 Admin 端口；每个实例两把 key；原型阶段只承诺 M4 状态卡片。同时同意 Agent24 那约 20 行文档修改 | 等 Agent24 回复「R2 无异议」后定稿 → 转 JSON Schema |
| R4 | 2026-09-27 | Agent24 | 「已读 §3.12，与我方 R1 一致，**R2 无异议**」。Agent24 开始修订自己的约 20 行文档：L3 改为 ATIF v1.8 交换格式；ADR-032 §9 标为过期；引用本规范。见 [Agent24 PR #540](https://github.com/iDoris-ai/Agent24/pull/540) | **定稿 v1.0** |
| R5 | 2026-09-27 | iDoris | v1.0.1 措辞澄清（T4.1 验收时发现）：Served-Locality 只在已选定后端时才带；它表示推理实际发生的位置；缓存命中带 Cached 和 Origin-Record-Id。都是加法兼容 | **Agent24 无异议**：接入时按 Cached + Origin-Record-Id 去重记账，这条写进它们的 P4 设计 |
| R6 | 2026-09-27 | iDoris | jason 提出，iDoris 提议：请求阶段职责划分 S0–S7；统一凭证模块 vault | Agent24 方向同意，提出措辞修订与五个条件 |
| R7 | 2026-09-27 | 双方 | iDoris 采纳全部措辞修订与条件，并对 J-4、J-2 各加一处限定；Agent24 确认；jason 拍板 J-1…J-8 全部选 (a) | **v1.1** |
| R8 | 2026-09-27 | Agent24 | 审阅 v1.1：无异议；建议 loopback 免 key 仅作为显式开发者选项，免 key 时视为 local_only | 已采纳，写入 §3.2 |
| R9 | 2026-10-15 | iDoris | [文档修订 1.2 整合与三个 owner 决策](negotiation/R9-v1.2-协商提案.md)；local_only v0.3.1 最小数据面与 launcher 解耦 | OD-1/OD-2 已由修订 1.3 决定（见 §2.7）；fast/daily 标签重标仍 **OWNER REVIEW PENDING** |
| R10 | 2026-10-28 | jason / 双方 | 文档修订 1.3：owner 要求 Agent24 桌面端一站式「下载 → 运行 → 连接」，由此定下 OD-1（宿主为 agent24d）、OD-2（用户在桌面端同意）；新增 §2.7 launcher 托管契约（iDoris v0.4.0 起）、§2.8 wire 契约 1.1.0 | launcher 设计已合入 [Agent24#1437](https://github.com/iDoris-ai/Agent24/pull/1437)；按 v0.4.0 的实现与验收是 Agent24 的 IL-01 起各项，尚未完成。其中「额度紧时 503 `headroom_confirmation_required` → 用户确认 → 重发」的确认交互登记为 ID-HR，见 [Agent24#1442](https://github.com/iDoris-ai/Agent24/pull/1442) |
| R11 | 2026-11-15 | jason / 双方 | 文档修订 1.4：新增 §2.9 本机 chat 模型推荐（`GET /v1/recommendation?role=chat`、`idoris recommend --role chat --json`）、本地模型类别（`GET /v1/models/local`、`kind`）、下载归 iDoris 且须用户确认、oMLX 密钥读取顺序；档位 16/24/32GB → Qwen3-8B-4bit，48GB+ → Qwen3.8-27B-OptiQ-4bit，64GB+ 可选 Qwen3.6-35B-A3B-OptiQ-4bit | iDoris 与 Agent24 会话各确认一次（2026-11-15）；iDoris 侧推荐、列表与密钥顺序已在 v0.5.0 实现（#604、#605，文档修订 1.5）；下载未实现；Agent24 先做不依赖 iDoris 的部分 |
| R12 | 2026-11-20 | iDoris / Agent24 | 文档修订 1.6（wire 1.3.0）：IDORIS-CORE（[Agent24#1937](https://github.com/iDoris-ai/Agent24/pull/1937)）。iDoris 提案见设计稿（[PR #614](https://github.com/iDoris-ai/iDoris/pull/614)）；新增 §2.10 登记协议 v1、§2.11 wire 契约 1.3.0、§2.12 多引擎（预留），增补 §2.1/2.2/2.3/2.5/2.7/2.8/2.9/4.2。Agent24 2026-11-20 对设计稿 §3.3 七个问题的答复：<br>• Q1：登记方允许清单含 identifier `ai.idoris.cli`（Team `HYAP444FN9`）；只有 agent24d 本身是非打包的开发构建、且显式设置 `AGENT24_DEV_ALLOW_UNSIGNED_IDORIS=1` 时才接受未签名登记方，打包版一律拒绝。<br>• Q2：发现文件的时间字段用 `started_at_ms`（毫秒整数），不用 Agent24 方案里的 `started_at`。<br>• Q3：Electron main 不持有 Admin token，桌面端所有 Admin 操作走 agent24d 的 `/api/v1/idoris/*`，`set_admin_token` 轮换只是 agent24d 内部的事。<br>• Q4：两级就绪：`status=="ok"` 且 Admin 可答为 iDoris 可用（Agent24 发布网关槽位），`phase=="ready"` 为模型可用，`starting` 最长 90 秒；Agent24 的 chat 在 `phase!="ready"` 时有界等待，上限 120 秒，超时 503 加 `retry_after`（Agent24 随后细化为只在 `starting` 或有上游 `restarting` 时才等待，见下方「已确认项」第 1 条）。<br>• Q5：退出码 4（孤儿超时）不计入 Agent24 崩溃熔断，5（自有引擎无法恢复）计入。<br>• Q6：embeddings 请求发 `model:"idoris/embed"`，数据 key 的角色为 `daily`、`deep`、`embed`（最小权限，不发全角色 key）；v0.5.0 的过渡写法见 §2.11.3，v0.6.0 用伴生 resident 卡，chat 保持 `on_demand`。<br>• Q7：Agent24 删除 `omlx_probe`，候选与配置全部改为调用 `idoris setup omlx --auto`（原答复里「或 iDoris 提供的候选接口」已由下方「已确认项」第 2 条确认为不做候选接口），之后 Agent24 源码不再出现任何 oMLX 地址。<br>另已商定：nonce 由 iDoris 生成，写进 0600 发现文件，登记用 HMAC 证明，nonce 不上线；`/health` 只返回公开的 `instance_id`；Agent24 的核对顺序是先 `LOCAL_PEERTOKEN` 与端口属主，再 `/health.instance_id`，最后才发数据 key，`/health` 不带 key；孤儿超时 `orphan_timeout_s` 由 launcher 给（Agent24 取 900）；Admin 新增受限写接口 `upstreams/{id}/start\|stop\|restart`；引擎在 v0.6.0 由用户自装，iDoris 报 `engine_not_found`。<br>owner 2026-11-20：OD-2（安装 iDoris 须用户在桌面端同意）被取代，iDoris 随 Agent24 自动安装、不弹同意（§6）。<br>owner 2026-11-20（追加）：多引擎需求。iDoris 以后要支持多个引擎（oMLX、LM Studio、Ollama、llama.cpp、mlx_lm 等），引擎之间可以切换，默认引擎可以指定、可以修改；v0.6.0 只实现 oMLX，预留字段与契约见 §2.12（`engine_kind` 进入 wire 1.3.0，其余预留），实现另排。<br>Agent24 承诺（自身实现的注意事项，不改规范正文）：agent24d 拉起 iDoris 时用 `setsid` 放进独立会话，这样 launchd 回收 agent24d 时不会连带杀掉 iDoris，接管孤儿才能成立；核对端口属主用 `proc_pidfdinfo`（libproc），不用 `lsof` 子进程。<br>**Agent24 的保证（冻结时汇总；均已在上列答复与正文对应处出现，此处集中列出）**：（1）登记方只认 `LOCAL_PEERTOKEN` 加 codesign（identifier `ai.idoris.cli`，Team `HYAP444FN9`），不看 pid；核对顺序是端口属主、`/health.instance_id`，最后才发数据 key，`/health` 永不带 key，打包版一律拒绝未签名登记方（Q1、§2.10.9）；（2）桌面端的 Admin 操作都经 agent24d，Electron main 不持有 Admin token，`set_admin_token` 只在 agent24d 内部使用（Q3）；（3）chat 只在 `starting` 或有上游 `restarting` 时有界等待，上限 120 秒，终态原因立即 503 并带 `reason`（Q4、已确认项 1）；（4）退出码 4 不计入崩溃熔断，5 计入（Q5）；（5）embeddings 只发 `model:"idoris/embed"`，数据 key 按最小权限只含 `daily`、`deep`、`embed`（Q6）；（6）删除 `omlx_probe`，之后 Agent24 源码不再出现任何 oMLX 地址，所有模型调用只经 iDoris（Q7）；（7）不做候选列表接口，v0.6.0 不随包附带 oMLX.app（已确认项 2、3）；（8）`instance_conflict` 的判定与单实例会话规则（已确认项 4）；（9）iDoris 的 `setup upstream` 最小版交付后，Agent24 用它生成 LM Studio 等自定义上游卡（§2.7.8）。 | **已冻结（2026-11-21）：Agent24 已评审并同意（#615）。** 上列 Q1–Q7 与下方「已确认项」都是 Agent24 的答复；评审之后追加的内容（§2.12 多引擎预留、`instance_conflict` 的 iDoris 侧处理、`setup upstream` 最小版）Agent24 一并同意。冻结的是契约，不是 iDoris 已实现的功能；§2.12 的预留项另待后续修订冻结。冻结时 iDoris 另补：§2.10.10 的计时与边界口径、自有 base 与 `base_unavailable`（§2.7.6）、发现文件基于 `parent_fd` 的原子写与私有 key 的同 UID 边界（§2.7.3、§2.7.7）、`phase` 以门控完成为前提（§2.11.1）。Agent24 方案里的「§2.7 修订 1.4」编号改记为修订 1.6 |

**R12 已确认项（Agent24 2026-11-20；原为待确认项，已回填进正文对应位置）**：

1. 已确认（Agent24 2026-11-20）：chat 只有在 `phase=="starting"`，或有上游处于 `restarting` 时才有界等待，上限 120 秒。`phase=="degraded"` 且所有上游的 `reason` 都是终态原因（`engine_not_found`、`auth_required`、`auth_failed`、`budget_zero`）时，立即返回 503 并带上 `reason`，界面按 `reason` 引导（例如 `engine_not_found` 引导安装引擎）。正文见 §2.7.4。
2. 已确认（Agent24 2026-11-20）：不做候选列表接口。`idoris setup omlx --auto` 按 8088、8000、8001 的顺序取第一个可达的，桌面端只显示结果（`mode`、`endpoint`、`engine.installed`），§2.7.8 不再补字段。正文见 §2.7.8。
3. 已确认（Agent24 2026-11-20）：v0.6.0 不加 `--omlx-executable`（不随包附带 oMLX.app）。以后要随包时另行修订，届时与帧字段对称地加上；规范里写作「不承诺 / 以后修订」，见 §2.7.8、§2.7.9。
4. 已确认（Agent24 2026-11-20）：`instance_conflict` 的定义是，agent24d 已有一个 `ack` 过、心跳正常的会话（`instance_id`=A），并且 A 的 `/health` 仍以 `instance_id`=A 应答，这时 `instance_id`=B 来登记，就回 `instance_conflict`。也就是两个活实例同时出现：拒绝后来者，不顶掉现有会话。A 已经失联（心跳超时，或 `/health` 不通、`instance_id` 不符）时，B 正常登记，A 视为陈旧。删除残留的发现文件时，同样持有旁路锁 `<发现文件>.lock` 并复核 `instance_id`。正文见 §2.10.4、§2.10.9、§2.7.3。**iDoris 侧口径（iDoris 补，Agent24 已同意冻结）**：收到 `instance_conflict` 后优雅停止并以退出码 3 退出，不继续服务、不重试，理由见 §2.10.4。
