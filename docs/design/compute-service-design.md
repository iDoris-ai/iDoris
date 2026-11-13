# idoris.ai/api 计算服务设计（定稿 1.0）

> 状态：**定稿 1.0，等 owner 过目后启动开发**（2026-11-05）。分支 `feat/compute-service`。
> 来源：脑暴稿 `compute-service-brainstorm.md`（供应商调研、价格、场景、模型候选），以及 Codex 挑战评审 `compute-service-codex-review.md`。本文和脑暴稿冲突时，以本文为准。
> 不变的东西：本地 iDoris 的 launcher 契约（规范 §2.7）、wire 1.1.0（§2.8）、Admin v0 只读且只听 loopback。本方案不改其中任何一条。

## 0. 一句话

idoris.ai/api 是我们自己运营的云形态 iDoris，接口兼容 OpenAI，用的是别人的算力，给 **eduai**（泰国首发，面向儿童）和 **Agent24**（办公 agent）供应 token。它和个人 iDoris、组织 iDoris 是**同一套代码的三种形态**。三种形态共享账户、预算、路由的规则，但**不共享密钥、不共享身份，也不跨节点共享事务**。

## 1. owner 已拍板的决策

| # | 决定 |
|---|---|
| 1 | 名字叫 idoris.ai/api，对外地址 `https://idoris.ai/api/v1` |
| 2 | eduai 首发泰国，**只发免费额度**；支付到 P2 再接 |
| 3 | 信用负值：只允许有担保的负值，加速率闸门，额度不够先降级 |
| 4 | 本地 iDoris 不改成 SaaS；SaaS 是云形态 |
| 5 | 40% 回流口径不在本服务考虑 |
| 6 | 前沿闭源模型不进 P0 |
| 7 | P0 不租 GPU，用 token API；Modal 放到 P1 跑自定义模型 |
| 8 | 三种形态之间的 4 处不一致作为开发项处理（§9） |

## 2. 部署拓扑

架构图见 artifact「idoris.ai/api 架构」。这里写文字版，它是权威版本。

```
泰国学生/家长/老师 ──浏览器/App──▶ eduai 前端
                                     │（eduai 自己的登录会话；前端不持有 iDoris 凭证）
                                     ▼
                                 eduai 后端 ── 持有「服务凭证」，提交假名成员 id
                                     │  HTTPS
                                     ▼
┌──────────── idoris.ai/api（新加坡，单活动写节点）────────────┐
│ 入口：TLS 由反向代理负责 · /api/v1 前缀 · 从凭证推导租户    │
│ 控制面：开户/成员/凭证/额度/担保，与数据面分开，有审计      │
│ 数据面：准入 → 路由 → 尝试 → 发布边界 → 结算                │
│ 内容安全：edu 租户强制，输入 + 完整输出检查                 │
│ 账本（SQLite，WAL）：成本 / 售价 / 补贴 / 担保分开记账      │
└────┬──────────────────┬──────────────────┬───────────────┘
     ▼                  ▼                  ▼
 儿童许可的上游       成人许可的上游       P1：Modal 上的自定义模型
（端点级 ZDR + DPA）  （可以用 DeepSeek 官方）  （泰语语音、安全分类）
     ▲
     │ 云端档（节点主动调用云；云从不反向访问用户的 Mac）
┌────┴───────────────┐        ┌─────────────────────────────┐
│ 组织 iDoris（Mac Ultra）│        │ 个人 iDoris（用户 Mac，Agent24 托管）│
│ 局域网，TLS + 成员凭证   │        │ loopback，契约不变                 │
│ 本机模型 + 云端档        │        │ 本机模型 + 云端档                  │
└────────────────────┘        └──────────────▲──────────────┘
                                              │ loopback
                                         Agent24 桌面端
```

要点：
- **eduai 前端永远不持有 iDoris 凭证**（评审 B1）。P0 一律走 eduai 后端代理。只有将来需要浏览器直连时，才加后端签发的短期、限定用途凭证。
- **Agent24 永远只连本机的 iDoris**。这符合规范 R3 D-1「配置 IDORIS_URL 后由 iDoris 独占」。本机 iDoris 的**云端档**再去调 idoris.ai/api。所以云端档的适配器（S7）进 P0（评审 M4）。
- **节点主动调用云，云从不反向访问用户或组织的 Mac**。不做内网穿透，也不做远程调度。
- `local_only` 请求一律**失败即关闭**，不会因为预算不够或容量不够就自动出网。用户确认突破 1GB 余量，也不等于授权走云端。

## 3. 核心对象：身份、成员关系、付款、凭证分开

评审 H1 指出，一棵树扛不住身份、成员关系和付款三种关系，所以拆成下面几个对象：

| 对象 | 是什么 | 例子 |
|---|---|---|
| 主体 Principal | 一个稳定的假名 id，由开通方提供 | eduai 的学生 id 哈希、Agent24 云账号 |
| 成员关系 Membership | 主体属于哪个组织、什么角色。**一个主体可以有多个** | 学生属于学校 A；老师同时属于学校 A 和学校 B |
| 付款账户 Account | 承担费用的账户，组成**额度委派树**：平台 → 组织 / 担保人 → 成员子账户 | 学校 A、家长乙、平台补贴池 |
| 凭证 Credential | 调用凭据。**每次请求只能落到一个付款上下文** | eduai 服务凭证、组织节点凭证、成员子凭证 |
| 额度 Allocation | 父账户给子账户的是**授权上限**，不是转移余额。父子共用一份余额，所以不会被花两次 | 老师给学生每月 X |

- 一次请求的付款上下文由**凭证**决定，再加上开通方提交的、经过校验的成员 id。请求本身不能自己指定由谁付款。
- 本地数据密钥、云凭证、Admin token 三者分开。
- 云账号绑定走「登录 / 配对」流程。解绑、撤销、多设备行为都在 S2 里定义。

