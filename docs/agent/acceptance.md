# iDoris 统一模型服务 · 验收标准（用户视角）

> ⚠️ **已废弃（2026-09-27）**：本文内容已统筹进 [`iDoris 总体规划`](../iDoris-总体规划.md)，**冲突时以总体规划为准**。保留作决策追溯，不再更新。

> 这是「做成了没有」的标准，不是技术任务清单（技术拆解见 [`tasks.md`](tasks.md)）。
> 判断依据只有一条：**这些事情能不能真的替我做掉，且我敢让它替我做。**
> 记录日期：2026-09-07

## B3 task27 · 真实订阅 CLI smoke（Rust-only）

> 本节是 B3 Rust 迁移的显式 opt-in 实机验收记录；本文其余旧状态仍按顶部“已废弃”说明处理。

运行：

```bash
IDORIS_SUBSCRIPTION_REAL_CLI=codex cargo test -p idoris-upstream --test subscription_real -- --nocapture
IDORIS_SUBSCRIPTION_REAL_CLI=claude cargo test -p idoris-upstream --test subscription_real -- --nocapture
# 或一次跑两套：
IDORIS_SUBSCRIPTION_REAL_CLI=both cargo test -p idoris-upstream --test subscription_real -- --nocapture
```

规则：

- 默认不访问真实订阅账号，也不使用 `#[ignore]`；只有显式设置 opt-in 才运行。
- 一旦 opt-in，缺 CLI、未登录、版本/安全 flags 不兼容、真实调用失败都必须使测试失败；禁止 SKIP 冒充通过。
- 测试只记录 CLI 版本与通过项；失败断言也不得回显模型自由文本。relay 错误仍只暴露固定 reason code/白名单 diagnostics。
- 每套 CLI 必须验证：固定回复；工具/写入诱导下未出现禁写文件；真实 timeout；真实 CLI 进程组建立后的显式 cancellation；正常/timeout/cancel 后其独占 PID/PGID 都消失。该 smoke 不把“未出现禁写文件”夸大成“证明模型一定尝试了写入”，也不把 marker 后立即 cancel 夸大成“已取消一段确认在生成中的模型输出”。
- wrapper 只先记录自己的 PID/PGID 然后 `exec` 真实 CLI，参数原样透传；它不是 fake CLI，也不会改变 iDoris 的固定安全 argv。
- 真实 CLI 版本不兼容时应拒绝开放该 CLI，而不是剥掉安全 flag 重试。

2026-10-05 本机实测（Mac，真实登录态）：

- Codex CLI `0.156.1`：PASS —— 固定回复、write-inducement 下未出现禁写文件、500ms timeout、PGID 建立后的显式 cancel、正常/timeout/cancel 后 PGID 消失。
- Claude Code `2.1.289`：PASS —— 固定回复、write-inducement 下未出现禁写文件、500ms timeout、PGID 建立后的显式 cancel、正常/timeout/cancel 后 PGID 消失。
- 测试输出仅记录版本与 PASS；未记录 prompt、stderr、凭据或自由文本。

### B3 kill switch 运维验收

订阅中转不是热开关。紧急停用时：

1. 设置 `IDORIS_DISABLE_SUBSCRIPTION=1`。
2. 优雅重启 iDoris；shutdown 必须先拒绝新 relay、取消 active CLI、等待 PGID 回收。
3. 重启后确认 `/v1/models` 无 `claude-subscription` / `codex-subscription`。
4. 再发一个普通本地/免费 HTTP 模型正控，确认能力②/③不受 kill switch 影响。
5. 发一个 `local_only` 请求并确认 CLI spawn marker 为 0。

若 CLI 版本升级后 task27 的 safety flags / fixed reply / write-inducement /
timeout / cancel 任一项失败，该 CLI 必须保持禁用，禁止删安全 flag 后重试。

## 一、我只需要记住一个地址

我（或我的任何业务：Agent24 / 微信 bridge / blog 脚本 / banner 生成）只配置 `http://127.0.0.1:PORT/v1` 一个 OpenAI-compat 地址，就能用到本机所有 AI 能力，不用关心背后是本地模型、我的 Claude 订阅、还是某个云 API。

| 子项 | 要求 | 当前状态 |
|:---|:---|:---|
| 统一 URL 可用 | `curl /v1/models` 返回本机可用模型清单 | ⏳ T1.3.1 |
| OpenAI-compat 保真 | 标准 openai SDK 不改代码即可调通（含 streaming）| ⏳ T1.3.2 |
| 换后端不改调用方 | 换掉 oMLX / 换掉核心模型，业务代码零改动 | ⏳ T1.2.3 |
| 远程同址 | Mac mini + Tailscale 后，从别处访问同一个 URL 行为一致 | ⏳ M2 |

## 二、我敢把隐私数据交给它

标了「本地处理」的任务，**在任何情况下都不会**被送去云端——包括本地模型 OOM、崩溃、超时这些「顺手降级」最容易出事的时刻。

| 子项 | 要求 | 当前状态 |
|:---|:---|:---|
| LocalOnly 硬隔离 | `X-iDoris-Privacy: local_only` 的请求，本地不可用时**报错**而非转外部 | ⏳ T1.3.3 |
| 组件卡门禁 | 缺 `privacy_class`/`allowed_egress`/`fail_closed` 字段的组件**拒绝注册** | ⏳ T1.1.3 |
| 订阅不外借 | 能力①只绑 loopback + 单用户；社区/城市端配置下**无法启用** | ⏳ T1.4.2 |
| 可审计 | 每次路由决策留下「选了谁、为什么、是否降级」的记录 | ⏳ T2.2.3 |

## 三、24GB 的机器上它自己会安排内存

