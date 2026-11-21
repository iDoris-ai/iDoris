//! idoris.ai/api 计算服务 **S4：云供应商契约**（路由层）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §6（请求状态机与失败切换）、§8（端点级许可表）、
//! §12 的 S4 一行。这一片决定「钱算没算对、会不会重复扣费、儿童数据会不会被送到不合规的上游」，
//! 所以**每一条规则都由 S0 契约裁决**（`idoris-compute-contracts`），这里只负责用契约的类型和纯函数驱动真实的上游调用：
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`executor`] | 执行器：候选选择（`select_compliant`）→ 准入 → 尝试（先落库再写字节）→ 首字节前切换 → 发布边界 → 结算 |
//! | [`answer`] | 两种结果：等待发布的完整回答（非流式）、一条流；发布边界与客户端断开时的收尾 |
//! | [`admission`] | 候选准入的接口（S3 的接缝）与 S4 的简单实现（只算预留额，一律放行，TODO(S3)） |
//! | [`config`] | 云上游配置（YAML）：地址、密钥来源、价目、路由、静态调用方 |
//! | [`clock`] | 时钟（许可表的核验过期按「现在」判，测试要能拨） |
//! | [`ingress`] | **S5：云入口**——`/api/v1`、从凭证推导调用方、三级上限、公网防滥用、CORS、公网监听 |
//! | [`safety`] | **S6 儿童安全**：租户属性绑定的安全管线、规则 / LLM 检查器、成本接口、进程级「不落盘」加固 |
//!
//! 协议与传输（OpenAI 兼容的请求、SSE、usage 解析、超时、密钥不外泄）在 `idoris-upstream::cloud`；
//! 尝试记录与结算分录（SQLite，只追加，触发器兜底）在 `idoris-tenancy::attempts`。
//!
//! # 默认关闭
//!
//! 启动入口是 [`CloudUpstream::from_env_for_form`]：云形态（`IDORIS_FORM=cloud`）没设
//! [`config::CONFIG_ENV`]（`IDORIS_CLOUD_UPSTREAM_CONFIG`）是启动错误，不是 `None`；非云形态设了这个变量也是启动错误。
//! 非云且没设才是 `AppState::cloud == None`，`build_app` 挂的还是原来的处理函数，
//! **现有的本地 oMLX 路径、订阅中转和全部既有 HTTP 行为一字不变**。
//! 旧入口 [`CloudUpstream::from_env`] 仍只看 `deploy_mode=tenant`（设了且是 tenant 才启用），启动不用它。
//! 个人形态永远不会把请求送到云上游
//! （「个人 iDoris 不改成 SaaS」，设计稿 §1 决定 4；`local_only` 失败即关闭）。
//!
//! # 交接给 S1 / S3 / S5 / S6
//!
//! - **S3（已完成）**：[`admission::LedgerAdmission`] 包住账户存储里的准入账本（`idoris_tenancy::accounts::ledger`：
//!   五项准入、有担保的负值、宽限、免费额度），生产装配（[`CloudUpstream::from_file`]）已经换掉了
//!   [`admission::SimpleAdmission`]（它现在只是 S4 的测试替身）。尝试记录的收尾和账本结算在同一个事务里
//!   （`AttemptStore::install_ledger_hook`）；候选没被采用时执行器会 [`admission::CandidateAdmission::release`]；
//!   体验分级的响应头见 `handler` 模块文档（`X-iDoris-Reason` / `X-iDoris-Tier` / `X-iDoris-Max-Tokens`，
//!   以及 [`EXPOSED_HEADERS`]，S5 的 CORS 层要用它）；
//! - **S1 / S2 / S5（已完成）**：付款上下文、租户、数据类别现在由 [`ingress`] 从凭证推导（`AccountStore::payment_context`，
//!   成员 id 逐次验证），处理函数读请求扩展里的 [`CallerPolicy`]；[`config::CallerBook`] 只剩测试夹具的用途
//!   （`CloudIngress::new` 拒绝非空的簿）。云入口（`/api/v1`）接在 `handler` 前面。云上游开关现在**要求**
//!   `IDORIS_FORM=cloud`（[`CloudUpstream::from_env_for_form`]）；
//! - **S6**（已完成）：非流式回答在发布前停在 [`answer::PendingAnswer`]，完整输出检查放在 `publish()` 之前，不通过就
//!   `withhold(..)`（用户付 0）；edu 一律非流式的拒绝在 `handler` 里（先于任何上游调用），执行器里再查一道。
//!   管线在 [`CloudExecutor::execute`]：处理儿童数据的租户（`TenantDataPolicy::handles_child_data()`）输入检查 -> 上游调用 ->
//!   完整输出检查，见 [`safety`] 的流程图。安全检查的成本走 S3 的账本：[`safety::LedgerExtraCost`] 实现了
//!   [`safety::ExtraCostMeter`]，生产装配（[`CloudUpstream::from_file`]）已经装上（输入检查、输出检查各预留一次，
//!   按检查端点的价目表折算成付款单位，按实际用量结算）；[`safety::SimpleExtraCost`] 只给没有账本的装配用。
//!
//! # 对设计稿 / S0 含糊处的理解（没有悄悄拍板，等 owner 确认）
//!
//! 1. **连接失败不回到 `not_sent`**：`reqwest` 的 `send` 把连接、写请求、等响应头合在一个 future 里，没法在「连接成功」
//!    与「开始写」之间落库。为了守住「先记 `WireWriteStarted` 再写字节」，`send` 一开始就按「字节可能已经出去」算：
//!    连接被拒 / DNS / TLS 失败的尝试保持 `maybe_executed`（成本按预留额挂账，对账查无此单后释放），失败标注是
//!    `connect_failed`。`never_left_host` 只用于 `prepare` 阶段的本地失败（取不到密钥、请求体不合法）。要做到精确，
//!    得给 `reqwest` 的连接器插一层（新增 `tower` 直接依赖并关掉连接复用）或给契约加「连接失败证据」事件——都没有做。
//! 2. **所有 4xx 都按契约当作「没执行」**（含 408、429；既有 `ChatProxy` 把 408 当作不确定）；**2xx 之后的任何失败**
//!    （响应体坏、流里报错、首个事件前断开）按「确认执行了、usage 未知」（`ExecutionObservedUsageUnknown`）。
//! 3. **哪些首字节前的失败会切换**：见 [`status_allows_failover`]。请求形状被拒的 4xx（400 / 413 / 422……）不切换：
//!    换一个同样合规的上游多半也被拒，而每多发一次，孩子的输入就多送给一个上游（数据最小化）。
//! 4. **错 usage = usage 未知**：结构不对、`total_tokens` 对不上、缓存大于输入、输出超过发给上游的 `max_tokens`、
//!    金额溢出，用户按预留价付费；上游声称的数字（只含数字字段）进 `usage_claim_json` 供对账。
//! 5. **客户端取消**：首字节前取消，尝试保持 `maybe_executed`；首字节后断开，看许可表里这个端点的「取消」能力：
//!    有 → 执行确认、usage 未知（按预留结算）；没有 → 上游可能还在计费，保持 `maybe_executed`。两种情况用户都按预留价付费
//!    （契约规则：已发布且 usage 未知）——产品上可能想按已转发量估算，没做。
//! 6. **供应商请求号**：正文里的 `id` 优先，其次是配置的响应头；格式不对只留标注；同一供应商下别的尝试用过同一个号、
//!    或流中途换了号：两条记录都保留并标注，不拒绝服务（丢掉审计痕迹比重复更糟）。
//! 7. **分录**：`user_price`（只有发布的那次）、`platform_cost`（按实际用量的成本）、`hold`（按预留额挂账待核销的成本，
//!    `maybe_executed` 与 usage 未知）、`release`（从用户额度释放的价预留）；零金额不记；
//!    一个请求至多一个付费尝试、至多一条正的 `user_price`（唯一索引）。
//! 8. **S0 `ReasonCode` 之外的稳定字符串**：请求记录的 `reason_code` 里还有 `upstream_unavailable`、`client_cancelled`、
//!    `upstream_stream_failed` 等；HTTP `error.type` 里还有 `upstream_error`、`caller_not_configured`、`cloud_unavailable`、
//!    `streaming_not_allowed`。S0 没有「所有上游都失败」这一类原因码。
//! 9. **edu 一律非流式**：S0 把它划给 S6，但漏掉它会让儿童数据绕过完整输出检查，所以 S4 在处理函数里就拒绝
//!    （先于任何上游调用，400 `streaming_not_allowed`）。
//! 10. **后一个候选被准入拒绝**：已经有尝试失败时，请求以上游失败（502）结束，不是 402。
//! 11. **付款上下文**：静态调用方簿经 S0 的 `resolve_payment_context`（平台 -> 组织账户、节点凭证）得到，不绕过契约的唯一入口；
//!     `QUOTA` 单位在云形态被拒绝。S5 落地后生产里换成账户库里按凭证解析的真实付款上下文（见 [`ingress`]）。
//! 12. **`Request` 的持久化**：创建时的快照 JSON（不可变）+ 规范化的查询列；`state_json` 是状态的真相，`state`、`certainty`、
//!     usage 是它的派生列（CHECK 与生成列）。
//!
//! # S6（儿童安全）对设计稿 / S0 含糊处的理解（没有悄悄拍板，等 owner 确认）
//!
//! 13. **「租户属性」就是 `TenantDataPolicy::handles_child_data()`**（`data_classes` 含 `child_edu`），来自服务端配置的调用方簿
//!     （S5 之后，生产里是云入口配置的 `tenants:`：租户从付款账户推导，见 [`ingress`]），经虚拟 key 绑定的租户解析；
//!     模型名、路由别名、任何请求头和请求体字段都改不了它。
//!     没有另设一个布尔字段：S0 已经有这个属性，并且它同时决定了端点许可表的硬约束（ZDR + 已签 DPA）。
//! 14. **管线放在执行器里，不是 handler 里**：任何入口（S5 的云入口、测试直接调执行器）走到执行器，edu 都绕不过。
//!     `RunState` 还有一道发布守卫：儿童请求的回答没有「输出检查放行」标记就发布不出去，标记在请求创建时由数据类别决定。
//! 15. **edu 的响应体是重建的，不是上游原样**：只含 `content` / `refusal` / `tool_calls` 和校验过的 `id` / `model` /
//!     `finish_reason` / 只含数字的 `usage`；上游多出来的文本字段（`reasoning_content`、`annotations`、`provider`……）不转发，
//!     因为没有检查过。非 edu 的响应体照旧原样。
//! 16. **输出被拒不换候选重试**：被拒的回答对应的尝试已经执行了（用户付 0，平台承担成本）；P0 不为了「换个模型再答一次」
//!     继续花钱，也不给孩子（或试探者）一个反复试探安全系统的循环。
//! 17. **检查结果和拒绝码**：`safety_input_blocked` / `safety_output_blocked` 用 HTTP 422，`safety_unavailable` 用 503
//!     （4xx 不会被 OpenAI 兼容客户端自动重试，503 才应该稍后重试）。设计稿只说「拒绝放行」，没有定状态码。
//! 18. **类别不落盘**：拦截类别（自伤、成人内容……）是从孩子的话里推出来的敏感推断，只在当次响应里给 eduai；
//!     请求记录里只有粗的码（`safety_input_blocked`），日志里也没有类别（PDPA 第 26 条把健康等列为敏感数据，设计稿 §7 要求最小化）。
//! 19. **客户端写的 `system` / `developer` 消息按孩子的话检查**（Codex r3 High#1、PR-Daemon B2 之后）：角色是客户端填的，不证明
//!     来自运营方。只有服务端配置 `safety.trusted_operator_prompts` 里登记的提示，逐字、只认消息开头，才按运营方内容对待（只查
//!     越狱类）；登记的提示加载时要先过越狱类规则。见 `safety/extract.rs` 的 `origin_of` 与 `safety/config.rs`。
//! 20. **儿童租户默认必须有 LLM 检查器**（owner 2026-11-13 方案 B）：有 `child_edu` 时，`rules_only` 不能启动，除非
//!     `safety.rules_only_acknowledgement` 写了原因、确认人和不超过 90 天（UTC）的到期日。没有儿童租户时 `rules_only` 照旧。
//!     显式放宽生效时打一行醒目警告，启动摘要写成 `rules_only (acknowledged until YYYY-MM-DD)`。**到期日在运行期间同样生效**：
//!     确认随 `SafetySetup::into_gate` 带进 gate，每次儿童请求（输入检查、输出检查）按执行器当次的时钟再核对，到期（UTC 日历日，
//!     到期日当天含）后没有 LLM 检查器的儿童请求不再放行，在上游调用和检查器的完整检查之前被拒：免费的规则预检先跑，命中是
//!     422（带类别，自伤是 `self_harm`），其余是 503 `safety_unavailable`（`cause: no_compliant_checker`）；没有新增线上字符串，选这个已有的 cause 是因为它的含义就是「没有满足硬约束的检查器」（需 owner 确认）。
//! 21. **安全检查的成本走 S3 的账本**（[`safety::LedgerExtraCost`]，生产装配 [`CloudUpstream::from_file`] 装上）：输入检查在任何
//!     尝试之前、输出检查在回答出来之后，各用 `reserve_request_component` 预留一次（component `safety_input` / `safety_output`，
//!     都挂在这个请求的 `#0` 名下，不依赖作答的是哪一次尝试），检查完用 `settle_component` 按实际用量结算。预留额按检查器**最终要
//!     发出的全部调用**算（先生成 payload 和分块计划，按计划预留，发送的是同一份计划），不是按原始文本的字节估一次。预留被账本拒绝
//!     （余额、速率、在途、补贴池、凭证状态）= 失败即关闭：不检查、不出网，返回账本的准入原因码（402 / 429 / 403）。账本出错、
//!     同一个 (请求, 阶段) 预留两次（`HoldExists`）同样失败即关闭（`safety_unavailable` / `internal`），不复用已有的预留。
//!     结算失败只写日志（预留有 TTL 兜底）。没有账本的装配（测试）用内存里的 [`safety::SimpleExtraCost`]。
//! 22. **非文本内容、过长文本一律拒绝**：图片、音频、文件等无法检查的内容按 `unsupported_content` 拒绝；超过检查上限的文本按
//!     `safety_unavailable`（`too_large`）拒绝。语音转写的检查是 P1。
//! 23. **旧路由对儿童租户关闭**：`build_app`（本地流水线加云处理函数）里，`/v1/messages`、`/v1/embeddings`、`/v1/rerank`
//!     仍是旧的本地流水线，没有安全管线，对儿童租户一律 403（`child_route_unavailable`）；`ChatProxy` 对这些租户同时关闭响应缓存。
//!     S5 的云入口（`IDORIS_FORM=cloud` 的生产装配）的数据面根本不挂载它们（`/v1/*` 的任何路由都不挂载，`/api/v1` 下也没有
//!     同名路径）：404，请求连认证都过不了，比 403 更严（见 [`ingress`]）。
//! 24. **谁为安全检查付钱**（设计稿 §7 只说「计入准入和结算」）：给出了结论的检查（放行或拦截）按实际用量向付款账户收费，
//!     用量未知按预留额（和 S4 对「用量未知」的规则一样）；规则检查器就拦下的（没有调用 LLM）、没能给出结论的（超时、出错、
//!     返回垃圾、客户端断开）收 0，由平台承担——故障的安全服务不该向孩子的账户收钱。备选：只为交付了回答的请求收检查费；
//!     或者检查费一律记给平台。**平台一侧的供应商成本（USD）目前没有记到任何地方**：S3 的组件接口只记付款账户一侧的额度，
//!     S4 的尝试记录里输入检查没有尝试可挂，要记就得给尝试记录库加分录种类。
//! 25. **检查成本折算成付款单位**：检查器给的是上游成本（`Cost`，供应商币种），账本要付款单位的价（`Price`）。用检查端点配置里的
//!     `prices`（和聊天端点同一份）按检查器给出的 token 上界折算（`amount_for_tokens`，向上取整），结算用同一张价目表和实际
//!     `Usage`，**封顶在预留额**（检查模型的 tokenizer 和字节估算对不上时，超出预留的部分由平台承担，免得账本的 overage 规则
//!     冻结孩子的凭证）。LLM 检查端点对每个儿童调用方的付款单位都要有价目，否则配置加载失败。
//! 26. **免费的规则检查先于预留**（PR-Daemon S5 F4 之后）：规则检查器先跑，命中直接 422（含 `self_harm` 类别，eduai 的升级信号
//!     不丢），不预留、不收费；规则放行后才按检查计划预留 LLM 检查费用。所以额度耗尽的孩子写出明显有害的输入仍得到 422 而不是 402。
//!
//! # Codex 审查 r2 与 PR-Daemon 评审之后的口径（需要 owner 确认，没有悄悄拍板）
//!
//! 27. **显式 `local_only` 在云入口一律拒绝**（403 `local_only_refused`，早于一切候选选择、准入、安全检查模型调用和上游请求；
//!     入口只校验声明是否合法，拒绝 `local_only` 的是执行器 `CloudExecutor::execute` 的最开头，这是唯一的一道闸）。**没声明不等于 `local_only`**：本地路由器「没声明 = `local_only`」的保守默认在云入口不适用，
//!     因为普通的 OpenAI 客户端不会发这个头，而它们调的就是云。头不合法（重复、空、不认识的值）、请求体里写了 `privacy` / `metadata.privacy` 却不是 `any` / `local_only` 都是 400 `invalid_privacy`，
//!     不当作没声明。请求体里的等价声明（顶层 `privacy` 或 `metadata.privacy` 写了 `local_only`）同样拒绝，取最严的。
//! 28. **`billing: unbilled` 没有实现，加载配置时拒绝**（备选：完整实现「不收用户费用、单独记平台成本」，要改准入、预留、结算）。
//!     契约（S0）里 `Billing::Unbilled` 对流式 usage 的豁免没有改动。
//! 29. **`extra_body` 不能碰协议 / 计费 / 身份 / 留存字段**（`idoris_upstream::cloud::RESERVED_EXTRA_BODY_FIELDS`），加载时拒绝，
//!     构造请求时这些字段最后写入。另外是**白名单**（issue #603 N6）：顶层只收 `provider`，里面只收布尔值的 `zdr`、`allow_fallbacks`，
//!     其余加载时拒绝，构造请求时按同一张白名单再过滤一遍。保留表与白名单是我拟的，增减要 owner 看一眼。
//! 30. **流里的 usage 什么时候可信**（`answer::StreamTracker::usage_is_trusted`，owner 口径：**不多收也不少收**）：分开跟踪最近的
//!     usage、最后一次「新内容」出现的位置、每个 choice 的结束位置（非空**字符串**的 `finish_reason`）。usage 之后**没有新内容**
//!     （只有空 delta 的结束块、`[DONE]`）且所有 choice 都已结束（或见到 `[DONE]`）才可信。**单独的 usage 块（`choices` 为空）自己不算结束的迹象**（issue #603 N1）：
//!     OpenAI 规范流里它跟在带 `finish_reason` 的块后面，照旧按实际用量；没有任何结束标志的单独 usage 块不可信。**结束标志不是永久的**（第六轮）：一个 choice 的结束只在它不早于这个 choice 最近一次新内容时才算数，
//!     已经结束的 choice 又出现新内容，旧的结束标志作废，直到它再次结束（原来是个永不清除的布尔：内容 -> finish -> 新内容 + 累计 usage ->
//!     错误 / 断开，累计数字被当成最终用量结清、不挂账）。选「作废」而不是「把流判成畸形」：两者对这个场景的结算相同（预留价），
//!     作废还保留上游声称的累计数字供对账，也不掐断用户已经在收的回答。每个 choice 按 `index` 单独看，所有出现过的 choice 都结束了才算；
//!     我们从不请求 `n > 1`，多个 choice 是不守约定的上游，最多跟踪 8 个，超出的保守地当作没有结束。
//!     usage 之后**有新内容**就不可信，**即使最后是 `[DONE]` 也不可信**（中途的数字只是已生成部分）。不可信时按「用量未知 / 可能已
//!     执行」的原规则（预留价、挂账待核销），上游声称的数字记进 `usage_claim_json`（`untrusted:not_final`）供对账。**不看流是怎么
//!     结束的**：最终 usage 之后客户端读到结束标志就断开，照样按实际用量结算（原来把无内容的结束块当作「usage 之后又有内容」，
//!     本该按实际用量收的变成按整笔预留收）。`content: ""`、只带 `role`、`null`、空数组、空 delta 的块都不算新内容；`delta` 里
//!     不认识的字段有文字、choice 自己的 `text` 有文字（issue #603 N1），都当内容（误判的方向是不信 usage，不是少收）。
//!     单独的 usage 块之后客户端立刻断开：前面已有结束标志就按实际用量，没有就按预留价（issue #603 N1）。
//!     **空 choice 不是合法的 completion chunk**（`{"choices":[{}]}`）：一个 choice 必须带 `delta` 对象或已结束的 `finish_reason`，
//!     否则发布之前按「流不合法」失败、允许切换候选，不转发、不收费。**choice 的字段类型也要对**（第六轮）：`finish_reason` 只能是
//!     缺省 / `null` / 字符串（布尔值、数字、对象、数组既不算结束，也让整个 chunk 不合法：原来 `{"choices":[{"finish_reason":false}]}`
//!     被当成合法回答转发、跨过发布边界），`delta` 只能是缺省 / `null` / 对象，`index` 只能是缺省 / `null` / 非负整数；
//!     顶层 `choices` 只能是缺省 / `null` / 数组（带着有效 usage 也一样，不再被当成「没有 choices 的单独 usage 块」转发）。
//! 31. **儿童判定看整条付款链**：顶层客户之外的任何账户声明了 `child_edu`，这个请求就按儿童处理（数据类别并集、留存取更严、
//!     区域取交集）；声明写在已经存在的非顶层账户上，启动失败。**儿童租户必须配 LLM 检查器**（生产启动，`rules_only` 失败）：
//!     规则检查器靠关键词和正则，换字体、拼写变形、base64、藏在工具描述里的内容都能绕过。
//! 32. **认证之前有界**：同时在途的认证查询个数 + 「最近没有认证成功过的凭证前缀」的查询预算（默认 64 个在途、每分钟 1200 次，
//!     突发 200），满了直接 429 `auth_capacity_limited`，不碰库；在用的凭证不消耗预算。默认值是我拟的，要按真实流量调。
//!     每个来源另有自己的份额（默认每分钟 120 次、突发 20），**先于**全局预算扣：一个来源刷随机 key 只能耗掉它自己那一份，
//!     耗不光全局预算，别的来源冷的合法凭证照常通过（#603 N4，设计稿 S5-15）。「最近认证成功过的前缀」表按最近使用淘汰（LRU）。
//!     在途名额**跟着真正执行认证查询和其后调用方解析的阻塞任务走**（同一个 `AuthSlot`，两段都结束才还）：请求被取消（客户端断开）
//!     名额不提前归还，所以「发起再取消」攒不出超过上限的阻塞任务。令牌桶里的时间永远只前进（落后多少的读数都不拨回它）。
//! 33. **供应商主机绑定**：云上游端点的 `base_url` 必须是回环地址，或者这个供应商核验过的 API 主机（`config::VERIFIED_PROVIDER_HOSTS`，
//!     来自 `config/compute/endpoint-urls.md`）；主机没核实的供应商不能配置到非回环地址；**没有登记主机的供应商（许可表的新行、
//!     `example-*`）默认拒绝非回环地址**（原来不受限：新增一条允许儿童数据的已审核行之后，它的 `base_url` 能误指任意 HTTPS 主机）。
//!     更彻底的做法（把主机写进许可表的 schema，由 S0 校验、路由时一起过滤）要改契约，没做（需要 owner 确认）。儿童安全的 LLM 检查端点
//!     在 `safety/config.rs` 的 `build_judge` 里调用同一个 `check_provider_host`，不依赖 `endpoints:` 逐行校验先跑过。
//! 34. **已发送的尝试报 0 用量按 0 结算**（用户付 0、平台成本 0）。真实分词器不会给非空提示算出 0 个输入 token，这多半说明上游出错了：
//!     是否按最小值收、或按「用量未知」处理，要 owner 定；`cloud_r2_fixes` 里有一条测试钉住今天的行为。
//! 35. **迟到 usage 对账**（`AttemptStore::reconcile_late_usage`）只处理没有向用户收费的 `maybe_executed` 尝试：冲销旧挂账、记实际成本、
//!     按 S0 的转移结清，一个事务。向用户收费那一次的迟到 usage 要不要退差价是准入账本的事，这里拒绝。
//! 36. **同步的账本操作用 `block_in_place` 跑**（`blocking`）：准入、尝试记录、收尾、拒绝记录、供应商请求号落盘
//!     （`RunState::record_supplier_id` 里的两处同步写），以及儿童安全检查的成本计量
//!     （`LedgerExtraCost` 的预留和结算）不再直接阻塞 tokio worker 线程。
//!     账户存储里的其它同步调用不在这里。