## 4. 计量单位

评审 H3：钱和配额混在一起时，必须有类型系统，不能直接相加。

- 每一笔金额都带 `(单位, 精度, 用途, 有效期, 可否转让=否)`。单位只有三种：`THB`、`USD`、`QUOTA:<节点>`。
- **精度**：货币用**微单位**（1e-6），不用分。一次小调用的成本远小于 1 分钱，按分向上取整会严重失真。对外展示时再四舍五入。
- 每次请求都记 `cost`（上游成本，带供应商的币种）和 `price`（对用户的价，带账户的币种）。需要换算时，记下汇率和取值时间。
- **组织配额**：`QUOTA` 的单位是「按本节点价目表折算的 token 当量」。本机模型的价格由组织自己设，用来公平分配算力。
- **免费额度**：平台补贴池（有总上限）按月给成员**授权**额度，发放带幂等键 `(成员, 月份)`。实际消耗时，补贴池记一笔支出，成员账户记一笔消耗，这样免费档每月花了多少一眼可见。
- 配额永远不能转让，也不能兑现（PGL 红线）。

## 5. 准入、信用与宽限

- **准入是原子的**：一次请求要沿付款账户**所有祖先**同时检查五项，任何一项不过就不放行：
  1. 余额，或授权额度；
  2. 授信下限；
  3. 速率闸门（分钟 / 小时花费上限）；
  4. 在途总额上限；
  5. 平台补贴池总上限（评审 H10）。
- **预留** = `max_tokens × 输出单价 + 输入估算`，并设硬性的 `max_tokens` 上限。余额不够时，先自动压低 `max_tokens`，或者降级到更便宜的模型。
- **有担保的负值**：只有担保人（家长 / 组织）开了授信，下级才能透支，最多透支到授信下限。
- **宽限资格单独计算**：每个成员每个周期最多 1 次，金额不超过一次预留。它不依赖 `settle` 的 overage（评审 B4）。
- **overage 照常记账，只是事实**。超出预留的部分，在担保人授信范围内的由担保人承担，超出授信的记入**平台损失账户**，告警并冻结这张凭证。预留过期后迟到的结算，同样按这条规则走。
- **体验分级**：
  1. 正常；
  2. 降级，同场景换便宜模型；
  3. 宽限一轮并通知担保人；
  4. 返回 402。
  每一级都有稳定的原因码（`budget_degraded` / `capacity_degraded` / `upstream_failover` / `grace_used` / `budget_exhausted`），浏览器能读到对应的响应头（评审 L2）。给孩子看的文案由 eduai 根据原因码来写。

## 6. 请求状态机与失败切换

评审 B3 指出，流式与失败切换没有结算闭环，所以定义如下状态机：

```
admitted ─▶ attempt[i]: sending ─▶ maybe_executed ─▶ executed(usage 已知 | usage 未知) ─▶ settled
                             └─▶ not_sent（可以安全地换下一个候选）
```

- 一次用户请求可以有多次**尝试**。每次尝试单独记录：供应商、模型、价格版本、usage、执行确定性、预留。
- 切换到下一个候选之前，**按新候选重新准入**。
- 落到 `maybe_executed` 的尝试，它的成本**保留待核销**：按预留额挂账，等供应商账单或 usage 回来再结算，不立即释放。
- **用户只付一次**，按最终给出回答的那次尝试计价。额外尝试的成本由**平台**承担，单独入账。
- **发布边界**：已经向客户端输出第一个字节之后，**禁止透明重试或切换**。
  - 流式：中途失败就以错误结束这条流，并按已产生的 usage 结算；usage 未知时按预留结算。
  - 流式 usage 来源：上游必须支持在流里返回 usage（`stream_options.include_usage`）。不支持的端点不能用来跑付费或补贴的流式请求。
- **P0 范围**：
  - **edu 一律非流式**（同时满足 §7 的完整输出检查）。
  - Agent24 可以流式，但只能走支持流式 usage 的端点。
  - 首字节之前的失败切换，进 P0。

## 7. 儿童安全与数据处理

**内容安全**（评审 B5）：
- edu 租户的安全策略绑定在**租户属性**上，不绑定在别名上。改用具体模型名也绕不过去。
- 流程：输入检查 → 上游调用（非流式）→ **完整输出检查通过后才发给孩子**。
- 安全服务超时或出错，一律**拒绝放行**。
- 工具调用的参数和返回的引用内容，同样要检查。P1 加语音时，转写文本也要检查。
- 安全检查的成本计入准入和结算。它的数据处理方同样要符合端点许可表。

**数据处理**（评审 H6、H8）：
- eduai 负责：身份、监护人同意、年龄核验。它要向 iDoris 提供**处理资格**和**撤销状态**：撤销后，相关凭证和成员立即失效。
- **财务担保人和同意主体是两个概念**：老师可以担保费用，但不能代替家长同意。
- **正文只在内存里处理**。edu 租户**关闭响应缓存**；幂等记录只存请求摘要和结算状态。
- 反向代理、错误采样、崩溃转储、安全分类器、Modal 日志都在「不落盘」的范围内。上线验收要扫描整条链路，不只看事件日志。
- 孩子仍可能在 prompt 里写进个人信息。所以上游端点必须满足 ZDR 和 DPA，而不是只靠假名化。

## 8. 上游：端点级许可表

评审 H7、M2、L1 指出 ZDR 不能是一个布尔标签。本表是唯一的权威路由约束，取代脑暴稿的 §2.4：

| 字段 | 说明 |
|---|---|
| 端点 | 供应商 + 模型 + 具体端点 |
| 处理区域 | 推理实际在哪里执行 |
| 留存 | ZDR / 有例外（比如 Safety Retention）/ 会留存 |
| DPA | 已签 / 未签 |
| 允许的数据类别 | `child_edu` / `adult` / `public` |
| 能力 | 工具调用、流式 usage、缓存计价、结构化输出、取消 |
| 核验时间 | 过期就自动停用 |

