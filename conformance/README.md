# @idoris/conformance

R0：语言无关的黑盒 HTTP 契约测试套件。

只通过 HTTP 访问被测服务，**不 import 任何 `@idoris/*` 实现代码**——这套测试要在
iDoris 从 TypeScript 迁移到 Rust 期间，同时锁住 TS 参考实现（`packages/router`）
和将来的 Rust 版的行为。谁通不过，谁就是有问题的那一个。

> 本套件覆盖 PR #46（T4.1 Agent24 接入前置，规范 v1.0.1）落地的 `/health` 服务
> 身份、`X-iDoris-Record-Id`、`X-iDoris-Served-Locality`、`X-iDoris-Cached`/
> `X-iDoris-Origin-Record-Id`——#46 已合并进 `main`，本套件直接基于 `main` 编写。

## 怎么跑

```bash
# 推荐：一条龙（build router → 跑套件），根 package.json 已经接好
pnpm conformance
```

`pnpm conformance` 做两件事：
1. `pnpm --filter @idoris/router... build`：把 TS router 及其依赖 build 出 `dist/`；
2. `pnpm --filter @idoris/conformance test:conformance`：起被测进程、跑全部用例。

也可以手动指定被测进程的启动命令（见下面「接入新实现」）：

```bash
IDORIS_CONFORMANCE_CMD="node packages/router/dist/cli.js serve" \
  pnpm --filter @idoris/conformance test:conformance
```

## 接入新实现（例如 Rust 版）

被测服务的启动方式完全由环境变量 `IDORIS_CONFORMANCE_CMD` 决定，套件本身不知道
也不关心被测服务是用什么语言写的。接入新实现只需要三件事：

1. 提供一个可执行命令，启动后监听 `IDORIS_PORT` 指定的端口（loopback）；
2. 从 `IDORIS_COMPONENTS_DIR` 读组件卡目录（`config/components/*.yaml` 同格式），
   从 `IDORIS_ROUTING_POLICY`（可能不传）读路由策略；
3. 暴露 `GET /health`，套件靠它判断"进程已经准备好接收请求"。

把 `IDORIS_CONFORMANCE_CMD` 换成新实现的启动命令即可，例如：

```bash
IDORIS_CONFORMANCE_CMD="/path/to/idoris-router-rs serve" \
  pnpm --filter @idoris/conformance test:conformance
```

`IDORIS_CONFORMANCE_CMD` 用一个极简的、支持基本引号的分词器解析，不是真正的 shell；
命令本身很简单（没有带空格的路径/参数）时够用。参数比较复杂、或者想彻底避免任何
解析歧义时，改用 `IDORIS_CONFORMANCE_ARGV`（JSON 字符串数组，优先级更高）：

```bash
IDORIS_CONFORMANCE_ARGV='["/path/to/idoris-router-rs","serve","--flag","value with spaces"]' \
  pnpm --filter @idoris/conformance test:conformance
```

`conformance/src/harness.ts`、`conformance/tests/*.ts` 不需要改一行。

## 环境变量约定（被测进程需要遵守）

| 变量 | 必填 | 说明 |
|---|---|---|
| `IDORIS_PORT` | 是 | 监听端口（套件每组测试分配一个随机空闲端口） |
| `IDORIS_COMPONENTS_DIR` | 是 | 组件卡目录 |
| `IDORIS_ROUTING_POLICY` | 否 | 路由策略文件；相对路径按**仓库根目录**解析，不是 cwd。不传时 TS 参考实现（M1）落到仓库自带的 `config/routing-policy.yaml`，**不是**"未配置" |
| `IDORIS_ALLOW_MOCK` | 否 | TS 参考实现默认拒绝注册 `provider.id: mock` 的组件卡（M1）；套件统一设成 `1`，虽然目前所有 fixtures 都用 `omlx` id，用不到它 |
| `IDORIS_DEPLOY_MODE` | 否 | `personal`（默认）/ `tenant` / `community` / `city`；部分用例用它单独起一个 `tenant` 模式的进程 |

## 覆盖清单

- `GET /health`：200；服务身份字段 `service`/`version`/`contract_version`（锁定为规范标注的
  `1.0.1`）/`instance_id`（同进程两次请求不变，不同进程互不相同）
- `GET /v1/models` 形状（`id`/`object`/`owned_by`）
- 请求体：非法 JSON → 400 `invalid_json`；合法 JSON 但不是对象（`null`/数组/数字）→ 400
  `invalid_body`；合法最小请求走通全链路；并锁定 `invalid_json` > `invalid_body` >
  header 校验的判定顺序
- 控制面 header：`X-iDoris-Privacy` 非法值 → 400 `invalid_privacy`；缺省按 `local_only` 处理；
  `Complexity`/`Capabilities`/`Fallback` 非法值 → 400 `invalid_header`；`Intent` 接受任意非空字符串
- 隐私 fail-closed：`local_only` 且唯一候选是远程后端 → 503 `local_only_unavailable`，
  假上游收到的真实 HTTP 请求数为 0（含缺省未带 Privacy header、显式 `local_only`、
  命中 vision 规则三种情形），并有 `privacy=any + complexity=complex` 的正控对照
