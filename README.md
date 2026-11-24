# iDoris
Community Brain for cooperation and coordination like Mycelium

## Rust vs. TS

Rust（`Cargo.toml` workspace 下的 `crates/`）是生产实现。`packages/` 下的 TS 代码是**参考实现**，保留供他人借鉴，不再是行为的权威来源。两者行为如有出入，以 `conformance/`（黑盒测试套件，另一任务建立）为准。

## 安装与运行

新用户先读 [v0.3.1 安装、密钥初始化、模型调用与 Agent24 接入检查](docs/安装与首次调用-v0.3.1.md)（[v0.3.1 发布说明](docs/releases/v0.3.1.md)）。**最新版本 [v0.5.0](docs/releases/v0.5.0.md)**：wire 契约 1.2.0（本机 chat 模型推荐 `idoris recommend`、本地模型列表 `idoris models local`、oMLX 密钥解析顺序），并带入 idoris.ai/api 计算服务第一版（`IDORIS_FORM` 切换云 / 组织 / 个人形态；云形态尚无可用上游，不能对外承诺可用）。[v0.4.0](docs/releases/v0.4.0.md) 起支持 Agent24 桌面端一站式托管，CLI 已签名并公证。正式包是 Apple Silicon macOS 命令行程序，控制台位于 `http://127.0.0.1:8741/admin/`，当前只读；不含模型或桌面安装器。推理和模型发现需要 virtual key，`/health` 就绪不代表完成模型配置。