- 路由先过硬约束：租户的数据类别 ⊆ 端点允许的类别，区域和留存也要满足。兜底同样要过这些约束。**没有合规候选，就拒绝**。
- 云端**排除 subscription 路径**（它走的是 CLI）。
- **P0 候选，都待核实**：
  - `child_edu`：只用「ZDR + 已签 DPA」的端点。候选是 DeepInfra（官网说明基础设施在美国，跨境要写进隐私声明）和 OpenRouter 的 ZDR 路由（要确认账户设置和 Safety Retention 的影响）。**不用 DeepSeek 官方**。
  - `adult`（Agent24）：再加上 DeepSeek 官方。
- 模型选型以泰语 edu 评测集（S8）的结果为准，脑暴稿 §3 的模型表只是候选。

## 9. 三种形态与 4 处不一致（都作为开发项）

| | 个人 | 组织 | 云 idoris.ai/api |
|---|---|---|---|
| 跑在哪 | 用户 Mac，由 Agent24 托管 | 组织的 Mac Studio Ultra | 我们的服务器（新加坡） |
| 入口 | 只听 loopback（不变） | 局域网 + TLS + 成员凭证 | 公网 + TLS + 开通流程 |
| 单位 | 不计费 | `QUOTA` | `THB` / `USD` + 补贴额度 |
| 上游 | 本机模型 + 云端档 | 本机模型 + 云端档 | 许可表里的端点（+ P1 Modal） |
| 云端认谁 | 个人云账号 | **只认组织付款账户**；组织按节点或用途分别发受限凭证 | — |
| 账本权威 | 本机 | 组织内部的归因 | **云端成本以云账本为准** |

4 处不一致，加上评审补充的部分：
1. **写操作的管理接口（控制面）**：独立于 Admin v0。授权看「操作 × 资源范围」，比如「能给哪棵子树发多少额度」，**下发权限不能超过授权者本人**。启用 `admin_scopes`（签发时现在会拒绝，`key_issue.rs:122`）。所有写操作都有审计。组织局域网也**不默认互信**（评审 H5）。
2. **租户来源改成数据库为准**：一次性从配置文件初始化，之后以数据库为准。组织和云形态**不再在启动时用 `retain_tenants` 删租户**（`storage.rs:224`）。个人形态保持现状（评审 H4）。
3. **公网防滥用**：只在云形态加载。资格由开通方证明，IP 和设备只作辅助信号，避免学校共享出口误伤一整个班。
4. **容量**：组织节点排队，本机放不下就走云端档，复用现有的内存额度、驱逐 / 降级二选一、1GB 余量逻辑。

另外两点：
- **组织与云的对账**（评审 H2）：用尝试 ID 和不可变账目做幂等游标。离线的组织节点**不能自行增加可消费的云额度**。本地离线时只消耗 `QUOTA`。
- **数据面的租户**（评审 M1）：云入口从凭证推导租户，普通 OpenAI 客户端只要地址加密钥就能用。本地租户模式的 `X-iDoris-Tenant` 契约不变。

## 10. 运维边界（评审 H9）

- **P0 只有一个活动写节点**：持久卷上的 SQLite（WAL）。不做多活。
- **备份**：持续传送 WAL 到异地对象存储。目标 **RPO ≤ 1 分钟、RTO ≤ 30 分钟**。这叫可恢复，**不叫高可用**。
- **恢复后对账**：拿尝试表里 `maybe_executed` / `executed` 的记录和上游账单核对，防止重复结算或漏记。
- 压测场景：一个班级同时发起请求、账本锁等待、磁盘写满、进程崩溃、恢复后重复结算。目前能扛多少 QPS **还不知道**，以压测为准。

## 11. 经济性（按评审 M5 修订）

- 保留「P0 不租 GPU」。根据：按需 H100 每小时 $2.7–4，在低利用率下每百万输出 token 的成本比同级 API 高出数倍。这是**估算**，前提是「同模型、同质量、同延迟目标」，还没有实测。
- 每 50 个学生每月约 $20：只是给定轮数和 token 数下的推理成本估算，**还没算**两次安全检查、重试，以及泰语上下文变长带来的增量。用首个班级的真实数据来校准补贴池的上限。
- Modal 放 P1：$30 的月赠额不能当作稳定的成本基础。CPU、内存、冷启动都要算进去。

## 12. 实施切片（按评审 M3 重排，每片一个 PR 进 main，用配置开关隔离）

| 片 | 内容 | 依赖 |
|---|---|---|
| **S0** | 契约冻结：单位类型、主体 / 成员关系 / 付款 / 凭证模型、请求与尝试状态机、端点许可表的 schema、原因码 | — |
| **S8** | 泰语 edu 评测集 + 端点核验（区域、留存、DPA）。**从第一天开始，和其他片并行** | — |
| S1 | 账户存储以数据库为准 + 初始化迁移（去掉组织 / 云形态的 `retain_tenants` 删除行为）；**把真实的账户、凭证、模型接进预算**（取代 `DEFAULT_KEY_ID`） | S0 |
| S2 | 控制面：开户、成员关系、凭证、额度、担保；授权看「操作 × 资源范围」；审计；云账号配对 | S1 |
| S3 | 准入：祖先额度 / 授信 / 速率 / 在途 / 补贴池；有担保的负值与独立宽限；免费额度按幂等发放；降级原因码 | S1 |
| S4 | 云供应商契约：tools、流式 usage、尝试记录、首字节前切换、许可表硬约束；云端排除 subscription | S0、S8 的核验结果 |
| S5 | 云入口：从凭证推导租户、`/api/v1` 前缀、eduai 服务凭证与后端代理开通、平台 / 租户 / 成员三级上限 | S2、S3 |
| S6 | 儿童安全：租户属性绑定、edu 非流式完整输出检查、失败即拒绝、正文不落盘、edu 关缓存 | S4 |
| S7 | 本地 / 组织的云端档适配器（Agent24 → 本机 iDoris → 云），`local_only` 失败即关闭 | S4、S5 |