我不用手动决定「现在该卸哪个模型」。它知道我的机器有多少内存，自己算得出常驻放什么、临时能同时挂几个、什么时候必须驱逐，并且**提前告诉业务**而不是等 OOM。

| 子项 | 要求 | 当前状态 |
|:---|:---|:---|
| 硬件自动探测 | 首次启动无需配置即给出推荐组合 | ⏳ T2.1.1 |
| 推荐有理由 | 输出附可读 tradeoff 说明，不是黑箱打分 | ⏳ T2.1.3 |
| 容量可查询 | `GET /capabilities` 告诉业务 `admission_status: ready\|requires_eviction\|blocked` | ⏳ T2.2.1 |
| 可 override | `IDORIS_CORE_MODEL` 强制指定时推荐模块让路 | ⏳ T2.1.2 |

## 四、作为组织大脑，租户之间彼此看不见

我把 iDoris 部署成组织的大脑，为多个客户/部门托管。每个租户的用量、预算、审计**互不可见、互不影响**；我敢把两个竞争关系的客户放在同一个实例上。

| 子项 | 要求 | 当前状态 |
|:---|:---|:---|
| 租户硬隔离 | A 租户查不到 B 租户的**任何一条**用量/审计记录 | ⏳ T1.5.3 |
| 预算互不影响 | A 租户预算耗尽，B 租户照常调用 | ⏳ T1.5.2 |
| 预算是拒绝不是降级 | 超预算 → 明确报错且**不产生任何计费调用**，错误里说清「是预算不是故障」 | ⏳ T1.5.2 |
| 超预算仍可用本地 | `scope=paid_only` 时，零成本本地模型不被预算闸住 | ⏳ T1.5.2 |
| 每次决策答得出为什么 | 审计里 `reason` 非空，可区分 隐私强制/预算/意图匹配/降级 | ⏳ T1.5.4 |
| 账单不随机器变 | 月度聚合用租户显式时区，换台机器部署结果一致 | ⏳ T2.6.1 |
| 订阅红线不松动 | `deploy_mode=tenant` 下订阅中转**拒绝注册**，启动即报错 | ⏳ T1.4.2 |

## 五、用得越久它越懂我（M3，尚未开工）

| 子项 | 要求 | 当前状态 |
|:---|:---|:---|
| 本地自增长闭环 | 使用 → 数据湖 → 提炼 → MLX-LoRA → 热挂载，全程不出设备 | ⏳ M3 |
| adapter 不会串味 | base/tokenizer 指纹不匹配的 adapter **拒绝挂载/聚合** | ⏳ T3.2.1 |
| 联邦只传 LoRA | 个人↔社区联邦全程不传原始数据 | ⏳ M3 |
| 真实数据门禁 | 隐私层未就位时，真实个人数据**无法**进入联邦 | ⏳ M3 |

## 验收时怎么判断「好用」

1. **一个地址够用**：把 Agent24 的 `IDORIS_URL` 指过来，原有功能零回归，且能用到本地模型（`ModelRouter` 的 provider 列表里出现 iDoris）。
2. **隐私不靠自觉**：人为把本地模型全部停掉，再发 10 条 `local_only` 请求，**10 条全部报错，0 条落到外部** —— 一条漏出即判不合格。
3. **内存不失控**：在 `--memory-guard-gb` 模拟的 24GB 预算下连续跑 vision/coding/chat 混合负载 30 分钟，**进程不 OOM、不被系统 kill**，且每次切换有明确的 load/evict 日志。
4. **换件不塌方**：把能力③实现从 oMLX 换成 Ollama（同 LoadPolicy 契约），黄金一致性测试全绿，业务侧零改动。
5. **租户之间是墙不是筛子**：造两个 tenant，用 A 的上下文去查 B 的用量/审计，**一条也查不到**；再把 tenant 上下文整个去掉去查，**直接报错**而不是返回全量。
6. **账单可复现**：同一批用量数据在三个不同时区的机器上聚合，月度结果完全一致。
7. **不出事故**：没有 LocalOnly 外泄、没有订阅被非本机调用、没有跨租户数据泄漏、没有静默升级破坏已有 adapter。

## 待补能力清单（按这份标准倒推）

| 能力 | 优先级 | 已有参考 |
|:---|:---|:---|
| ProviderDescriptor / 组件卡校验器 | 高 | [`../06-组件接口契约与互换标准.md`](../06-组件接口契约与互换标准.md) §10.1/10.2 |
| LoadPolicy 抽象 + oMLX 适配器 | 高 | 06 §10.3；U0 实测的 oMLX knob 映射表 |
| 控制面 header 解析 | 高 | 06 §10.5；Agent24 `TaskProfile` |
| 声明式 routing policy 引擎 | 高 | 06 §10.6 |
| 订阅中转薄封装 | 中 | agent-cli-to-api 模式；U0 已验证 `claude -p` / `codex exec` |
| TenantContext 契约 + 租户隔离存储 | **最高** | 下游 iDoris-website 已挂起等此契约；其 `routing.py`/`audit.py`/`egress_guard.py` 可整体移交（Apache-2.0，含变异测试）|
| HardwareAwareModelRecommender | 中 | [`../07-模型量化内存评估与动态推荐.md`](../07-模型量化内存评估与动态推荐.md) §5 完整伪代码 |
| 语义意图路由 | 中 | [`../10-入口路由模型-调研对比.md`](../10-入口路由模型-调研对比.md) → semantic-router |
| AdapterManifest 校验 + 联邦 | 低（M3）| [`../03-自增长与联邦学习-架构设计.md`](../03-自增长与联邦学习-架构设计.md)、06 §10.4 |
