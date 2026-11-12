# 上游端点核验（S8 首版）

> 状态：核验记录，不是许可。路由是否放行以端点级许可表为准，而许可表的 schema 要等 S0 契约。本文件配套的草案在 `compute-service-endpoint-table.draft.yaml`。
>
> 查询日期：2026-11-12。下文每一条外部事实都写了来源 URL。页面上的「最后修改」日期与查询日期分开写。查不到、页面互相矛盾、或只读到二手转述的，写「未核实」。
>
> 本次没有登录任何供应商后台，没有调用付费 API，没有测量新加坡到上游的延迟。设计稿 §15 里的泰国 PDPA 条文、AI 法草案、SQLite 负载，不在这次范围内。

设计稿 §8（定稿 1.0）规定：`child_edu` 只用「零数据留存（ZDR）且已签数据处理协议（DPA）」的端点，候选是 DeepInfra 与 OpenRouter 的 ZDR 路由，不用 DeepSeek 官方；`adult` 可以再加 DeepSeek 官方。本次核验的结论是：**这两个候选今天都不能写入「已允许 child_edu」**。公开条款里的 ZDR 都有例外，DPA 都还没有以本项目的名义签署，OpenRouter 的自助条款还禁止为转售 API 访问而使用（企业协议另有明文的终端客户许可，见 §2 复核更正）。

## 怎么读置信度

| 置信度 | 含义 |
|---|---|
| 高 | 2026-11-12 打开的供应商自己的条款、隐私政策或文档，并在下面摘了原意 |
| 中 | 一手页面存在，但正文是后台才能确认的设置，或只覆盖供应商默认、不覆盖具体型号 |
| 低 | 来源互相矛盾，或只有路由商转述、没有打开该供应商自己的页面 |

二手站点（比价站、隐私简报）只用来找 URL，不写入事实。

## 1. DeepInfra

**结论（高，条款与文档；中，区域是否还会变）：** 自有页面写推理在美国数据中心。服务条款第 7(b) 条写了 Zero Data Retention，同时列出支持排障、法律与滥用、批量、付费缓存等例外。DPA 被条款并入，正文在 Trust Center，本次没有打开正文，本项目也没有签署。转售默认禁止，除非服务订单明文允许。没有查到专门允许或禁止儿童业务的条款。OpenAI 兼容接口文档支持工具调用、`stream_options.include_usage`、结构化输出和缓存计价。客户端断开后是否停算、是否停计费，DeepInfra 自己的文档没写，标未核实。

### 处理区域

- 首页写：「DeepInfra runs on our own cutting-edge inference optimised infrastructure, in secure US-based data centers.」查询 2026-11-12。https://deepinfra.com/
- 隐私政策（最后修改：2026-08-15）写：数据可能在你所在司法辖区之外传输、存储和处理，「specifically in the United States」。https://deepinfra.com/privacy
- Trust Center 的基础设施一项写的是 GCP、United States。FAQ 标题包括「Where is my data processed」，本次抓取没有展开答案。https://trust.deepinfra.com/
- 服务条款（最后修改：2026-10-15）第 6(b) 条保留供应商自行更换、迁移基础设施的权利，服务订单没有明文压过这一条时，区域不是合同锁定。https://deepinfra.com/terms
- 具体城市、美国以外是否已有节点、无服务器 API 能否钉住区域：**未核实**。Trust Center 与首页一个写 GCP、一个写自有数据中心，本次没有第三份材料把两者对齐。

### 留存

条款第 7(b) 条（https://deepinfra.com/terms ，最后修改 2026-10-15）写：不把客户数据留到「处理并返回该请求所严格必要的时间」之后，并称之为 Zero Data Retention。同一条写明这一点**不适用于**：

1. 客户书面要求或事先书面授权、用于排障的数据，问题解决后 30 天内删除；
2. 不含请求正文的运营元数据（次数、时间戳、延迟、账单）；
3. 法律要求，或为侦查欺诈、安全事件、滥用而必须保留的记录；
4. 客户主动使用的功能：批量请求、异步媒体输出、第三方集成、付费缓存保留，保留期以该功能文档为准。

同一条还写：由客户数据得到的 KV / 前缀缓存只在内存里临时保存，只为完成请求。ZDR 只覆盖 **Provider 自己托管的模型**；客户选用的第三方模型走第 18 条和该第三方条款。

文档与条款一致的部分（https://docs.deepinfra.com/account/data-privacy ，查询 2026-11-12）：

- 推理输入不落盘，只在内存里活到请求结束；输出返回后从内存删除。
- 不把提交的数据用于训练，也不分享给第三方。例外是 Google、Anthropic 模型：输出按这两家自己的政策处理，请求会被送到他们的端点。
- 不记录请求正文。日志只有请求 ID、费用、采样参数这类元数据。Google 模型另说：Google 会在有限期间记录 prompt 和响应，只为检测其使用政策违规。
- 图像生成输出会短期保存。批量接口可能加密落盘，返回后「short retention period」再删。这个「短」没有给出天数。

付费缓存保留（https://docs.deepinfra.com/chat/prompt-cache-retention ，查询 2026-11-12）是客户点选的留存：5 分钟写入价为输入价的 1.25 倍，1 小时为 2 倍。条款把它列在 ZDR 例外里。不点这个功能时，自动前缀缓存仍可能把 KV 放在内存里（https://docs.deepinfra.com/chat/prompt-caching ）。

因此许可表里的留存应写 **有例外**，不能写纯 ZDR。儿童业务若要用，至少要避开：Google / Anthropic 模型、图像输出、批量接口、付费缓存保留。滥用与法律例外是否会在没有个案时保存正文，文档说不记录正文，条款又保留了权利。**这两句没有在同一份文件里对齐，日常是否抽存正文：未核实。**

### DPA

条款第 5(a) 条写：Trust Center 上的 DPA 并入协议，适用于客户数据中的个人数据；与隐私政策或本条款冲突时，就该事项以 DPA 为准。第 7(b) 条的 ZDR 承诺高于隐私政策和其他网页，DPA 只能补充、不能收窄 ZDR。https://deepinfra.com/terms

Trust Center 把 Data Processing Agreement 列为可查看的政策，同时页面上有 Request access。https://trust.deepinfra.com/ 与 https://trust.deepinfra.com/resources

本次没有打开 DPA 正文（标准合同条款、子处理者、儿童数据、跨境机制都没读到）。本项目没有签署服务订单，也没有以点击方式接受。字段记 **未签**。GDPR 在合规页上是 “In progress”；SOC 2 与 ISO 27001 标为 Compliant；HIPAA 标为 Compliant。https://trust.deepinfra.com/compliance 这些标志不能代替已签 DPA。

### 转售

条款第 11(a) 条：不得以与 Provider 竞争的方式使用服务；不得转售、再许可、出租、分发或以其他方式把服务提供给第三方，**除非本协议或服务订单明文允许**；不得买卖或共享账号与凭证。https://deepinfra.com/terms