组织形态做完 S1–S3 就可以先在局域网里验证；儿童业务必须等 S6 和 S8 都完成，才能上线。

## 13. P0 上线验收（整条纵向路径，不能只看「返回了文本」）

- 开户 → 发放额度 → 调用 → 撤销 → 撤销后调用被拒。
- 同一个主体属于两个组织，每次请求的付款上下文都唯一且正确。
- 并发把额度用超：祖先限额、速率、补贴池上限都生效，不出现超发。
- 流中途断开；上游执行结果不确定；首字节前切换；首字节后禁止重放。
- 在线开户 → 重启 → 仍然可用；从备份恢复后不重复结算。
- edu 在安全服务超时时拒绝放行；改用具体模型名绕不过安全检查；全链路扫描确认没有正文落盘。
- Agent24 走本地 → 云端档；`local_only` 不出网。
- 一个真实的泰国班级 + Agent24 内测用户跑满一周，输出 cost / price / 补贴报表。

## 14. 对 Codex 评审的处理

| 条目 | 处理 | 在哪里 |
|---|---|---|
| B1 可信开通 | 接受：P0 走 eduai 后端代理，前端不持有凭证；幂等发放、撤销 | §2、§3、S2/S5 |
| B2 真实预算接线 | 接受，已核实 `DEFAULT_KEY_ID` | §3、S1 |
| B3 流式与切换 | 接受：尝试状态机；首字节后不重放；edu 非流式；用户只付一次，额外成本由平台承担 | §6、S4 |
| B4 宽限 ≠ overage | 接受：独立的宽限资格，加损失账户 | §5、S3 |
| B5 输出过滤边界 | 接受：完整输出检查后才发布，失败即拒绝，绑定租户属性 | §7、S6 |
| H1 对象拆分 | 接受 | §3 |
| H2 组织 / 云 | 接受：云端只认组织账户；幂等对账；离线不加云额度 | §9 |
| H3 单位 | 接受：带类型的金额，微单位精度，授权上限而非转移余额 | §4 |
| H4 `retain_tenants` | 接受，已核实 | §9、S1 |
| H5 控制面授权 | 接受，已核实 `admin_scopes` 会被拒绝 | §9、S2 |
| H6 儿童数据生命周期 | 接受。法律条款原文未核实，作为待办 | §7、§15 |
| H7 端点级许可 | 接受：删掉脑暴稿里「主要上游都有亚太节点」这句没核实过的说法 | §8 |
| H8 正文链路 | 接受，已核实代理缓存存的是完整 body | §7 |
| H9 SQLite 运维 | 接受：单写节点，写明 RPO / RTO | §10 |
| H10 补贴总上限 | 接受 | §4、§5 |
| M1 租户头 | 接受，已核实 | §9、S5 |
| M2 供应商契约 | 接受 | §8、S4 |
| M3 切片顺序 | 接受：加 S0，S8 第一天就开始 | §12 |
| M4 拓扑 | 接受：只保留节点主动调用云；Agent24 只连本机；云端档进 P0 | §2、S7 |
| M5 经济性措辞 | 接受，全部标注为估算 | §11 |
| L1 路由矩阵 | 接受：以许可表为准 | §8 |
| L2 原因码 | 接受 | §5 |

没有不接受的条目。我做的取舍只有一处：失败切换产生的额外成本，**由平台承担，不转嫁给用户**。理由是体验优先，并且这部分成本会在账本里单独可见。

## 15. 待核实（开发中并行推进，不阻塞 S0–S3）

- 泰国 PDPA 关于未成年人同意的现行条文和 2026 年修订；AI 法草案的进展（要有现行的权威文本，最好请当地律师看）。
- 各候选端点的 DPA、是否允许转售、是否适用于儿童业务、推理的实际区域。
- 云形态部署区域（新加坡）到各上游的实测延迟。
- Typhoon 等泰语模型的托管可用性和评测结果。
- SQLite 单写节点能承受的实际负载。

## 附录：实现时的暂定理解（待 owner 确认）

本附录只记录 `feat/compute-service` 上已经写进代码的做法。出处是各切片模块文档（`//!`）和对应实现。正文不改。与正文直接冲突的条目放在文末「与正文不一致」，编号仍按切片。

### S0 契约（`crates/idoris-compute-contracts`）

