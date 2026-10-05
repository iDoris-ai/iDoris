# iDoris
Community Brain for cooperation and coordination like Mycelium

## Rust vs. TS

Rust（`Cargo.toml` workspace 下的 `crates/`）是生产实现。`packages/` 下的 TS 代码是**参考实现**，保留供他人借鉴，不再是行为的权威来源。两者行为如有出入，以 `conformance/`（黑盒测试套件，另一任务建立）为准。

## 安装与运行

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

- **付费的 `Resident` 直连转发卡目前不支持，启动时直接拒绝**：`form: http_service` + `load_policy.mode: resident` 的组件卡（例如指向一个通用 OpenAI 兼容后端）由 `idoris-router::proxy::ChatProxy` 直接转发，这条路径完全没有接预算 reserve/settle。为了不让付费候选悄悄绕过预算，启动加载组件卡时会硬性拒绝任何价格不是可证明为 `0`（付费，或价格未知/畸形）的 `Resident` `http_service` 卡，报错里会点名是哪个 `provider.id`。本地免费卡（`cost.input_per_m`/`output_per_m` 均为 `0`）不受影响；`on_demand`/`evict_to_load`（Supervisor + oMLX）路径也不受影响，不管价格多少都照常走预算。跟进项见 [`docs/agent/tasks.md`](docs/agent/tasks.md) FU-22。

## License

This project is licensed under the [Apache License, Version 2.0](LICENSE).  
Copyright 2024-present MushroomDAO Contributors.  
See [NOTICE](./NOTICE) · [TRADEMARK.md](./TRADEMARK.md) · [LICENSE-zh.md](./LICENSE-zh.md) · [TRADEMARK-zh.md](./TRADEMARK-zh.md) for details.