「下游教育产品调用我们的 API」是否算转售，条款没有定义。按字面，默认禁止向第三方提供服务。要当 iDoris 的上游，需要服务订单里的明文许可。这一点必须由人跟销售谈，不能从自助注册推断。

### 未成年人

在已读的条款章节里，没有看到年龄门槛、儿童数据禁令，或允许处理儿童数据的句子。检索词包括 minor、child、年龄相关表述；命中的是 “advantage” 一类无关词，以及高风险用途（生命支持、武器等）禁令。https://deepinfra.com/terms

隐私政策要求客户自行具备向 Provider 提供个人数据的合法基础（条款第 5(b) 条同旨）。**没有查到专门的儿童业务条款。未核实是否另有可接受使用政策。** 不能把「没写禁止」写成「允许 child_edu」。

### 能力（OpenAI 兼容）

基址：`https://api.deepinfra.com/v1/openai`。https://docs.deepinfra.com/api-reference/introduction （查询 2026-11-12）

| 能力 | 结论 | 出处 |
|---|---|---|
| 工具调用 | 文档有 OpenAI 兼容工具调用。支持单次、并行（质量可能波动）、`auto` / `none`、流式。嵌套调用不支持。是否每个型号都支持，以型号页徽章为准 | https://docs.deepinfra.com/chat/tool-calling |
| 流式 usage | Chat Completions 的 OpenAPI 含 `stream_options.include_usage` 与 `continuous_usage_stats`。流式文档写最后一块带 `prompt_tokens` 与 `completion_tokens` | https://docs.deepinfra.com/api-reference/chat-completions/openai-chat-completions 与 https://docs.deepinfra.com/chat/streaming |
| 结构化输出 | `json_object` 与 `json_schema`（可 `strict`） | https://docs.deepinfra.com/chat/structured-outputs |
| 缓存计价 | 自动前缀缓存，`usage.prompt_tokens_details.cached_tokens`。另有点选的 5 分钟 / 1 小时保留，写入另计 | https://docs.deepinfra.com/chat/prompt-caching 与 https://docs.deepinfra.com/chat/prompt-cache-retention |
| 取消 | 自己的流式文档没有写断开连接后停止推理或停止计费。OpenRouter 把 DeepInfra 列在「断开即停算停计费」的供应商里，那是路由商的名单，不是 DeepInfra 的承诺。**DeepInfra 一侧：未核实** | https://docs.deepinfra.com/chat/streaming ；对照 https://openrouter.ai/docs/api_reference/streaming |

型号页把 DeepSeek-V3.2 标成 Zero retention、JSON、Function（https://deepinfra.com/deepseek-ai/DeepSeek-V3.2 ，查询 2026-11-12）。这个徽章与条款第 7(b) 条的例外并存，徽章不是更窄的合同。

### 仍要人做的事

- 向 Trust Center 申请并读完 DPA 正文，再决定点击并入是否够用，还是要签字件。
- 谈服务订单：转售或向终端用户提供服务的许可、区域是否能写死、滥用抽存的范围。
- 在后台确认所选型号不是 Google / Anthropic 通道。
- 用律师看「有例外的 ZDR」是否满足设计稿对 child_edu 的定义。本次不把 DeepInfra 记为已允许。

## 2. OpenRouter

**结论（高）：** ZDR 是账户、护栏或单次请求上的路由开关，不是默认的全站行为。自助账户在未选择时允许 Safety Retention；企业账户默认 ZDR only。Safety Retention 可以对任意请求留存正文，只限防滥用、不用于训练，但它不是 ZDR。即便选了 ZDR only，CSAM 审查仍可能留存被标记内容。DPA 对商业使用经条款并入，签字件要向企业方案申请。服务条款禁止为转售 API 或做竞争服务而使用。账户持有人必须年满 18 岁。没有查到允许把儿童数据送到下游模型的条款。

### ZDR 怎么开、实际是什么

https://openrouter.ai/docs/guides/features/zdr （查询 2026-11-12）

- ZDR 的定义：处理之后不对 prompt 和响应做常规存储。同页写明 CSAM 例外。
- 三档：ZDR；Safety Retention（任意请求都可能被留存，不论有没有先被标记，只为发现、调查、阻止滥用，不用于训练）；Retention and Training。
- 强制 ZDR 时，还要单独选是否允许 Safety Retention。Google 模型与「所有其他模型」分开设。选 ZDR only 会排除 Safety Retention 端点，有些模型会不可用。
- 没做选择时：自助账户允许 Safety Retention，企业账户用 ZDR only。
- 设置位置：隐私设置 https://openrouter.ai/settings/privacy ，或护栏。护栏只能比账户更严。
- API：`provider.zdr: true` 只能加严，不能放松账户或护栏。也有按模型组的开关：Anthropic、OpenAI、Google、SpaceXAI、其他。旧字段 `enforce_zdr` 已弃用。
- 2026-10-18 起，经 Vertex 提供的 Google 模型被 Google 要求 Safety Retention：默认不留 prompt，但安全分类器标记的 prompt 最多留 90 天做滥用监测，不用于训练。这些端点不满足严格 ZDR。若自己的 Google Cloud 合同含 ZDR，可用 BYOK 并在密钥上声明 ZDR。
- 内存中的隐式 prompt 缓存不被 OpenRouter 算作留存，ZDR 路由仍会打到这些端点。
- 插件和自选工具（含网页搜索）不受 ZDR 路由约束。ZDR 强制时，网页搜索 / Web Fetch 会拒绝 Firecrawl 引擎。
- 某端点政策不清楚时，OpenRouter 按「既留存也训练」处理。
- 私有端点的 ZDR 来自客户自己的声明，不是上游合同的核验。

账户里还要关掉两项目志，否则 OpenRouter 自己会存正文（https://openrouter.ai/docs/guides/privacy/data-collection ，查询 2026-11-12）：

- Private Input & Output Logging：默认关。
- 允许 OpenRouter 使用输入输出改进产品：默认关，打开换 1% 折扣。
- CSAM：包括 ZDR 流量在内都会在传输中筛。疑似 CSAM 可以为人审、报告和法律保全而留存。公告写报告材料依法保全一年，不是每一条被扫到的流量都保全一年。https://openrouter.ai/blog/announcements/csam-screening （文内日期指向 2026-10 的计划更新；ZDR 文档页首仍标 “Planned update (October 27, 2026)”，与正文已经写成现行政策并存。**文档是否已经完成条款更新：未核实。**）

### 各下游留存差异

供应商默认表（不是每个型号）：https://openrouter.ai/docs/guides/privacy/provider-logging （查询 2026-11-12）。同页写明，个别端点可以不同于供应商默认，ZDR only 时可能被拿掉。

与本次候选相关的默认行：

| 供应商 | OpenRouter 表上的留存 | 是否可能训练 |
|---|---|---|
| DeepInfra | Zero retention | 不训练 |
| DeepSeek | 留存期未知 | 可能训练 |
| Together | Zero retention | 不训练 |
| Fireworks | Zero retention | 不训练 |
| Groq | Zero retention | 不训练 |
| SiliconFlow | Zero retention | 不训练 |
| Google Vertex | Zero retention | 不训练 |
| Google AI Studio | 55 天 | 不训练 |
| Alibaba Cloud Int. | 留存期未知 | 不训练 |
| Moonshot AI | Zero retention | 不训练 |
| MiniMax | 留存期未知 | 不训练 |