- **S0-01**：`QUOTA` 与货币一样用微单位；对外展示取 0 位小数（整数个 token 当量），货币取 2 位，半值远离零。代码：`crates/idoris-compute-contracts/src/money.rs:66-71`、`src/lib.rs:98`。设计稿 §4。
- **S0-02**：额度有效期必填，半开区间 `[from, until)`，没有「永不过期」。代码：`crates/idoris-compute-contracts/src/grant.rs:7-8`。设计稿 §4。
- **S0-03**：账户树父子必须同一单位；最大深度 5（根为 0）。代码：`crates/idoris-compute-contracts/src/account.rs:14-15`、`src/account.rs:41-44`。设计稿 §3。
- **S0-04**：单笔下发不超过授权者本人；同一父账户下各子额度之和可以超过父额度。代码：`crates/idoris-compute-contracts/src/account.rs:24-29`。设计稿 §3。
- **S0-05**：免费额度幂等键是 `subsidy:<主体 id>:<YYYY-MM>`，成员取主体 id，不是成员关系 id。代码：`crates/idoris-compute-contracts/src/account.rs:602-611`。设计稿 §4。
- **S0-06**：「钱够不够」= `Funds` 通过，或 `Funds` 失败但被授信救回；宽限只在钱不够、且账户尚未透支、缺口不超过一次预留时可用。谁承担宽限缺口，S0 留给 S3。代码：`crates/idoris-compute-contracts/src/admission.rs:14-16`、`src/admission.rs:642-670`。设计稿 §5。
- **S0-07**：体验分级第四级的原因码是 `budget_exhausted`；既有 402 体仍用 `budget_exceeded`。两个字符串并存。云聊天入口的 402 用 `budget_exhausted`。代码：`crates/idoris-compute-contracts/src/reason.rs:30-34`、`crates/idoris-tenancy/src/budget/error.rs:10`、`crates/idoris-router/src/cloud/handler.rs:22`。设计稿 §5。
- **S0-08**：一次请求最多 4 次尝试；没有任何尝试发布时用户付 0。代码：`crates/idoris-compute-contracts/src/request.rs:17-18`、`src/request.rs:52-54`。设计稿 §6。
- **S0-09**：从 `maybe_executed` 可以在首字节前换下一个候选。S0 只定义最终结算，不定义上一次尝试的价预留何时释放（释放见 S3-11）。代码：`crates/idoris-compute-contracts/src/lib.rs:108-109`。设计稿 §6。
- **S0-10**：核验有效期是表级 `verification_max_age_days`（1..=365）；没有 `enabled` 字段，从表里删掉即停用；处理区域必须全部被租户允许；区域是供应商无关的小写标签。代码：`crates/idoris-compute-contracts/src/permit.rs:10`、`src/permit.rs:67-72`、`src/lib.rs:111-113`。设计稿 §8。
- **S0-11**：没核实的许可字段写成最坏值（`allowed_data_classes: []`、`retained`、`unsigned`、能力 `false`、区域 `unknown`）；空的数据类别列表合法，这样的端点不会被选中。代码：`crates/idoris-compute-contracts/src/permit.rs:46-58`。设计稿 §8。
- **S0-12**：`deploy_mode=tenant` 且没有 `form` 时，契约解析成组织形态；云形态补贴按货币单位加 `FreeTier` 用途，不是 `QUOTA`。启动是否照此删租户见 S1-02。代码：`crates/idoris-compute-contracts/src/form.rs:16`、`src/form.rs:64-65`、`src/grant.rs:23-30`。设计稿 §9。
- **S0-13**：每次尝试另有可选的供应商请求号，供恢复后对账，不参与状态转移。代码：`crates/idoris-compute-contracts/src/lib.rs:119-120`。设计稿 §6、§10。

### S1 账户存储与启动

- **S1-01**：账户表只在显式 `IDORIS_FORM=organization` 或 `cloud` 时创建。个人形态和没写 `IDORIS_FORM` 的存量部署不建这些表。代码：`crates/idoris-tenancy/src/accounts/mod.rs:9`、`crates/idoris-router/src/startup_form.rs:18-23`。设计稿 §9、§12 的 S1。

### S2 控制面与配对

- **S2-01**：配对码有效期默认 10 分钟，允许范围 30 秒到 15 分钟；过期不改行，读取时按时间判定。代码：`crates/idoris-tenancy/src/accounts/pairing.rs:57-59`、`src/accounts/pairing.rs:68`。设计稿 §3。
- **S2-02**：同一个成员关系最多 10 台未撤销的设备凭证。代码：`crates/idoris-tenancy/src/accounts/pairing.rs:61-63`。设计稿 §3。
- **S2-03**：创建配对码时不写 `device_ttl_days`，设备凭证不过期；若签发者自己有有效期，设备凭证不晚于它。代码：`crates/idoris-router/src/control/wire.rs:135-136`、`crates/idoris-tenancy/src/accounts/pairing.rs:516-529`。设计稿 §3。
- **S2-04**：`/control/v1` 挂在数据面同一个监听上，只在显式的组织或云形态挂载；请求来源不参与授权。代码：`crates/idoris-router/src/control/mod.rs:8-26`。设计稿 §9 第 1 项。
- **S2-05**：读操作不写审计；认不出的密钥也不写审计。代码：`crates/idoris-tenancy/src/accounts/control_reads.rs:7`、`crates/idoris-router/src/control/mod.rs:29-32`。设计稿 §9。
- **S2-06**：平台根上的开户、额度、第一张管理凭证只走本机 `idoris control bootstrap`；没有任何一张云凭证能管到平台根。代码：`crates/idoris-router/src/control/bootstrap.rs:6-12`。设计稿 §9。

### S3 准入账本

