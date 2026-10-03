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

发布包只带本地免费 oMLX 卡与默认路由策略，不带订阅卡或 mock 卡。默认从 **二进制所在目录**读取 `config/components/` 和 `config/routing-policy.yaml`，可从任意工作目录启动。可用 `IDORIS_COMPONENTS_DIR`、`IDORIS_ROUTING_POLICY` 指定其他路径（显式相对路径以 cwd 为准）；配置缺失或校验失败仍拒绝启动，订阅卡仍受 K04 限制，不会自动忽略或回退。源码构建不会自动复制配置，需要自行准备相同目录布局。

默认卡连接 oMLX 的 `http://127.0.0.1:8000`（[oMLX 官方默认端口](https://github.com/jundot/omlx#installation)）。若 oMLX 开启认证，把在 oMLX 管理界面或启动配置中设置的推理 API key 通过 `IDORIS_OMLX_API_KEY` 传给 iDoris；iDoris 不自动读取 oMLX 配置或 `OMLX_API_KEY`。自定义端口（如旧工作站使用的 8088）需修改包内 `config/components/omlx.yaml` 的 endpoint。上游恢复或补充 key 后重启 iDoris，让 Supervisor 重新核对后端状态。

> macOS 对未签名的下载二进制默认会拦一下（Gatekeeper 隔离属性）。如果双击或直接执行时被系统拒绝，跑一次 `xattr -d com.apple.quarantine idoris` 再重试即可，这一步不算"改配置文件"，只是解除下载文件的隔离标记。

## 已知限制（v0.x）

- **付费的 `Resident` 直连转发卡目前不支持，启动时直接拒绝**：`form: http_service` + `load_policy.mode: resident` 的组件卡（例如指向一个通用 OpenAI 兼容后端）由 `idoris-router::proxy::ChatProxy` 直接转发，这条路径完全没有接预算 reserve/settle。为了不让付费候选悄悄绕过预算，启动加载组件卡时会硬性拒绝任何价格不是可证明为 `0`（付费，或价格未知/畸形）的 `Resident` `http_service` 卡，报错里会点名是哪个 `provider.id`。本地免费卡（`cost.input_per_m`/`output_per_m` 均为 `0`）不受影响；`on_demand`/`evict_to_load`（Supervisor + oMLX）路径也不受影响，不管价格多少都照常走预算。跟进项见 [`docs/agent/tasks.md`](docs/agent/tasks.md) FU-22。

## License

This project is licensed under the [Apache License, Version 2.0](LICENSE).  
Copyright 2024-present MushroomDAO Contributors.  
See [NOTICE](./NOTICE) · [TRADEMARK.md](./TRADEMARK.md) · [LICENSE-zh.md](./LICENSE-zh.md) · [TRADEMARK-zh.md](./TRADEMARK-zh.md) for details.