这张表是 OpenRouter 的分类。它和 DeepInfra 条款里的例外、Vertex 文档页上的 Safety Retention、阿里云默认 30 天，并不总是同一句话。**端点级以各家自己的条款为准；这张表只说明 OpenRouter 会怎么路由。** 具体账号里某个模型当前落到哪一家、那一家的端点是不是 ZDR，必须登录模型页或活动日志才能看。本次没有登录，**未核实。**

区域：Business 与 Enterprise 可以把请求发到 `https://eu.openrouter.ai` 或 `https://us.openrouter.ai`，该请求的 prompt 和完成内容留在该区域内。没有亚太区域主机名。https://openrouter.ai/docs/guides/privacy/provider-logging

### DPA

- 面向一般商业使用的 DPA，最后更新 2026-08-26：客户是控制者，OpenRouter 是处理者。https://openrouter.ai/data-processing-agreement
- 条款（查询页面标注发布于 2026-08-31）第 10.2 条：以组织身份或为商业、营利目的使用的，DPA 经引用并入条款。https://openrouter.ai/terms
- 价格页：自助方案的 DPA 是 “Via Terms of Service”；企业方案可以要签字件，并可以谈定制。https://openrouter.ai/pricing.md （查询 2026-11-12）
- 企业准入协议里也有一份作为附件的 DPA，并禁止转售（见下）。https://openrouter.ai/terms-of-service-enterprise

本项目没有签署，也没有企业账户。字段记 **未签**。DPA 是否覆盖下游模型提供商、儿童数据、以及 Safety Retention，本次没有逐条读完附件。**未核实。**

### 转售与年龄

条款写：使用服务的人必须年满 18 岁。禁止「access the Site or Service for purposes of reselling API access to Models or otherwise developing a competing service」。另禁止出售或转让访问权。https://openrouter.ai/terms

企业准入协议（页面标注更新于 2026-06-22）第 2.3.7 条禁止「rent, resell or otherwise allow any third-party access to or use of the Service」。**但同一协议第 2.1(a)(ii) 条明文授予许可，可以「making the Service available to Customer's End Customers」**；第 1.11 条把 End Customers 定义为通过本协议访问服务的客户的客户，序言也写明服务用于「customers' products and services」。协议没有正面调和这两条；按常见读法，2.1(a)(ii) 是对终端客户使用的专门授权，2.3.7 针对的是把访问权本身拿去出租或转卖。该许可不可再许可（non-sublicensable）。https://openrouter.ai/terms-of-service-enterprise （2026-11-12 主会话复核原文）

> **复核更正（2026-11-12，主会话）**：初稿只写了 2.3.7 的禁止，漏了 2.1(a)(ii) 的终端客户许可，因此把 OpenRouter 写成「卡在转售禁令上」。更正后的结论是：**自助条款**（第 7 条第 4 项）禁止为转售 API 访问而使用，且自助条款没有明文授予终端客户使用权，只在 §5.1 承认「客户把服务嵌入自己的产品」；**企业协议**明文允许把服务提供给自己的终端客户。iDoris 云形态把 token 经自己的账户、凭证、预算提供给 eduai 和 Agent24，更接近「嵌入自己的产品」而不是「转卖原始 API 访问」，但这一定性仍须律师确认。

iDoris 计算服务的产品形态就是把上游 token 提供给 eduai 和 Agent24。走自助账户时，这与第 7 条第 4 项直接相关，不应接入；走企业协议时，有明文的终端客户许可，转售问题基本可解，剩下的是 DPA 正文、留存例外和儿童数据。年龄条款约束的是账户持有人，没有写成「禁止处理终端用户中的儿童数据」，也没有写成允许。**儿童数据是否被 DPA 或 AUP 排除：未核实。**

### 能力

这些是网关能力。模型或下游不支持时，请求会失败或被改写。本次没有对具体型号做实测。

| 能力 | 结论 | 出处 |
|---|---|---|
| 工具调用 | `tools` / `tool_choice` / `parallel_tool_calls` 按 OpenAI 形状接收，非 OpenAI 供应商由网关转换 | https://openrouter.ai/docs/api_reference/parameters （页面时间 2026-11-12） |
| 流式 usage | Chat Completions 的流在 `[DONE]` 前必有一块 usage。`stream_options.include_usage` 与 `usage: {include: true}` 已弃用，且不再起作用 | https://openrouter.ai/docs/guides/administration/usage-accounting 与 https://openrouter.ai/docs/api_reference/streaming |
| 结构化输出 | `response_format.type = json_schema`，建议 `strict: true`。模型不支持时请求失败 | https://openrouter.ai/docs/features/structured-outputs |
| 缓存计价 | 多数供应商自动缓存；用量里有 `cached_tokens` 与 `cache_write_tokens`。OpenRouter 会尽量把后续请求送到同一家以碰到热缓存 | https://openrouter.ai/docs/features/prompt-caching 与 usage accounting 页 |
| 取消 | 断开流式连接。名单内的供应商会停算停计费，包括 Fireworks、DeepInfra、Together、DeepSeek。明确写尚未支持的包括 Groq、Google、Google AI Studio、Alibaba、Nebius。非流式或不支持的供应商会继续算完并计费 | https://openrouter.ai/docs/api_reference/streaming |

### 仍要人做的事

- 若仍想用：只走企业协议（第 2.1(a)(ii) 条有终端客户许可），不走自助账户；让律师确认 iDoris 云形态属于「提供给终端客户」而不是第 2.3.7 条的「转售」。
- 企业方案、签字 DPA、账户设为全模型组 ZDR only，并排除 Google 与其他模型的 Safety Retention。
- 确认没有打开输入输出日志，也没有打开 1% 折扣的数据使用。
- 登录每个候选模型页，核对当前端点的数据政策不是 Safety Retention，也不是「政策不明所以按留存+训练处理」。
- 不要依赖私有端点上的自我声明来满足 child_edu。

## 3. DeepSeek 官方 API（只评估 adult）

**结论（高，对已读条款；中，对开放平台终端用户是否同一套存储规则）：** 不满足 child_edu，这与设计稿一致。隐私政策写个人数据在中华人民共和国收集、处理、存储；服务不以儿童为对象。条款允许在去标识化之后，把输入和输出少量用于改进服务，可用「Improve the model for everyone」退出。开放平台条款明确允许开发者把模型接到面向内部或外部终端用户的下游系统，这比 DeepInfra / OpenRouter 更接近「可以做下游产品」。没有找到公开的 DPA。推理机房的城市、以及 API 控制台里那个退出开关是否存在：**未核实**。

### 处理区域与留存

隐私政策，最后更新 2026-02-10。https://cdn.deepseek.com/policies/en-US/deepseek-privacy-policy.html