use std::path::Path;
use std::sync::Arc;

use idoris_compute_contracts::ids::TimestampMs;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::attempts::AttemptStore;

pub mod admission;
pub mod answer;
mod blocking;
pub mod clock;
pub mod config;
pub mod executor;
mod handler;
pub mod ingress;
mod request;
mod run;
pub mod safety;

pub use admission::{
    AdmissionError, AdmissionRequest, CandidateAdmission, LedgerAdmission, ReleaseRequest,
    SimpleAdmission,
};
pub use answer::{AnswerMeta, CloudOutcome, CloudStream, PendingAnswer, PublishedAnswer};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{
    CallerBook, CallerPolicy, CloudLimits, CloudParts, EndpointRegistry, EndpointRuntime, RouteBook,
};
pub use executor::{
    CallPrivacy, CloudCall, CloudError, CloudExecutor, FailureInfo, status_allows_failover,
};
pub use ingress::{CloudIngress, IngressContext, build_cloud_app};
pub use run::Settled;
pub use safety::{
    ExtraCostMeter, GateDenial, SafetyChecker, SafetyGate, SafetyLimits, SafetyPipeline,
    SimpleExtraCost, Verdict,
};

pub use handler::{EXPOSED_HEADERS, HEADER_MAX_TOKENS, HEADER_TIER};
pub(crate) use handler::{chat_completions, child_route_guard};