- 默认路由策略（M1）：不传 `IDORIS_ROUTING_POLICY` 时落到仓库自带的
  `config/routing-policy.yaml` 并能正常放行请求；`IDORIS_ROUTING_POLICY` 指向不存在的
  文件时启动直接失败（fail-fast），不是延迟到运行期才报错
- 上游 5xx：重试到成功、持续失败原样透传，均锁定"最多重试 2 次（共 3 次尝试）"
- `X-iDoris-Request-Id` 幂等：60s 窗口内同一 Request-Id 只打一次上游
- 慢响应：上游延迟但最终成功时不会被提前掐断
- 取消传播：**目前锁定的是"不工作"这个已知 bug**（客户端 abort 后上游不会被 abort），
  见下面「已知的规范 vs TS 现状落差」第一条；负对照（不主动断开也不该被误判成已取消）
  仍然是真正在验证的行为
- 流式 SSE：正常透传；上游直接 5xx 时不重试，改以 JSON 错误响应（而不是 SSE）返回
- `deploy_mode=tenant` 缺 `X-iDoris-Tenant` → 400 `tenant_missing`；带了则放行
- `X-iDoris-Record-Id`：所有响应都带（成功、400、404、503……），服务端生成，调用方
  填的 `X-iDoris-Request-Id`/`X-iDoris-Record-Id` 都不会被采纳
- `X-iDoris-Served-Locality`：只在真的选中了某个后端之后才出现（loopback 本地候选
  回报 `loopback`）；在此之前就被拒绝的响应（400/503 等）不带这个头
- `X-iDoris-Cached` + `X-iDoris-Origin-Record-Id`：同一个 Request-Id 在 60s 窗口内
  第二次命中缓存时才带；首次命中（真实推理）不带

## 暂不覆盖（按上游指令）

预算与意图解析顺序相关的用例暂不写：jason 正在把顺序修订为"隐私 → 意图 → 预算"，
TS 实现里还没有对应代码，等这部分定稿并落地后再补测试。

## 已知的规范 vs TS 现状落差（`it.todo`）

见 `tests/known-spec-conflicts.test.ts`：

1. **【真实 bug，本轮 conformance 复审发现】取消传播不工作**——
   `packages/router/src/server.ts:330` 的 `req.on("close", () => controller.abort())`
   监听的是请求对象（客户端→路由器）的 close，不是响应对象/socket 的 close；请求体
   读完之后 `req` 会自己触发一次 'close'，跟客户端有没有真的断开连接无关，等这行代码
   挂上监听器时往往已经错过了真正的断开事件。已用变异测试验证（改一行 `req`→`res`
   就能让取消传播工作，细节见本次 PR 描述），改动已还原，**没有改动任何 TS 实现代码**。
2. 统一错误体缺 `rule_id/reason_code/evidence/remediation` 字段。
3. TS 用了规范 §3.11 枚举之外的错误 `type`（含新出现的 `invalid_body`/`internal_error`）。
4. 完全没有 §3.2 的虚拟 key 鉴权。
5. `ChatProxy` 对上游请求没有任何超时（本来设想只有客户端主动断开才会 abort，
   但见第 1 条，这条防线本身也没工作）。
6. `/v1/messages`、`/v1/embeddings`、`/v1/rerank`、`/v1/systemone`、`/v1/inspect`、
   `/v1/feedback`、`/v1/trajectories`、`/admin/api/v1/*` 均未实现。

## 设计约束

- `conformance/` 只允许 `import` node 内置模块和 `vitest`；不得 `import` 任何 `@idoris/*`
  包——否则就不是黑盒测试了。
- 假上游（`conformance/src/fake-upstream.ts`）是一个独立的 `node:http` 服务器，
  实现最小 OpenAI 兼容面（`/v1/models`、`/v1/chat/completions`，含流式/非流式/慢响应/
  挂起不回），并统计真实收到的请求数——"远程出站 0 次"这类断言，断的是这个真实计数，
  不是被测服务内部的某个计数器。
- 组件卡的 `provider.id` 只能是 `mock` 或 `omlx`（`packages/adapters/src/factory.ts`
  目前只认这两个 id，其余 id 启动时会直接抛错）；`endpoint` 一律指向假上游的真实
  HTTP 地址，这样 `form: http_service` 才会真的发出一次网络请求。
- 每个 fixture 目录只放一张组件卡：TS 参考实现（L5）现在拒绝同一个 `provider.id`
  出现两次；需要"同时有本地+远程候选"的场景不在本轮覆盖范围内。
- 被测子进程以 `detached: true` 起（POSIX 上即新进程组的组长），杀的时候按
  `-pid` 杀整个进程组，不是只杀顶层那一个 pid——被测实现如果自己又 fork 了
  子进程，不会留下孤儿。
- 传给被测子进程的环境变量会先剥掉当前进程（跑 `pnpm conformance` 的那个 shell）
  自己带的所有 `IDORIS_*`，再叠加套件显式要传的那几个——避免开发者本机环境里
  偶然导出的 `IDORIS_DEPLOY_MODE` 之类的变量意外泄漏进被测进程，污染测试隔离性。