- 控制者：Hangzhou DeepSeek Artificial Intelligence Co., Ltd.，注册地址在中国。
- 「Where We Store Your Personal Data」：收集的个人数据可能存在你所在国家以外的服务器上；为提供服务，他们在中华人民共和国直接收集、处理并存储个人数据。
- 保留：为提供服务及政策中的其他目的而保留必要期限，也包括履行法律义务和正当业务利益（含改进服务与安全）。没有写固定天数。
- 使用目的包括训练和改进机器学习模型。权利里包括退出「把个人数据用于训练模型或优化技术」。政策没有写 API 控制台的开关路径。
- 儿童：服务不以儿童为对象，不故意处理儿童个人数据。服务也不面向敏感个人数据，政策举例包括儿童个人数据，并要求不要提供。
- 重要边界：政策写明，开发者用开放平台做的下游系统，其终端用户个人数据的处理规则**不由本隐私政策覆盖**。开发者应自行向终端用户披露。

使用条款第 4.3 条：在加密、严格去标识化、且不能再识别到个人的前提下，可以少量使用 Inputs 和 Outputs 来提供、维护、运营、开发或改进服务及其底层技术。拒绝的方式是关掉 “Improve the model for everyone”。https://cdn.deepseek.com/policies/en-US/deepseek-terms-of-use.html （查询 2026-11-12；本次摘录没有显示该页的修订日期）

第 2.1 条：服务主要面向成年人；未满 18 岁或所在国最低年龄的，须与监护人一起阅读并在监护人同意下使用。

开放平台条款，发布 2026-04-22，生效 2026-04-29。https://cdn.deepseek.com/policies/en-US/deepseek-open-platform-terms-of-service.html

- 它是使用条款的具体协议，账户与聊天产品相同。
- 第 1.1 条：可以把 DeepSeek Chat 模型接到下游系统、应用或功能，向内部和外部终端用户提供服务。
- 第 3.3 条：向公众提供应用时，开发者必须告知个人信息规则，并就收集、处理以及委托 DeepSeek 处理取得同意或其他合法基础。
- 第 5.5 条：再次写明，终端用户的个人信息处理规则不在《DeepSeek 隐私政策》的说明范围内。

因此：开发者账户一侧的存储地和处理目的是写明的；**终端用户 prompt 是否同样落在中国、是否同样进入第 4.3 条的改进用途，条款把隐私政策排除在这个场景之外，又把使用条款并入。两份文件如何同时适用：未核实。** 不能写成「API 默认零留存」。OpenRouter 的供应商表把 DeepSeek 标成「留存期未知、可能训练」，与「可能用于改进」方向一致，但不能填补存储地的空白。

磁盘上下文缓存是产品功能：重复前缀从缓存读取，缓存命中有单独价格，文档写存储免费、命中按命中价计。https://api-docs.deepseek.com/news/news0802 （文档站查询 2026-11-12）。缓存是否把正文留在磁盘上、留多久，该新闻稿强调的是计费和安全策略的存在，**保留时长未核实。** 对 child_edu 这已经足够排除；对 adult，缓存意味着正文会为了后续命中而存在于他们的缓存系统里。

### DPA

开放平台 FAQ 写：API 以自助和标准化方式提供；若确需线下合作协议（举例是应用备案或企业入库），提交合作协议工单。https://api-docs.deepseek.com/zh-cn/faq （查询 2026-11-12）

没有找到名为 DPA 或数据处理协议的公开文本。合作协议是否包含 GDPR/PDPA 意义上的 DPA：**未核实**。字段不写「未签」冒充已经知道他们提供 DPA，写 **unverified**。

### 转售

第 1.1 条允许集成进下游系统并服务终端用户。第 4.2 条（使用条款）允许在守法的前提下把输入输出用于个人使用、学术、衍生作品、训练其他模型。第 5 条禁止未经许可使用 DeepSeek 品牌暗示官方合作、战略伙伴或认证集成。

「纯转售 API 访问、不自建应用」有没有被禁止，条款没有用 resell 这个词写明。下游产品这条路径是写明的。品牌用法要单独遵守。

### 能力与价格

文档：https://api-docs.deepseek.com/quick_start/pricing/ （查询 2026-11-12）

| 项 | deepseek-flash | deepseek-v4-pro |
|---|---|---|
| 基址 | `https://api.deepseek.com`（OpenAI 格式）；Anthropic 格式为 `/anthropic` | 同左 |
| 文档中的模型版本 | DeepSeek-V4.1-Flash-1017 | DeepSeek-V4-Pro-0813 |
| 上下文 / 最大输出 | 1M / 384K | 同左 |
| JSON、工具调用 | 支持 | 支持 |
| 旧名 | `deepseek-v4-flash` 仍被接受，但对应模型已下线，由 V4.1-Flash-1017 按 Flash 价格提供 | — |

英文价目（美元 / 百万 token）。高峰为周一至周五 01:00–04:00 与 06:00–10:00 UTC，不含中国法定节假日；其余时间为低峰，低峰是高峰的一半。

| | flash 低峰 | flash 高峰 | v4-pro 低峰 | v4-pro 高峰 |
|---|---|---|---|---|
| 输入，缓存命中 | 0.003 | 0.006 | 0.022 | 0.044 |
| 输入，缓存未命中 | 0.15 | 0.30 | 0.66 | 1.32 |
| 输出 | 0.60 | 1.20 | 1.98 | 3.96 |

中文价目页用人民币写出另一套数字（例如 flash 低峰：命中 0.02 元、未命中 1 元、输出 4 元 / 百万 token），并写高峰为北京时间周一至周五 9:00–12:00、14:00–18:00。https://api-docs.deepseek.com/zh-cn/quick_start/pricing 两套数字不是本次换算出来的。结算币种以账户所在价目为准，**未核实一个国际账户到底走哪一页。**

流式：Chat Completions 支持 `stream: true`（SSE）。https://api-docs.deepseek.com/zh-cn/api/create-chat-completion

`stream_options.include_usage` 是否被官方接受：**未核实**（本次打开的参数表摘录没有这一项）。用量字段本身存在，包括 `prompt_cache_hit_tokens`、`prompt_cache_miss_tokens`、`cached_tokens`。

结构化输出：价目表的 Json Output 为支持。JSON Schema 的严格模式是否与 OpenAI 的 `json_schema` 相同：**未核实**。

取消：OpenRouter 把 DeepSeek 列在流式断开即停算的名单里。DeepSeek 自己的文档本次没有写断开语义。**官方一侧：未核实。**

### 仍要人做的事

- adult 若要接：律师确认向中国传输、第 4.3 条的改进用途、以及「隐私政策不覆盖终端用户」之后，开发者还要在自己的隐私声明里写什么。
- 登录开放平台，确认是否有 “Improve the model for everyone” 或等价开关，并确认它覆盖 API 而不只覆盖聊天产品。
- 若需要签字 DPA，走合作协议工单，不要假定 FAQ 里的工单就是 DPA。
- 不把官方 API 写入 child_edu。

## 4. 其他候选（亚太或 ZDR 线索）