- **S3-01**：宽限那一轮里，账户可用额度加担保人授信之内的部分按普通请求记账；超出这个下限的部分（至多一次预留）记 `grace_absorbed`，由平台承担，不冻结凭证。代码：`crates/idoris-tenancy/src/accounts/ledger/mod.rs:69-71`、`src/accounts/ledger/holds.rs:518-519`。设计稿 §5。
- **S3-02**：宽限周期是账本时区的自然月。宽限放行的预留最后没有向用户收费，这次宽限作废，本周期还可以再用。代码：`crates/idoris-tenancy/src/accounts/ledger/mod.rs:73-74`、`src/accounts/ledger/holds.rs:478`。设计稿 §5。
- **S3-03**：从未充过值、也从未有过 `free_tier` / `prepaid` / `org_quota` 的账户不给宽限。代码：`crates/idoris-tenancy/src/accounts/ledger/admit.rs:388-391`。设计稿 §5。
- **S3-04**：补贴池总上限按账期；没设过的账期上限是 0。代码：`crates/idoris-tenancy/src/accounts/ledger/mod.rs:75`、`src/accounts/ledger/project.rs:12`。设计稿 §4、§5。
- **S3-05**：授权额度剩余 = 此刻有效的花费类额度之和，减去从最早有效起点算起的消耗。多个有效额度相加。代码：`crates/idoris-tenancy/src/accounts/ledger/project.rs:8`。设计稿 §3。
- **S3-06**：速率闸门是滑动窗口：最近 60 秒、最近 3600 秒，不是日历桶。代码：`crates/idoris-tenancy/src/accounts/ledger/project.rs:10`、`src/accounts/ledger/project.rs:33-35`。设计稿 §5。
- **S3-07**：overage 里担保人还能吸收多少，取整条链「可用额度 + 授信」的最小值。速率、在途、补贴池上限不算进这笔。代码：`crates/idoris-tenancy/src/accounts/ledger/project.rs:322-326`。设计稿 §5。
- **S3-08**：钱不够同时又被速率等挡住时，拒绝码取 `blocking_codes` 的第一个。原因码按声明顺序，`budget_exhausted` 在 `rate_limited` 之前。代码：`crates/idoris-tenancy/src/accounts/ledger/admit.rs:309-315`、`crates/idoris-compute-contracts/src/admission.rs:245-252`、`crates/idoris-compute-contracts/src/reason.rs:44-60`。设计稿 §5。
- **S3-09**：账期时区的库默认值是 `UTC`。类型注释写明泰国首发应改成 `Asia/Bangkok`。代码：`crates/idoris-tenancy/src/accounts/ledger/migrations/0001_ledger.sql:32`、`src/accounts/ledger/types.rs:23`。设计稿 §5 没写时区。
- **S3-10**：预留 TTL 的库默认值是 `1800000` 毫秒。代码：`crates/idoris-tenancy/src/accounts/ledger/migrations/0001_ledger.sql:32`。设计稿 §5。
- **S3-11**：切换候选时，新候选准入的同一个事务里先释放这个请求此前失败候选占着的价预留。代码：`crates/idoris-tenancy/src/accounts/ledger/admit.rs:217-226`。设计稿 §6。
- **S3-12**：除平台根和补贴池外，既没充值也从没发过额度的账户，授权额度剩余是 0，不是不设限。代码：`crates/idoris-tenancy/src/accounts/ledger/project.rs:14-16`。设计稿 §5。

### S4 云上游与尝试记录

- **S4-01**：`reqwest` 的 `send` 一开始就按「字节可能已经出去」处理。连接被拒、DNS、TLS 失败保持 `maybe_executed`，失败标注 `connect_failed`。`never_left_host` 只用于 `prepare` 阶段的本地失败。代码：`crates/idoris-router/src/cloud/mod.rs:48-52`。设计稿 §6。
- **S4-02**：所有 4xx（含 408、429）当作没执行，进 `not_sent`。2xx 之后的失败（坏响应体、流里报错、首个事件前断开）当作已执行且 usage 未知。代码：`crates/idoris-router/src/cloud/mod.rs:53-54`。设计稿 §6。
- **S4-03**：首字节前，400 / 413 / 422 以及其余未点名的 4xx 不切换候选；401、402、403、404、408、425、429 以及 3xx、5xx 会切换。代码：`crates/idoris-router/src/cloud/executor.rs:258-269`。设计稿 §6。
- **S4-04**：usage 结构不对、`total_tokens` 对不上、缓存大于输入、输出超过发给上游的 `max_tokens`、金额溢出，都当 usage 未知，用户按预留价付。代码：`crates/idoris-router/src/cloud/mod.rs:57-58`。设计稿 §6。
- **S4-05**：首字节前客户端取消，尝试保持 `maybe_executed`。首字节后断开：端点有取消能力则记已执行且 usage 未知，没有则保持 `maybe_executed`。两种情况用户都按预留价付。代码：`crates/idoris-router/src/cloud/mod.rs:59-61`。设计稿 §6。
- **S4-06**：供应商请求号优先取响应正文的 `id`，其次是配置的响应头。格式不对、重复、流中途换号都留标注，不因此拒绝服务。代码：`crates/idoris-router/src/cloud/mod.rs:62-63`。设计稿 §6、§10。
- **S4-07**：结算分录分 `user_price`、`platform_cost`、`hold`、`release`。零金额不记。一个请求至多一条正的 `user_price`。代码：`crates/idoris-router/src/cloud/mod.rs:64-66`。设计稿 §6。
- **S4-08**：请求记录里还有 S0 原因码之外的稳定字符串，例如 `upstream_unavailable`、`client_cancelled`、`upstream_stream_failed`；HTTP `error.type` 还有 `upstream_error`、`streaming_not_allowed` 等。代码：`crates/idoris-router/src/cloud/mod.rs:67-69`。设计稿 §5、§6。
- **S4-09**：`child_edu` 的流式请求在任何上游调用之前拒绝，HTTP 400，`streaming_not_allowed`。代码：`crates/idoris-router/src/cloud/handler.rs:30-31`。设计稿 §6。
- **S4-10**：已经有尝试失败、后一个候选又被准入拒绝时，请求以 502 结束，不是 402。代码：`crates/idoris-router/src/cloud/mod.rs:72`。设计稿 §5。
- **S4-11**：云形态拒绝 `QUOTA` 付款单位。代码：`crates/idoris-router/src/cloud/ingress/caller.rs:127-129`、`crates/idoris-router/src/cloud/config.rs:586-588`。设计稿 §9。
- **S4-12**：体验分级响应头的名字是 `X-iDoris-Reason`、`X-iDoris-Tier`、`X-iDoris-Max-Tokens`，并列入 CORS 暴露头。代码：`crates/idoris-router/src/cloud/handler.rs:18-22`、`src/cloud/handler.rs:169`。设计稿 §5。
- **S4-13**：没有合规候选时 HTTP 503，原因码 `no_compliant_endpoint`。代码：`crates/idoris-router/src/cloud/handler.rs:21`。设计稿 §8。

### S5 云入口