从 [GitHub Releases](https://github.com/iDoris-ai/iDoris/releases) 下载对应版本，校验后解压运行——包内自带可用的 `config/`，不需要编辑配置文件。目前只发布 macOS Apple Silicon（`aarch64-apple-darwin`）。

```bash
# 1. 下载二进制包和它的 sha256 校验文件（把 <tag> 换成具体版本号，使用修复后发布的版本；v0.1.0/v0.1.1 的旧包不可用）
curl -LO https://github.com/iDoris-ai/iDoris/releases/download/<tag>/idoris-<tag>-aarch64-apple-darwin.tar.gz
curl -LO https://github.com/iDoris-ai/iDoris/releases/download/<tag>/idoris-<tag>-aarch64-apple-darwin.tar.gz.sha256

# 2. 校验完整性——对不上就不要往下运行
shasum -a 256 -c idoris-<tag>-aarch64-apple-darwin.tar.gz.sha256

# 3. 解压并运行（保留 idoris 旁边的 config/ 目录）
tar xzf idoris-<tag>-aarch64-apple-darwin.tar.gz
./idoris
```

不需要设置任何环境变量就能起来：`idoris` 缺省监听 `127.0.0.1:8740`（可选用 `IDORIS_PORT` 覆盖）。另开一个终端确认服务已就绪：

```bash
curl http://127.0.0.1:8740/health
```

看到 `{"status":"ok","version":"<tag 去掉 v>",...}` 即表示路由服务已启动；目标是下载解压后 60 秒内启动，无需设置环境变量或改配置。`/health` 不代表模型后端可用：即使没有安装或启动 oMLX，服务也能启动；推理请求遇到不可用候选仍按 fail-closed 返回错误，不回退到远程付费服务。

发布包带本地免费 oMLX 卡、**默认关闭的订阅中转卡**、默认路由策略与模型 catalog；不带 mock 卡。默认从 **二进制所在目录**读取 `config/components/`、`config/routing-policy.yaml` 与 `config/catalog.yaml`，可从任意工作目录启动。可用 `IDORIS_COMPONENTS_DIR`、`IDORIS_ROUTING_POLICY`、`IDORIS_CATALOG` 指定其他路径（显式相对路径以 cwd 为准）；配置缺失或校验失败仍拒绝启动。订阅中转默认不会注册，也不会因为卡文件随包存在就自动调用远程 CLI。

个人模式显式启用订阅中转时，必须同时满足固定沙箱档和 CLI 白名单；例如 Claude：

```bash
IDORIS_ENABLE_SUBSCRIPTION=1 \
IDORIS_SUBSCRIPTION_SANDBOX=idoris-subscription-no-tools-readonly-v1 \
IDORIS_SUBSCRIPTION_CLI=claude \
./idoris serve
```

`IDORIS_SUBSCRIPTION_CLI` 目前只接受 `claude` 或 `codex`；未设置时默认 Claude。`IDORIS_DISABLE_SUBSCRIPTION=1` 优先于 enable，是需要重启生效的 kill switch。订阅能力只接受 loopback 来源，并始终按 remote locality 处理；`local_only` 请求不会 spawn CLI。源码构建不会自动复制配置，需要自行准备相同目录布局。

默认卡连接 oMLX 的 `http://127.0.0.1:8000`（[oMLX 官方默认端口](https://github.com/jundot/omlx#installation)）。若 oMLX 开启认证，把在 oMLX 管理界面或启动配置中设置的推理 API key 通过 `IDORIS_OMLX_API_KEY` 传给 iDoris；iDoris 不自动读取 oMLX 配置或 `OMLX_API_KEY`。自定义端口（如旧工作站使用的 8088）需修改包内 `config/components/omlx.yaml` 的 endpoint。上游恢复或补充 key 后重启 iDoris，让 Supervisor 重新核对后端状态。

> macOS 对未签名的下载二进制默认会拦一下（Gatekeeper 隔离属性）。如果双击或直接执行时被系统拒绝，跑一次 `xattr -d com.apple.quarantine idoris` 再重试即可，这一步不算"改配置文件"，只是解除下载文件的隔离标记。

## 已知限制（v0.x）

- **未确认的本地加载会持久阻止新的加载**：Supervisor 在发送加载请求前写入围栏；加载超时、探测失败或进程中断后，仅重启 Router 或看到空引擎快照不会解除围栏。oMLX 默认使用 `$HOME/.local/state/idoris/omlx/<编码后的引擎地址>/load.pending`，可用绝对路径 `IDORIS_STATE_DIR` 指定状态根目录。同一引擎的所有 Router 必须保持相同的状态目录和地址；容器部署须持久挂载该目录。恢复时先停止 Router，再停止并确认旧 oMLX 进程及其加载任务已退出，随后删除错误中指定的 `load.pending`，重启引擎和 Router。不要仅凭 unload 成功或空 status 删除围栏。
- **已确认加载后的 pin 配置失败可恢复**：例如 sub-key 被 admin 登录明确拒绝，但模型状态确认已加载，此时返回 `load_postcondition_failed` 并尝试卸载。确认释放后恢复预算；释放失败仍保留该模型占用，可稍后卸载或修正配置后重试。这类已完成加载不保留全局加载围栏，重启会按引擎实际驻留重新对账。pin 请求超时、连接中断或模型状态无法确认时，仍按 `load_unconfirmed` 保守处理。

- **付费 `Resident` 直连卡支持 buffered 请求，暂不支持付费 streaming**：`form: http_service` + `load_policy.mode: resident` 走独立 HTTP 代理；付费非流式请求执行预算预留与结算，付费流式请求在调用上游前拒绝。付费 resident 卡不会被全局启动门禁拒绝；resident 代理不能替代 Supervisor 的模型加载、驱逐与状态核对。

## 进程生命周期（由启动器托管时）

- **停止**：`SIGTERM` 与 `SIGINT` 走同一条优雅停机路径（停止接收新连接、收尾订阅运行时），成功时打印 `idoris: 已停止` 并以 0 退出；若 10 秒内未收尾则输出 `[idoris] 停止失败：优雅停机超过 10s，强制退出` 并以 1 退出；退出前会销毁异步运行时（最多再等 5 秒）以回收本地 runtime 子进程组，总停机时间上限约 15 秒。
- **单实例**：`idoris serve` 启动时对 `<IDORIS_DB_PATH>.lock`（路径先经 canonicalize，符号链接别名共用同一把锁）加独占咨询锁（进程退出由操作系统自动释放，无陈旧锁）。同一数据库已有实例时，在绑定端口前输出 `[idoris] 启动失败：已有 iDoris 实例在使用同一数据库 <path>` 并以退出码 **3** 退出（其他启动失败为 1）。离线的 `idoris key issue` / `idoris admin` 不受该锁影响。
- **托管引擎**（卡里有 `extensions.idoris_managed`，由 `idoris setup omlx --managed` 生成；没有这样的卡时下面都不适用，行为与以前一致）：iDoris 自己探测、拉起并监管本地 oMLX，不再要求「先起 oMLX 再起 iDoris」。引擎没就绪时 iDoris 照常答 `/health`（`phase:"starting"`，`upstreams[]` 带每个引擎的状态与原因码），数据面其余路径返回 503 `idoris_starting`，引擎落定（就绪，或终局的失败）之后才切换到完整服务；等待有上限（启动超时，oMLX 缺省 90 秒，到点还没好就放弃这个引擎）。**托管引擎起不来（没装、配置错误、熔断、等不到就绪）不会拖住别的卡**：门控降级放行，健康的 resident 卡与自定义上游照常服务，路由到托管卡的请求直接 503 并带原因（`error.reason_code` 是 `/health` 里该上游的 `reason`，例如 `engine_not_found`），`/health.phase` 是 `degraded`；降级放行之后这个引擎不再重试，恢复要重启 iDoris。端口上已有引擎时只探测、不发信号，也不会在退出时停它；iDoris 自己拉起的引擎用私有 key，崩溃循环有熔断，退出时一并停掉（`stop_on_exit:"never"` 的正常退出除外）。`kill -9` iDoris 之后重启会接管仍在跑的自有引擎；若上次在加载中被杀，对自有引擎会杀掉重拉并清除加载围栏，外部引擎的围栏原样保留、仍须按上面的步骤人工处理，处理之前该上游报 `unavailable`（`fence_pending`），路由到它的请求 503，别的卡照常服务。引擎在服务期间进程退出、进程内无法恢复时，iDoris 优雅停止并以退出码 **5** 退出，由启动器重启（孤儿超时是退出码 4）。
- **托管引擎的 Admin 控制**（接口规范 §2.11.4；同一个 Admin token，`Authorization: Bearer <token>`）：`GET /admin/api/v1/upstreams` 列出每个上游（状态、原因码、`managed`、`owned`、端口、自有引擎的进程号、失败次数、`restart_required`，没有任何 key、token）；`POST /admin/api/v1/upstreams/{id}/start|stop|restart` 只对托管且由 iDoris 自己拉起的引擎生效（外部引擎、没有声明托管的卡是 409 `upstream_not_owned`，iDoris 不给别人的进程发信号），受理即回 202 加元素（元素是实例任务**受理那一刻自己的快照**：`stop` 受理时状态已经是 `stopped`、`restart` 受理时已经是 `restarting`，数据面立即 503；操作员的重启不记失败），幂等（`restart` 的幂等只在第一次操作进行期间成立：并发的重复请求并入它，它做完之后再发就是一次新的重启，再换一次引擎），要轮询 `GET` 到终态：`stop` 要看到 `state` 是 `stopped` **并且** `owned` 是 false 才算做完（`unavailable` 加 `port_in_use` 是没能确认进程已经消失，不是停好了：进程可能还占着端口，用 `start` 重试）。`stop`、`restart` 先关 Supervisor 的门、等在途的加载与聊天做完（至多 30 秒，到点切断旧一代的凭证）才停旧引擎；放行时就绪的引擎 `restart` 之后，聊天请求会在新进程里重新加载模型（Supervisor 的账本在发布 `ready` 之前对着新进程重建，旧一代的任务拿不到新一代的凭证）；放行时没就绪而降级放行的上游，`start` 是 409 `restart_required`、仍要重启 iDoris（读 `GET` 元素的 `restart_required` 字段）；它没有自有引擎，`stop`、`restart` 一律是 409 `upstream_not_owned`。路径里的 `{id}` 解码之后不是 UTF-8 是 400 `invalid_upstream_id`；命令在截止时间（`command_timeout`，缺省 60 秒）之前没有被实例任务认领是 503 `idoris_stopping`（没有受理，事后也不会执行）。iDoris 每次拉起引擎会写一行 `拉起引擎：program=… port=… memory_guard=… model_dir=… base=…`（oMLX 会改写进程标题，`ps` 里看不到这些），里面没有任何 key。

## License

This project is licensed under the [Apache License, Version 2.0](LICENSE).  
Copyright 2024-present MushroomDAO Contributors.  
See [NOTICE](./NOTICE) · [TRADEMARK.md](./TRADEMARK.md) · [LICENSE-zh.md](./LICENSE-zh.md) · [TRADEMARK-zh.md](./TRADEMARK-zh.md) for details.