每一家只给一行，展开写在后面。没有一家在本次核验里同时具备：推理区域可证明在新加坡或泰国附近、默认或可合同化的 ZDR、以及本项目已签的 DPA。

| 供应商 | 一行结论 |
|---|---|
| Together | 无服务器默认会存 prompt 并可能用于产品改进；组织设置里关掉 Prompt storage 才是他们定义的 ZDR。公开的地理钉死只看到美国端点 `us-api.together.ai`（1.25 倍价格），不是新加坡。Trust Center 列了 DPA，正文未读。 |
| Fireworks | 开放模型默认不把 prompt 写入持久存储；企业策略可以拒绝会落盘的功能。公开的数据驻留表只写了美国。API 枚举里有 `APAC`，没有公开的新加坡无服务器端点。DPA 在 Trust Center，正文未读。 |
| Groq | 推理默认不留客户数据，但可靠性和滥用调查可以留最多 30 天。文档写被留下的客户数据在美国 GCP。处理者协议列在政策页，正文未读。不是亚太。 |
| 阿里云百炼国际站，新加坡接入 | 接入区域可以是新加坡，但新加坡的推理部署范围是「国际、不含中国大陆的全球节点」，不是钉在新加坡。默认留存不超过 30 天。ZDR 要企业客户找客户经理开通，且只覆盖已适配型号；即便开通，也有最长约 24 小时的加密缓存窗口，被安全过滤标记的日志最长 2 年。 |
| Google Vertex / Gemini Enterprise Agent Platform，区域端点 | 训练限制写在服务条款里。零留存不是默认：滥用日志、默认开启的持久上下文缓存（最长约 24 小时）、Grounding、Interactions API 的 `store` 默认 true，都要单独关掉或申请例外。区域端点的处理是否留在所选区域，以数据驻留文档为准；本次没有核完新加坡当前有哪些型号。 |
| SiliconFlow | 英文隐私政策与中文摘要对「会不会保存 prompt」说法不同。OpenRouter 把它标成零留存。推理区域未核实。 |
| Moonshot AI | 只看到 OpenRouter 写总部新加坡、零留存、不训练。一手隐私政策和区域文档本次没打开。除这一句线索外全部未核实。 |
| OpenTyphoon（SCB 10X 官方 API） | 有 OpenAI 兼容的免费研究接口，并写明收集使用数据来改进模型。不是 ZDR，也没有读到 DPA。 |

### Together

https://docs.together.ai/docs/privacy-and-security 与 https://docs.together.ai/docs/zero-data-retention （查询 2026-11-12）

- 默认存储 prompt 和响应，并可能用于产品改进。不默认与第三方分享。组织管理员可关 Prompt storage，从而启用他们定义的 ZDR。关掉后，透传模型（passthrough）也会关。
- 训练他人模型是选择加入，默认关。
- ZDR 仍保留：用量元数据、账户与账单、客户主动上传的微调或批量文件。
- 地理：`us-api.together.ai` 让所选无服务器模型的推理处理留在美国，价格为标准的 1.25 倍。文档写这只保证处理地点，不保证存储、日志和路由地点。其他区域用专属端点并钉硬件。企业 VPC 提到包括欧盟。**没有查到新加坡无服务器区域。**
- DPA 列在 https://trust.together.ai/ （页面有 2026-10-23 的子处理者更新）。正文本次没打开。子处理者列表含 AWS，地点美国。
- 转售、儿童：本次打开的页面没有这两条。**未核实。**
- 能力：文档描述无服务器推理沿用与专属端点相同的 API，前缀缓存是尽力而为。工具调用、`include_usage`、结构化输出、取消的型号级支持：**未核实**。

### Fireworks

https://docs.fireworks.ai/guides/security_compliance/data_handling 与 https://docs.fireworks.ai/accounts/zero-data-retention （查询 2026-11-12）

- 开放模型：不把 prompt 和生成结果打进持久存储，除非用户选择加入（例如 FireOptimizer）。请求期间在易失内存；若开了 prompt 缓存，部分 prompt 和 KV 会在易失内存里放几分钟。
- 企业账户可打开 ZDR 策略，拒绝会持久化或记录客户内容的请求：Responses API 在 `store: true`（默认）或 `background: true` 时被拒，批量与训练被拒。聊天补全允许。策略不管内存里的 prompt 缓存，也不管计费元数据。法律或防止滥用仍可能留存。
- Responses API 在 `store=true` 时，对话最多留 30 天，常常更早被赶出缓存。
- 数据驻留是企业功能。公开表只有「不限制」和「美国」（`us.api.fireworks.ai`）。未列出的区域写 contact sales。策略 API 的枚举含 `US`、`CANADA`、`EUROPE`、`APAC`。**没有公开文档证明无服务器推理能钉在新加坡或泰国。APAC 是否已有容量：未核实。** https://docs.fireworks.ai/accounts/data-residency 与 https://docs.fireworks.ai/api-reference/get-policy-settings
- DPA：企业控制页写协议在 Trust Center。https://docs.fireworks.ai/nexus/enterprise-controls 正文未读。
- 2026-10 安全事件通报写：没有发现对默认 ZDR 的 Inference 的影响。这不是区域或儿童条款。https://fireworks.ai/security/bulletins/october-2026-security-incident-update
- 转售与儿童：**未核实。**
- 取消：OpenRouter 名单包含 Fireworks。Fireworks 自己的断开语义：**未核实。**

### Groq

https://console.groq.com/docs/your-data （查询 2026-11-12）

- 用量元数据始终收集，不含输入输出。
- 推理默认不留客户数据。会留下的情况：批量、微调，以及为平台可靠性或滥用调查而临时记录，最多 30 天，法律要求更长则更长。
- 数据控制里可以选 Zero Data Retention。同页仍把推理标成 ZDR Eligible，保留类型是「系统可靠性与滥用监测，最多 30 天」。所以他们的 “ZDR” 不是「任何情况下都不留正文」。
- 「All customer data is retained in Google Cloud Platform (GCP) buckets located in the United States.」这句说的是留下来的数据放在美国，不是一张推理区域地图。**推理是否只在美国：未核实。**
- 处理者协议：政策索引列出 Groq Customer Data Processing Addendum。https://console.groq.com/docs/terms-of-use-ksa/ 正文本次没打开。
- 转售与儿童：**未核实。**
- 生产型号价见第 6 节。没有在型号表里看到 Typhoon。

### 阿里云百炼国际站（新加坡）

留存政策，最后更新 2026-11-10。https://www.alibabacloud.com/help/en/model-studio/data-retention-policy