- **S5-01**：租户取付款账户祖先链里、平台根之下、跳过补贴池之后的顶层客户。个人云账户直接挂在平台根下时，每个账户自己就是一个租户。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:76-78`、`src/cloud/ingress/mod.rs:94-95`。设计稿 §9。
- **S5-02**：租户的数据类别、留存、DPA 要求和计费性质写在云入口配置的 `tenants:` 里。没声明的租户拒绝。网络接口不能改这些字段。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:79-82`。设计稿 §7、§9。
- **S5-03**：Service 凭证每个数据面请求都要带 `X-iDoris-Member`，包括 `GET /api/v1/models`。成员凭证带了必须是自己。节点凭证不能带。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:53-56`、`src/cloud/ingress/mod.rs:83-84`、`src/cloud/ingress/mod.rs:133-135`。设计稿 §3、§9。
- **S5-04**：请求个数和并发上限用入口自己的原因码，不复用 S0 的 `rate_limited` / `in_flight_limited`。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:85-86`。设计稿 §5、§9。
- **S5-05**：凭证不认识、撤销、冻结、过期是不透明的 401。凭证有效但这次付款上下文建不起来，是带 S0 原因码的 403。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:87-88`。设计稿 §3。
- **S5-06**：来源地址只给没有凭证的请求分软配额，不按纯 IP 封禁。带有效凭证的请求不被来源地址拦住。失败计数按凭证前缀。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:89-90`。设计稿 §9 第 3 项。
- **S5-07**：平台 / 租户 / 成员的请求数和并发上限在进程内存里，重启清零，不做多节点共享。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:91`。设计稿 §9、§10。
- **S5-08**：慢速请求头超时交给反向代理。本进程只对已经通过认证和限流的请求做请求体读取期限。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:92-93`。设计稿 §2。
- **S5-09**：云形态数据面不挂载 `/v1/*`。旧推理路由在云入口上是 404，不是 403。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:36-40`、`src/cloud/ingress/mod.rs:96-100`。设计稿 §9。
- **S5-10**：`/control/v1` 默认不挂在公网监听上。配置 `control_plane.public: true` 才挂在同一个公网监听上，并且不发 CORS 头。代码：`crates/idoris-router/src/cloud/ingress/mod.rs:42-49`。设计稿 §2、§9。

### S6 儿童安全

- **S6-01**：是否走儿童管线只看 `TenantDataPolicy` 是否含 `child_edu`。模型名、别名、请求头和请求体改不了它。生产环境这份策略来自云入口配置的 `tenants:`。代码：`crates/idoris-router/src/cloud/mod.rs:80-83`。设计稿 §7。
- **S6-02**：管线在执行器里。儿童请求的回答没有「输出检查放行」标记就不能发布。代码：`crates/idoris-router/src/cloud/mod.rs:84-86`。设计稿 §7。
- **S6-03**：edu 的响应体是重建的，只保留检查过的字段。非 edu 的响应体原样转发。代码：`crates/idoris-router/src/cloud/mod.rs:87-88`。设计稿 §7。
- **S6-04**：输出被拒后不换候选重试。用户付 0，上游成本由平台承担。代码：`crates/idoris-router/src/cloud/mod.rs:89-90`、`crates/idoris-compute-contracts/src/safety.rs:18`。设计稿 §6、§7。
- **S6-05**：`safety_input_blocked` 和 `safety_output_blocked` 用 HTTP 422；`safety_unavailable` 用 503。代码：`crates/idoris-router/src/cloud/mod.rs:91-92`、`src/cloud/handler.rs:36-38`。设计稿 §7。
- **S6-06**：拦截类别只出现在当次响应里，不写入请求记录、日志或审计。记录里只有 `safety_input_blocked` 这一类粗码。代码：`crates/idoris-router/src/cloud/mod.rs:93-94`、`crates/idoris-compute-contracts/src/safety.rs:25-26`。设计稿 §7。
- **S6-07**：系统角色的文本只查越狱类规则。孩子的话若放在 `system` 里，规则检查器也只按越狱类查；LLM 检查器仍查全部。代码：`crates/idoris-router/src/cloud/mod.rs:95-97`、`crates/idoris-router/src/cloud/safety/rules.rs:14-16`。设计稿 §7。
- **S6-08**：`safety.mode` 必填。`rules_only` 可以启动，并打一行警告。代码没有强制 `child_edu` 必须配 LLM 检查器。代码：`crates/idoris-router/src/cloud/safety/config.rs:22-23`、`crates/idoris-router/src/cloud/mod.rs:98-99`。设计稿 §7。
- **S6-09**：给出结论的检查（放行或拦截）按实际用量向付款账户收费，用量未知按预留额，并且封顶在预留额。规则检查器直接拦下、以及超时、出错、返回无法解析、客户端断开，收费 0。代码：`crates/idoris-router/src/cloud/mod.rs:113-116`。设计稿 §7。
- **S6-10**：检查端点的供应商成本（上游币种）不记入账本。账本只记付款账户一侧的价。代码：`crates/idoris-router/src/cloud/mod.rs:115-116`。设计稿 §4、§7。
- **S6-11**：预留发生在规则检查之前。额度不足时，有害输入返回准入原因码（402 / 429 / 403），不是 422。规则拦下之后释放这次预留，不收费。代码：`crates/idoris-router/src/cloud/mod.rs:121-122`、`src/cloud/handler.rs:42`。设计稿 §5、§7。
- **S6-12**：非文本内容按 `unsupported_content` 拒绝。超过检查上限的文本按 `safety_unavailable`（`too_large`）拒绝。代码：`crates/idoris-router/src/cloud/mod.rs:106-107`。设计稿 §7。
- **S6-13**：在没有云入口的 `build_app` 上，`/v1/messages`、`/v1/embeddings`、`/v1/rerank` 对儿童租户返回 403。云入口形态下这些路由不存在（见 S5-09）。代码：`crates/idoris-router/src/cloud/mod.rs:108-111`。设计稿 §7。
- **S6-14**：规则检查器把含 `suicide` 的句子算作自伤，包括 “Can you explain suicide prevention?”。测试把这类误拒列出来，不把它当成放行。代码：`crates/idoris-router/src/cloud/safety/rules.rs:200`、`crates/idoris-router/src/cloud/safety/rules/tests.rs:470-476`。设计稿 §7。

### S7 云端档

- **S7-01**：云端档是回落时临时合成的组件卡，不放进 `AppState::cards`，不和本机候选一起竞价。代码：`crates/idoris-router/src/cloud_tier/mod.rs:73-76`。设计稿 §9 第 4 项。
- **S7-02**：每次回落都要带 `X-iDoris-Cloud-Fallback: allow`，不记忆、不复用 `X-iDoris-Fallback`。代码：`crates/idoris-router/src/cloud_tier/mod.rs:39-45`、`src/cloud_tier/mod.rs:77`。设计稿 §2、§13。
- **S7-03**：本机只记云端报告的 token 数，不记金额，也不写本机用量表。代码：`crates/idoris-router/src/cloud_tier/mod.rs:53`、`src/cloud_tier/mod.rs:61-64`、`src/cloud_tier/mod.rs:78-79`。设计稿 §9。
- **S7-04**：配对兑换拿到的是成员关系 id 和凭证 id。`idoris cloud status` 显示成员关系 id，凭证 id 只显示前缀。代码：`crates/idoris-router/src/cloud_tier/mod.rs:80-81`。设计稿 §3。
- **S7-05**：缺省控制面地址是数据面源加上 `/control/v1`，可以改，但必须和数据面同源。代码：`crates/idoris-router/src/cloud_tier/mod.rs:82-83`。设计稿 §2。
- **S7-06**：没声明隐私、被内容检查钉在本地、或远程熔断的请求，都按不能出网处理。契约里的隐私类别仍然只有 `local_only` 和 `any`。代码：`crates/idoris-router/src/cloud_tier/mod.rs:84-85`。设计稿 §2。
- **S7-07**：本机运行时把 400 和 500 折成同一个 `Upstream` 错误。有同意时，这类失败也会把同一请求发去云端。代码：`crates/idoris-router/src/cloud_tier/mod.rs:86-87`。设计稿 §9 第 4 项。
- **S7-08**：resident `http_service` 和订阅中转的失败不触发云端档。代码：`crates/idoris-router/src/cloud_tier/mod.rs:88-89`。设计稿 §8、§9。
- **S7-09**：同一个 `X-iDoris-Request-Id` 重试云端档时，每次都是新的云端请求，不复用本机 60 秒幂等缓存。代码：`crates/idoris-router/src/cloud_tier/mod.rs:90-91`。设计稿 §7、§9。
- **S7-10**：个人形态只用已配对的设备凭证；组织形态只用节点凭证文件或环境变量。没声明 `IDORIS_FORM` 的存量 tenant 部署不能启用云端档。代码：`crates/idoris-router/src/cloud_tier/mod.rs:92-93`。设计稿 §9。
- **S7-11**：流式请求不回落。并发槽用 `try_acquire`，满了记 `saturated` 并返回本机原来的失败，不排队。代码：`crates/idoris-router/src/cloud_tier/gate.rs:214-215`、`crates/idoris-router/src/cloud_tier/mod.rs:63`、`src/cloud_tier/mod.rs:700-701`。设计稿 §6、§9 第 4 项。

### S8 许可表与评测材料

- **S8-01**：正式许可表的 `verification_max_age_days` 取 schema 示例里的 90。代码与表：`config/compute/endpoint-permit-table.yaml:16-21`、`crates/idoris-compute-contracts/src/permit.rs:10`。设计稿 §8。
- **S8-02**：正式表里 `dpa` 全部是 `unsigned`，`allowed_data_classes` 全部是空列表，因此没有任何一行带 `child_edu`。空类别的端点不会被 `select_compliant` 选中。代码与表：`config/compute/endpoint-permit-table.yaml:9-14`、`crates/idoris-compute-contracts/src/permit.rs:51-53`。设计稿 §8。

### 与正文不一致

- **S0-14**：正文 §6 的图只画了 `sending → not_sent`。代码还允许 `admitted → not_sent`（事件 `never_left_host`），以及 HTTP 4xx 的 `upstream_refused` 从 `sending` 或 `maybe_executed` 进入 `not_sent`。5xx 和超时不能走到 `not_sent`。代码：`crates/idoris-compute-contracts/src/attempt.rs:24-34`、`src/lib.rs:106-107`。
- **S0-15**：正文 §3 把额度委派树写成「平台 → 组织 / 担保人 → 成员子账户」。代码还允许平台直接把成员挂在自己下面，并允许补贴池作为组织、担保人或成员的上级。代码：`crates/idoris-compute-contracts/src/account.rs:63-69`。
- **S0-16**：正文 §5 写每个成员每个周期最多 1 次宽限。计数键是付款账户 id 加账期，不是主体 id，也不是成员关系 id。代码：`crates/idoris-compute-contracts/src/admission.rs:614-619`、`crates/idoris-tenancy/src/accounts/ledger/admit.rs:393`。
- **S1-02**：正文 §9 第 2 项写组织和云形态不再在启动时用 `retain_tenants` 删租户。S0 又把没写 `form` 的 `deploy_mode=tenant` 解析成组织，组织的能力表是不删（S0-12）。启动代码只在显式写了 `IDORIS_FORM` 时才照能力表办；没写时，存量 tenant 部署仍然删除配置里没有的租户。代码：`crates/idoris-compute-contracts/src/form.rs:169-176`、`crates/idoris-compute-contracts/src/form.rs:226`、`crates/idoris-router/src/startup_form.rs:10-16`、`src/startup_form.rs:86-93`。
- **S3-13**：正文 §5 第 3 级写「宽限一轮并通知担保人」。代码在账本里插入一条 `grace_used` 告警，带上父账户作为 `guarantor`，供查询。没有外发通知。代码：`crates/idoris-tenancy/src/accounts/ledger/admit.rs:436-464`、`crates/idoris-router/src/cloud/handler.rs:19`。
