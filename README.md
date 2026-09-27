# iDoris
Community Brain for cooperation and coordination like Mycelium

## Rust vs. TS

Rust（`Cargo.toml` workspace 下的 `crates/`）是生产实现。`packages/` 下的 TS 代码是**参考实现**，保留供他人借鉴，不再是行为的权威来源。两者行为如有出入，以 `conformance/`（黑盒测试套件，另一任务建立）为准。

## 安装与运行

从 [GitHub Releases](https://github.com/iDoris-ai/iDoris/releases) 下载对应版本，校验后直接运行——不需要编辑任何配置文件。目前只发布 macOS Apple Silicon（`aarch64-apple-darwin`）。

```bash
# 1. 下载二进制包和它的 sha256 校验文件（把 <tag> 换成具体版本号，例如 v0.1.0）
curl -LO https://github.com/iDoris-ai/iDoris/releases/download/<tag>/idoris-<tag>-aarch64-apple-darwin.tar.gz
curl -LO https://github.com/iDoris-ai/iDoris/releases/download/<tag>/idoris-<tag>-aarch64-apple-darwin.tar.gz.sha256

# 2. 校验完整性——对不上就不要往下运行
shasum -a 256 -c idoris-<tag>-aarch64-apple-darwin.tar.gz.sha256

# 3. 解压并运行
tar xzf idoris-<tag>-aarch64-apple-darwin.tar.gz
./idoris
```

不需要设置任何环境变量就能起来：`idoris` 缺省监听 `127.0.0.1:8740`（可选用 `IDORIS_PORT` 覆盖）。另开一个终端确认服务已就绪：

```bash
curl http://127.0.0.1:8740/health
```

看到 `{"status":"ok",...}` 就说明从下载到跑起来全程在 60 秒内、没碰任何配置文件。

> macOS 对未签名的下载二进制默认会拦一下（Gatekeeper 隔离属性）。如果双击或直接执行时被系统拒绝，跑一次 `xattr -d com.apple.quarantine idoris` 再重试即可，这一步不算"改配置文件"，只是解除下载文件的隔离标记。
>
> 现状（R1/R2 骨架）：这个二进制目前只实现 `GET /health`；`/v1/*` 等业务路由会返回 `501`，要等 R2-D 把路由/策略/租户逻辑接进来才有完整行为。

## License

This project is licensed under the [Apache License, Version 2.0](LICENSE).  
Copyright 2024-present MushroomDAO Contributors.  
See [NOTICE](./NOTICE) · [TRADEMARK.md](./TRADEMARK.md) · [LICENSE-zh.md](./LICENSE-zh.md) · [TRADEMARK-zh.md](./TRADEMARK-zh.md) for details.