- 默认：推理的 prompt 和响应留存不超过 30 天，法律要求或为保障服务安全所合理必要的可以更长。有状态功能留到用户删除。
- 未经另行同意，不把业务数据用于训练、微调或改进基础模型。这句话在该政策第 5.2 节，阅读时请与「默认仍留存 30 天」一起看，留存和训练是两件事。
- ZDR：仅国际站企业客户，由客户经理评估后开通，工作空间级。覆盖新加坡、弗吉尼亚、法兰克福、中国香港、东京等非中国大陆区域。未适配 ZDR 的型号即使用了 ZDR 工作空间，仍走标准留存，且不会被拦截。
- 已写明支持 ZDR 的文本系列包括：qwen-max（自 qwen3.8-max）、qwen-plus（自 qwen3.7-plus）、qwen-flash（自 qwen3.8-flash）、deepseek-pro（自 deepseek-v4-pro-0813）、deepseek-flash（自 deepseek-v4-flash-0731）、glm（自 glm-5.2）、kimi（自 kimi-k2.7-code）。第三方厂商直接提供的模型不在 ZDR 内。
- ZDR 下正文不写持久存储，但为完成推理会经过加密缓存窗口，默认不超过 24 小时，窗口内不用于训练或人工审阅。被自动安全过滤标记的内容，按隐私政策和法律保留，不超过 2 年。没有关闭内容安全的开关。
- 不支持 ZDR 的路径包括：应用编排、RAG、批量、控制台体验、有状态的 OpenAI 兼容 Responses API。

区域。https://www.alibabacloud.com/help/en/model-studio/regions/ （查询 2026-11-12）

- 新加坡区域 ID `ap-southeast-1`。请求数据存在所选接入区域。
- 新加坡的服务部署范围只有 International：不含中国大陆的全球节点。文档写推理节点上的瞬时数据不持久化。**这不是「推理只在新加坡」。**
- 工作空间域名形态：`{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com`。DashScope：`dashscope-intl.aliyuncs.com`。

OpenAI 兼容：角色扮演文档给出新加坡的 Chat Completions 兼容基址 `https://{WorkspaceId}.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1`。https://www.alibabacloud.com/help/en/model-studio/role-play 通用聊天型号是否同一路径：**该页的例子是角色模型，外推到全部型号未核实。**

DPA、转售、儿童：本次没有打开对应条款。**未核实。**

价格见第 6 节。币种为文档中的美元。

AWS Bedrock：按 owner 2026-11-12 决定排除。

### Google（Vertex / Gemini Enterprise Agent Platform）

零留存说明，最后更新 2026-11-10 UTC。https://docs.cloud.google.com/gemini-enterprise-agent-platform/resources/zero-data-retention

- 不在没有事先许可或指示时，用客户数据训练或微调模型。
- 滥用监测：受 Google Cloud Platform 服务条款约束的客户，Google 可能记录 prompt。要零留存需申请滥用监测例外。部分 Advanced AI 功能可能无法做到零留存，要问客户团队。
- 隐式缓存默认开。内存缓存最长约 24 小时，只在内存、按项目隔离、不用于训练。持久缓存在容量不够时把由输入算出的 KV 写入静态存储，同样最长约 24 小时，区域端点会守所选位置的数据驻留；全球端点可以从任何区域提供。可以按项目关掉持久缓存或全部缓存。
- Grounding with Google Search / Maps 会存储，不能单关存储。Interactions API 的 `store` 不填时默认为 true。
- 同页给出 Cloud Data Processing Addendum 的链接：https://cloud.google.com/terms/data-processing-addendum 正文本次没有通读。本项目未签。

数据驻留文档写：静态数据留在客户所选位置；生成式 AI 的机器学习处理发生在发出请求的区域或多区域。https://cloud.google.com/vertex-ai/generative-ai/docs/learn/data-residency 该页表格里新加坡（`asia-southeast1`）出现在型号可用性表中，摘录里能看清的是 Gemini 2.5、2.0 与部分第三方型号的列头。**2026-11-12 当天新加坡实际在售的型号清单，没有逐行核完，未核实。**

OpenRouter 把「Google Vertex」供应商默认标成 Zero retention，同时又把 Vertex 上的 Google 模型写成 Safety Retention。这是路由分类和 Google 文档的差异，许可表以 Google 文档为准：默认不是严格 ZDR。

OpenAI 兼容接口在新加坡是否提供：**未核实**（本次没有打开一份写明 Vertex OpenAI 兼容端点的当前文档）。

### SiliconFlow

- 英文隐私政策：除非法律或具体服务规则要求，他们没有义务存储 Interaction Data。个人数据保留到收集目的不再需要为止。定义里提到 PDPA，这不能推出机房在新加坡。https://docs.siliconflow.com/en/legals/privacy-policy （页面时间 2026-09-09）
- 中文摘要（更新日 2026-07-30）把「输入 prompts 和输出内容」列为内容安全场景要收集的个人信息，依据包括生成式人工智能服务管理暂行办法。https://docs.siliconflow.cn/docs/legals/privacy-policy-summary
- 两份文档哪一份约束国际 API、推理发生在哪：**未核实**。在冲突解开之前，不能标 ZDR。
- OpenRouter 的「Zero retention」只是路由商分类。
- 价格站抓取到人民币价目（见第 6 节），页面是中文站。国际站是否同价：**未核实。**

### Moonshot

仅 OpenRouter 供应商表：总部新加坡、Zero retention、不训练。https://openrouter.ai/docs/guides/privacy/provider-logging

一手隐私政策、DPA、真实推理区域、价格：**未核实**。不单列进可执行的许可行，只在草案里留一条全字段未核实的线索，避免以后被人当成已经核过。

## 5. 泰语模型的托管

### Typhoon（SCB 10X）官方

文档站 https://docs.opentyphoon.ai/en/models/ 与 FAQ https://docs.opentyphoon.ai/en/faq/ （页面带有 2025-12-12 的发布时间；2026-11-12 仍能打开）

| 模型 ID | 文档中的上下文 | 速率 | 价格 |
|---|---|---|---|
| `typhoon-v2.5-30b-a3b-instruct` | 128K | 5 请求/秒，200 请求/分钟 | FAQ：研究展示，免费 |
| `typhoon-v2.1-12b-instruct` | 56K | 同上 | 免费 |
| `typhoon-ocr` | 文档未给出窗口 | 2 请求/秒，20 请求/分钟 | 免费 |
| `typhoon-asr-realtime` | — | 100 请求/分钟 | 免费 |

- 基址在文档示例中为 `https://api.opentyphoon.ai/v1`，示例使用 OpenAI SDK。https://docs.opentyphoon.ai/en/rate-limits/
- FAQ：会收集使用数据以改进模型与 API；不会与第三方分享；SCB 10X 不对输出主张权利。这是留存加改进，不是 ZDR。
- 同一套文档的另一页仍把 Together.ai 写成官方云伙伴、把 OpenRouter 写成社区路由。https://docs.opentyphoon.ai/en/ 该页与下面 Together 的现况矛盾，以 Together 自己的页面和 Typhoon 博客的下线通知为准，文档索引视为过期。**2026-11-12 是否仍能用密钥调通上述四个 ID：未核实**（没有登录，也没有请求 `/v1/models`）。
- 博客上的「每百万 token 0.10 美元」是他们自己算的 H100 推理成本，不是 API 价。https://opentyphoon.ai/blog/en/typhoon2-5-release 不写入价目。
- 官方博客写：基于 Together 的 Typhoon API Pro 于 2025-12-31 停止；并预告 2026 年初在 AWS 上提供新的生产 API。https://opentyphoon.ai/blog/en/introducing-typhoon-2-api-pro-accessible-production-grade-thai-llms-3e139c077aab **这个 AWS API 在 2026-11-12 是否已经上线、区域、价格、DPA：未核实。** 历史价格（12B 每百万 token 0.20 美元、70B 0.88 美元）只属于已经通知停止的 API Pro，不能当现价。