/// 云上游：执行器 + 路由簿 + 调用方簿。挂在 `AppState.cloud` 上。
pub struct CloudUpstream {
    pub(crate) executor: CloudExecutor,
    pub(crate) callers: CallerBook,
    pub(crate) routes: RouteBook,
}

impl std::fmt::Debug for CloudUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudUpstream")
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl CloudUpstream {
    /// 用加载好的部件、尝试记录库、时钟和准入实现组装。
    pub fn new(
        parts: CloudParts,
        store: Arc<AttemptStore>,
        clock: Arc<dyn Clock>,
        admission: Arc<dyn CandidateAdmission>,
    ) -> Self {
        let CloudParts {
            permits,
            endpoints,
            routes,
            callers,
            limits,
            reserve_policy,
            safety,
        } = parts;
        let mut executor = CloudExecutor::new(
            Arc::new(permits),
            Arc::new(endpoints),
            admission,
            store,
            clock,
            reserve_policy,
            limits,
        );
        if let Some(setup) = safety {
            // 没有账本的装配（测试、嵌入式）用内存里的简单计量器；生产装配（`from_file`）随后换成读写账本的
            // `LedgerExtraCost`（见 `with_ledger_extra_cost`）
            executor =
                executor.with_safety(Arc::new(setup.into_gate(Arc::new(SimpleExtraCost::new()))));
        }
        Self {
            executor,
            callers,
            routes,
        }
    }

    /// 用给定的儿童安全 gate 换掉配置里的（测试和装配用）。
    pub fn with_safety(mut self, gate: SafetyGate) -> Self {
        self.executor = self.executor.with_safety(Arc::new(gate));
        self
    }

    /// 换掉安全检查成本的计量器（测试用它观察预留与结算，或模拟额度被拒）。没有装安全 gate 时什么都不做。
    pub fn with_extra_cost_meter(mut self, meter: Arc<dyn ExtraCostMeter>) -> Self {
        if let Some(gate) = self.executor.safety.take() {
            self.executor.safety = Some(Arc::new(gate.as_ref().clone().with_cost(meter)));
        }
        self
    }

    /// 安全检查的成本接上账本（S3）：输入检查在任何尝试之前、输出检查在回答出来之后各预留一次，按检查端点的价目表折算成
    /// 付款单位，按实际用量结算（见 [`safety::LedgerExtraCost`]）。生产装配（[`from_file`](Self::from_file)）用它。
    /// 没有装安全 gate 时什么都不做。
    pub fn with_ledger_extra_cost(self, accounts: Arc<AccountStore>) -> Self {
        let meter = safety::LedgerExtraCost::new(
            accounts,
            self.executor.endpoints.clone(),
            self.executor.clock.clone(),
        );
        self.with_extra_cost_meter(Arc::new(meter))
    }

    /// 配置里有没有处理儿童数据的调用方。
    pub fn has_child_data_callers(&self) -> bool {
        self.callers.has_child_data_callers()
    }

    /// 处理儿童数据的租户标识（`ChatProxy` 缓存键里的租户范围格式），给旧代理路径关缓存用。
    pub fn child_data_tenant_scopes(&self) -> std::collections::BTreeSet<String> {
        self.callers.child_data_tenant_scopes()
    }

    /// 是否装了儿童安全 gate。
    pub fn has_safety_gate(&self) -> bool {
        self.executor.safety.is_some()
    }

    /// 从配置文件加载，用系统时钟和 **S3 的准入账本**（[`LedgerAdmission`]）；账户存储（含账本）和尝试记录库
    /// 都打开在 `db_path`（与其它 store 共用同一个 SQLite 文件）。
    ///
    /// 装配的三件事，缺一不可：
    ///
    /// 1. 准入走账本（沿付款账户所有祖先的五项原子检查，占预留）；
    /// 2. 尝试记录装上账本钩子：创建尝试要有匹配的预留，收尾时账本在**同一个事务里**结算；
    /// 3. 预留 TTL 必须长于一次请求可能持续的最长时间（总期限 + 流的总时长）：预留在上游还在计费时就过期，
    ///    额度会提前放回去。不满足就启动失败（失败即关闭），不是悄悄放行；
    /// 4. 儿童安全检查（S6）的成本走同一个账本（[`safety::LedgerExtraCost`]）：输入检查、输出检查各预留一次，按实际用量结算。
    ///
    /// 账户存储里没有的付款账户 / 凭证（比如还在用静态调用方簿 `callers:`，账户库里没有对应的账户）一律被准入拒绝：
    /// 账本不认识的付款人不能花钱。
    pub fn from_file(path: &Path, db_path: &Path) -> Result<Self, String> {
        Self::from_file_with_ingress_tenants(path, db_path, &[])
    }

    /// 同 [`from_file`](Self::from_file)，另外把云入口（S5）配置里声明的儿童租户算进 S6 的启动校验：它们不在静态调用方簿里，
    /// 但同样走儿童安全管线，所以有儿童租户就必须有 `safety:` 段、LLM 检查端点要满足它们每一个的数据策略。
    pub fn from_file_with_ingress_tenants(
        path: &Path,
        db_path: &Path,
        ingress_tenants: &[config::IngressTenant],
    ) -> Result<Self, String> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let now = clock.now();
        let parts = config::load_with_ingress_tenants(path, now, ingress_tenants)?;
        // 儿童数据默认必须有 LLM 安全检查器。rules_only 只有在显式确认仍有效时才能启动（owner 2026-11-13 方案 B）。
        config::require_llm_judge_for_child_tenants(&parts, ingress_tenants, now)?;
        // 启动日志（S6）：哪些调用方走儿童安全管线（只有租户标识）。运营方一眼能核对「edu 租户的属性确实是 child_edu」
        let (mut child, mut other) = parts.callers.caller_summary();
        for tenant in ingress_tenants {
            let listed = format!("{} (cloud ingress)", tenant.name);
            if tenant.policy.handles_child_data() {
                child.push(listed);
            } else {
                other.push(listed);
            }
        }
        eprintln!(
            "[idoris] cloud: child-data callers (safety pipeline ON): [{}]; other callers (no safety pipeline): [{}]; safety: {}",
            child.join(", "),
            other.join(", "),
            parts.safety.as_ref().map_or_else(
                || "not configured".to_string(),
                safety::config::SafetySetup::startup_label,
            )
        );
        if let Some(safety) = &parts.safety {
            for warning in &safety.warnings {
                eprintln!("[idoris] cloud: {warning}");
            }
        }
        let accounts = Arc::new(
            AccountStore::open(db_path)
                .map_err(|e| format!("无法打开账户数据库 \"{}\"：{e}", db_path.display()))?,
        );
        let store = AttemptStore::open(db_path)
            .map_err(|e| format!("无法打开尝试记录数据库 \"{}\"：{e}", db_path.display()))?;
        store
            .install_ledger_hook(accounts.attempt_ledger_hook())
            .map_err(|e| format!("无法安装准入账本钩子：{e}"))?;
        let ttl_ms = accounts
            .ledger_config()
            .map_err(|e| format!("无法读取准入账本配置：{e}"))?
            .reservation_ttl_ms;
        let longest = parts.limits.total_deadline + parts.limits.stream_total;
        if u128::try_from(ttl_ms).unwrap_or(0) <= longest.as_millis() {
            return Err(format!(
                "准入账本的预留 TTL（{ttl_ms} ms）必须长于一次请求可能持续的最长时间（总期限 + 流的总时长 = {} ms）：\
                 否则预留会在上游还在计费时过期，额度提前放回去",
                longest.as_millis()
            ));
        }
        // 回答的钱（准入、预留、结算）和安全检查的钱走同一个账本：缺了后者，LLM 检查器的成本就没有真正计入准入和结算
        Ok(Self::new(
            parts,
            Arc::new(store),
            clock,
            Arc::new(LedgerAdmission::new(accounts.clone())),
        )
        .with_ledger_extra_cost(accounts))
    }

    /// **启动用的入口**（S5）：云上游只属于云形态（`IDORIS_FORM=cloud`），反过来云形态必须配云上游。
    ///
    /// - 云形态：[`config::CONFIG_ENV`] 必须设置，没设就是启动错误（云形态没有本机模型，没有上游的公网入口什么都服务不了，
    ///   不如启动失败）；
    /// - 其它形态：设了 [`config::CONFIG_ENV`] 就是启动错误，没设就是关闭（什么都不变）。
    ///
    /// 为什么要求显式的 `IDORIS_FORM=cloud`，而不是 S4 的「`deploy_mode=tenant` 就行」：云上游的付款上下文必须由账户库里
    /// 的凭证解析，而账户库只在显式声明的组织、云形态才会打开；`deploy_mode=tenant` 还包括存量的 tenant 部署，
    /// 那里的租户来自可以被调用方自己写的 `X-iDoris-Tenant`（本地契约），不能拿来决定谁付款。组织形态是云的客户
    /// （S7 的云端档），不是云本身（能力表 `uses_endpoint_permit_table` 只有云形态为真）。
    ///
    /// `ingress_tenants` 是云入口配置里声明的租户和它们的数据处理要求（非云形态传空）：其中的儿童租户和静态调用方簿里的
    /// 儿童调用方一起过 S6 的启动校验，启动日志按「走 / 不走儿童管线」把它们列出来。
    pub fn from_env_for_form(
        startup: &crate::startup_form::StartupForm,
        ingress_tenants: &[config::IngressTenant],
    ) -> Result<Option<Arc<Self>>, String> {
        let path = crate::config::optional_env_path(config::CONFIG_ENV)?;
        Self::resolve_for_form(
            startup,
            path,
            crate::storage::process_db_path,
            ingress_tenants,
        )
    }

    /// [`Self::from_env_for_form`] 的可测版本：路径与数据库路径由调用方给。
    pub fn resolve_for_form(
        startup: &crate::startup_form::StartupForm,
        config_path: Option<std::path::PathBuf>,
        db_path: impl FnOnce() -> Result<std::path::PathBuf, String>,
        ingress_tenants: &[config::IngressTenant],
    ) -> Result<Option<Arc<Self>>, String> {
        if startup.is_cloud() {
            let Some(path) = config_path else {
                return Err(format!(
                    "云形态（IDORIS_FORM=cloud）必须设置 {}：没有云上游，公网入口什么都服务不了",
                    config::CONFIG_ENV
                ));
            };
            return Ok(Some(Arc::new(Self::from_file_with_ingress_tenants(
                &path,
                &db_path()?,
                ingress_tenants,
            )?)));
        }
        if config_path.is_some() {
            return Err(format!(
                "{} 只能在 IDORIS_FORM=cloud（云形态）下启用：云上游的付款上下文由账户库里的凭证解析，\
                 个人、组织形态和存量 tenant 部署不会把请求送到云上游",
                config::CONFIG_ENV
            ));
        }
        Ok(None)
    }

    /// 读环境变量 [`config::CONFIG_ENV`]：没设（或为空）就是关闭（返回 `None`，什么都不改变）。
    /// 设了就必须是云形态（`deploy_mode=tenant`），否则启动失败：个人形态不会把请求送到云上游。
    /// （S4 的入口，只看 `deploy_mode`；启动用 [`Self::from_env_for_form`]，它还要求 `IDORIS_FORM=cloud`。）
    pub fn from_env(
        deploy_mode: idoris_contracts::DeployMode,
    ) -> Result<Option<Arc<Self>>, String> {
        let path = crate::config::optional_env_path(config::CONFIG_ENV)?;
        Self::from_env_path(path, deploy_mode, crate::storage::process_db_path)
    }

    /// [`Self::from_env`] 的可测版本：配置路径与数据库路径由调用方给。
    /// 先查形态、后碰文件和数据库：个人形态下设了开关也不会去读任何东西。
    pub fn from_env_path(
        config_path: Option<std::path::PathBuf>,
        deploy_mode: idoris_contracts::DeployMode,
        db_path: impl FnOnce() -> Result<std::path::PathBuf, String>,
    ) -> Result<Option<Arc<Self>>, String> {
        let Some(path) = config_path else {
            return Ok(None);
        };
        if deploy_mode != idoris_contracts::DeployMode::Tenant {
            return Err(format!(
                "{} 只能在 IDORIS_DEPLOY_MODE=tenant（云形态）下启用：个人形态不会把请求送到云上游",
                config::CONFIG_ENV
            ));
        }
        Ok(Some(Arc::new(Self::from_file(&path, &db_path()?)?)))
    }

    /// 测试和装配用：直接取执行器。
    pub fn executor(&self) -> &CloudExecutor {
        &self.executor
    }

    /// 解析时用的「现在」：给静态付款上下文用（配置里的凭证永不过期，这里只是满足函数签名）。
    pub fn config_time() -> TimestampMs {
        SystemClock.now()
    }
}