### 在其他供应商上

| 地方 | 查到的事实 | 没查到的 |
|---|---|---|
| Together | Typhoon 2 8B Instruct 页面写：「This model is not available on Together’s Serverless API.」https://www.together.ai/models/typhoon-2-8b-instruct （页面时间 2026-11-05，查询 2026-11-12） | 其他 Typhoon 型号是否还有专属部署，未逐个打开 |
| OpenRouter | URL `https://openrouter.ai/scb10x/llama3.1-typhoon2-70b-instruct` 返回了站点外壳。正文里的价格、在线供应商、是否仍在售，抓取结果里没有 | 能否实际调用：未核实。Typhoon 博客曾写 70B 计划在 2025-08-20 弃用，那是旧通知，不能单独证明今天的上下架 |
| DeepInfra 定价页 | 已读的 DeepSeek 与 Qwen 分组里没有 Typhoon 字样。https://deepinfra.com/pricing | 没有做全站穷尽搜索，不能写成「确定没有」 |
| Fireworks、Groq、百炼、Vertex | 本次检索没有打开到 Typhoon 型号页 | 托管与价格：未核实 |
| SiliconFlow 价目页 | 已展示的厂商分组里没有 Typhoon。https://www.siliconflow.cn/pricing | 同上，未穷尽 |

SeaLLM、OpenThaiGPT 以及其他泰语能力较强的开源模型，本次没有逐家检索到托管页。**未核实。** 不把印象中的旧托管写成现价。

## 6. 价格

币种按来源页面。查询日期都是 2026-11-12。缓存价单列。促销价会变，下表同时记下页面上的标价和折扣价（若有）。

### DeepInfra

来源：https://deepinfra.com/pricing 与型号页 https://deepinfra.com/deepseek-ai/DeepSeek-V3.2 、https://deepinfra.com/deepseek-ai/DeepSeek-V4-Flash/api 。美元 / 百万 token。

| 模型 | 输入 | 缓存输入 | 输出 | 备注 |
|---|---|---|---|---|
| DeepSeek-V4-Flash | 0.09 | 0.018 | 0.18 | 型号页另有 Priority 1.5 倍、Flex 0.8 倍 |
| DeepSeek-V4-Flash-0731 | 0.06 | 0.015 | 0.18 | 定价页 |
| DeepSeek-V4-Pro / V4-Pro-0813 | 1.30 | 0.10 | 2.60 | 定价页两行同价 |
| DeepSeek-V4.1-Flash-1017 | 标价 0.20，缓存 0.006；折后 0.098，缓存 0.0029 | 见左 | 标价 0.60，折后 0.294 | 页面写 51% off |
| DeepSeek-V3.2 | 0.26 | 0.13 | 0.38 | 上下文 163840。Flex 为 0.8 倍 |
| DeepSeek-V3.1 | 0.25 | 0.13 | 0.95 | 定价页 |
| Qwen3.6-35B-A3B | 0.10 | 0.10 | 0.95 | 定价页。缓存与输入同价 |
| Qwen3.5-397B-A17B | 0.45 | 0.22 | 3.00 | 定价页 |
| Qwen3-Coder-480B-A35B-Instruct-Turbo | 0.30 | 0.10 | 1.00 | 定价页 |

付费缓存保留的写入倍数（在输入价之上）：5 分钟 1.25 倍，1 小时 2 倍。https://docs.deepinfra.com/chat/prompt-cache-retention

### DeepSeek 官方

见第 3 节表格。美元价来自英文文档，人民币价来自中文文档。不要把两列相除当成汇率。

### Together

只记录本次为核对 Typhoon 页面时，出现在同一页「相关模型」里的标价。美元 / 百万 token。https://www.together.ai/models/typhoon-2-8b-instruct

| 模型 | 输入 | 缓存 | 输出 |
|---|---|---|---|
| GLM-5.3 | 1.40 | 0.26 | 4.40 |
| GLM-5.3-Flash | 0.15 | 0.03 | 0.50 |
| DeepSeek-V4.1-Flash-1017 | 0.30 | 0.006 | 1.20 |
| Kimi K3.1 Pro | 2.00 | 0.20 | 10.00 |

这些是相关模型卡片，不是 Together 全目录。美国地理端点另乘 1.25，见 Together 文档。

### Groq

https://console.groq.com/docs/models 与 https://console.groq.com/docs/model/qwen/qwen3.8-27b 。美元 / 百万 token。

| 模型 | 输入 | 输出 | 备注 |
|---|---|---|---|
| openai/gpt-oss-120b | 0.15 | 0.60 | 生产 |
| openai/gpt-oss-20b | 0.075 | 0.30 | 生产 |
| qwen/qwen3.8-27b | 0.80 | 4.00 | 预览，文档写勿用于生产 |
| Llama 3.3 70B、Llama 3.1 8B 企业版 | Contact Sales | Contact Sales | 页面没有公开单价 |

没有缓存列。

### 阿里云百炼（文档中的国际部署范围）

https://www.alibabacloud.com/help/en/model-studio/model-pricing （查询 2026-11-12）。美元 / 百万 token。下表只摘新加坡/国际范围里读清楚的行，不是全表。

| 模型 | 输入 | 输出 | 备注 |
|---|---|---|---|
| qwen-plus（文档写当前等同 qwen-plus-2025-12-01），International，≤256K | 0.40 | 非思考 1.20；思考 4.00 | 无免费额度 |
| 同上，256K–1M | 1.20 | 非思考 3.60；思考 12.00 | |
| qwen-plus-character，新加坡表 | 0.50 | 1.40 | https://www.alibabacloud.com/help/en/model-studio/role-play |

qwen3.7-max 等行在摘录里同时出现 Global 与分区域，容易抄错部署范围。未在上表逐行拆开的价格保持 **未核实**，用之前打开定价页原文。

### SiliconFlow 中文站

https://www.siliconflow.cn/pricing 摘录为人民币 / 百万 token，且按时段变化。国际站美元价未核实。

| 模型 | 时段 | 输入 | 输出 | 缓存 |
|---|---|---|---|---|
| deepseek-ai/DeepSeek-V4.1-Flash | 页面写的 0 点–2 点与 8 点–24 点 | 2.00 元 | 8.00 元 | 0.04 元 |
| 同上 | 2 点–8 点 | 1.00 元 | 4.00 元 | 0.02 元 |

页面没有写时区。**未核实是北京时间还是其他时区。**

### OpenTyphoon

FAQ：免费。没有每百万 token 的 API 价。速率限制见第 5 节。

### OpenRouter

没有登录，模型页没有给出可引用的现价。抽成比例本次没有在已读页面里核到一个当前数字。**未核实。** 不要用设计脑暴稿里的旧价。

## 7. 推荐顺序

排序的前提：设计稿要求 child_edu 同时满足 ZDR 和已签 DPA；没有合规候选就拒绝，而不是退到次好的上游。今天没有任何一行满足「已签」。下面是**把缺口补上之后**的优先顺序，不是现在可以放行的名单。

### child_edu

1. **先不要放行。** 在 DPA 签署、转售书面许可、以及律师接受「条款里的例外仍算设计稿要求的 ZDR」之前，路由应拒绝云端 child_edu。
2. 若只比「公开文件离要求有多近」：DeepInfra 自托管开源型号（排除 Google / Anthropic、图像、批量、付费缓存保留）的合同文字最完整，区域是美国，转售要服务订单。它不是亚太。
3. 阿里云百炼国际站新加坡工作空间，在客户经理开通 ZDR、且型号在已适配清单内、且不走 RAG / 批量 / 有状态 Responses 时，接入区域最近。推理仍可能在「不含中国大陆的全球节点」之间调度，24 小时缓存窗口和最长 2 年的安全日志也还在。区域没有钉死之前，不能把它写成新加坡推理。
4. Google 区域端点 `asia-southeast1`：只有在关掉持久缓存、不用 Grounding、Interactions 明确 `store=false`、并拿到滥用监测例外之后，才接近零留存。型号是否在新加坡提供，要按当时的型号表再核一次。OpenAI 兼容未核实。
5. OpenRouter 的 ZDR only **不要排在前面**。自助条款禁止转售 API 访问，只能走企业协议（有终端客户许可）；自助默认还允许 Safety Retention；CSAM 例外仍在；下游区域除企业版的欧盟/美国主机名外不可控。它适合作为「模型很多」的线索，不适合作为儿童数据的上游，除非走企业协议、律师确认定性并且后台逐个端点核过。
6. Together、Fireworks、Groq 的公开材料都比「已签 DPA + 无例外 ZDR + 亚太」少一截。Fireworks 的默认不落盘较接近，但亚太无服务器没有公开端点。Groq 的 30 天滥用/可靠性日志不符合严格零留存。
7. SiliconFlow、OpenTyphoon 官方免费 API、DeepSeek 官方：不进入 child_edu 短名单。前两者的留存陈述分别是冲突或明确用于改进；DeepSeek 官方是设计稿已经排除的，本次阅读支持这个排除。

### adult

adult 在设计稿里不要求同一套儿童门槛，但仍要过租户自己的区域和留存约束。若租户允许美国、并接受「有例外的不落盘」：

1. DeepInfra 上的开源型号（上表里的 DeepSeek-V3.2、V4-Flash、Qwen 系列），价格是本次读到的公开价里较低的一档，工具调用、结构化输出、流式用量和缓存计价都有文档。先补转售服务订单。取消语义仍未核实，S4 不能假设断开即停计费。
2. DeepSeek 官方 `deepseek-flash` / `deepseek-v4-pro`，仅在接受中国处理与存储、以及第 4.3 条改进用途（或确认 API 侧已经退出）之后。价格文档完整，缓存折扣大。没有公开 DPA。
3. 阿里云百炼新加坡接入，适合「希望接入点在新加坡」的 adult 流量。默认是最多 30 天留存。要更严就走企业 ZDR，并接受推理范围不是单城。
4. Fireworks、Together、Groq 作为价格或延迟的对照，不在 DPA 正文读完前替代前三名。Together 默认留存，必须先关 Prompt storage。
5. OpenRouter 只能走企业协议：自助条款禁止转售 API 访问，企业协议第 2.1(a)(ii) 条允许提供给终端客户。adult 同样适用这一前提。

泰语质量不在这次排序里。设计稿写模型选型以泰语评测集为准。托管上，今天能指出的可用接口只有 OpenTyphoon 文档里的免费研究 API，且它收集数据用于改进，不能进 child_edu。Together 上的 Typhoon 2 8B 无服务器已不可用。AWS 上的下一代 Typhoon API 是否上线，未核实。

## 8. 必须由人做的事

这些都不能靠再读公开网页完成。

1. **律师**看设计稿「ZDR」是否允许：法律/滥用例外、内存缓存、CSAM 保全、阿里云 24 小时缓存窗口、Groq 最多 30 天的可靠性日志。看完再改许可表，不要由工程侧把「有例外」改成 ZDR。
2. **DeepInfra 销售或 Trust Center**：索取 DPA 全文；服务订单写明可以让终端用户使用；确认无服务器推理区域；问清楚滥用抽存会不会保存正文。
3. **OpenRouter 企业销售**：确认企业协议第 2.1(a)(ii) 条的终端客户许可覆盖 iDoris 云形态（把 token 经自己的账户与预算提供给 eduai、Agent24），并书面排除第 2.3.7 条的适用；要签字 DPA；开通后由人登录隐私设置，把各模型组设为 ZDR only，并排除 Safety Retention；对每个要上的模型看端点政策。
4. **DeepSeek**：登录开放平台确认训练退出开关是否覆盖 API；若 adult 需要 DPA，提交合作协议工单并让律师看中国存储。不要为 child_edu 提这个工单当解决方案。
5. **阿里云客户经理**：评估并开通工作空间 ZDR；书面确认推理节点的地理范围能不能收成新加坡；确认要买的型号在 ZDR 清单内；提供 DPA。开通前默认按最多 30 天留存对待。
6. **Google Cloud 客户团队**：为将使用的项目申请滥用监测例外；关掉持久缓存；核对 `asia-southeast1` 当天的型号表；确认结算方式会不会改变滥用日志的适用范围（文档把「受 GCP 条款约束」写成日志条件，发票账户是否除外要以账户团队的书面答复为准，本次两版文档措辞不完全相同）。
7. **Together / Fireworks / Groq**：若短名单需要他们，向 Trust Center 要 DPA 正文。Fireworks 问 APAC 枚举是否已有可买的区域端点。Together 在组织设置关掉 Prompt storage 之后再谈。
8. **Typhoon**：向 SCB 10X 或 OpenTyphoon 确认免费 API 是否仍是 2025-12 文档中的四个 ID，以及预告的 AWS 生产 API 是否已上线、区域和合同。在此之前不要把 Together 或 OpenRouter 写成 Typhoon 的生产上游。
9. **延迟**：设计稿 §15 要求实测新加坡到各上游。本次没有测。
10. **S0**：把本草案的字段收成契约。在那之前，yaml 里的值不能被路由器读取为授权。

## 9. 本次故意没有写成事实的内容

- 任何供应商「可以处理儿童数据」或「已经允许 iDoris 转售」。
- DeepInfra、OpenRouter、Vertex 的推理延迟。
- OpenRouter 当前抽成比例和具体模型现价。
- Moonshot、MiniMax 的一手留存与区域。
- SiliconFlow 国际站与中文站哪一份政策适用。
- 除已点名的 Typhoon 页面外，其他泰语模型的托管清单。
- DPA 正文里的标准合同条款、子处理者名单细节、责任上限。正文没有打开的，都不摘要。
