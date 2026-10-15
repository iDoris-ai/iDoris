# v0.3.1 Release Wire 审计快照（2026-10-15）

> **性质**：有日期的审计快照，不是第二份规范，也不是发布验收 PASS。  
> **审计基线**：`base_sha` = `1c4cebdae582cda68db9f8e5aca9ff2caa1db8bd`（`main` 上 Merge #512）。  
> **权威规范**：以 [PR #516](https://github.com/iDoris-ai/iDoris/pull/516) 文档修订 1.2 / 现行 wire `1.0.1` 为准；R9 三项仍为 **OWNER REVIEW PENDING**。本快照只记录“仓库与可访问公开源里存在什么 / 缺什么”，不发明端点或开关。  
> **执行环境**：Grok Bot box（Linux）。不能执行 macOS arm64 正式发布二进制；macOS 路径证据来自 README、release workflow、GitHub Release 资产与 OPEN 的 [#507](https://github.com/iDoris-ai/iDoris/pull/507) 安装指南文本。

---

## 0. 结论（一句话）

v0.3.0 已发布 macOS arm64 文本网关归档；v0.3.1 里程碑仍 OPEN。用户可走「下载→校验→启动→签发 key→发现模型→具体模型调用」的 iDoris 单仓路径（依赖自备 oMLX/权重），但 **`idoris/<role>` 真实绑定未合入**、**Agent24 数据面 provider 未交付**、**图片/视频生成能力无路由证据**；用户候选模型一律标 **未验证**，不得当作已接线。

---

## 1. 权威来源与访问边界

| 来源 | 状态 / 路径 | 本审计用法 |
|---|---|---|
| README 安装段 | `README.md`（基线检出） | 下载 / 校验 / 启动 / Gatekeeper |
| Release workflow | `.github/workflows/release.yml` | 仅 `aarch64-apple-darwin` 打包；无 DMG / Windows |
| v0.3.0 发布说明草稿 | `docs/releases/v0.3.0.md` | 发布前置；**无** `docs/releases/v0.3.1.md` |
| 接口规范（main 基线） | `docs/interfaces/iDoris-Agent24-边界与接口规范.md` | 对照；完整 1.2 整合在 PR516 |
| PR516（权威修订 1.2） | https://github.com/iDoris-ai/iDoris/pull/516 （OPEN） | wire/角色/Admin/R9/§4 差距表 |
| PR507 onboarding | https://github.com/iDoris-ai/iDoris/pull/507 （OPEN）`docs/安装与首次调用-v0.3.0.md` | 实测安装旅程文案；**尚未合入 main** |
| Issue #506 | https://github.com/iDoris-ai/iDoris/issues/506 （OPEN） | v0.3 收尾清单 |
| Milestone v0.3.1 | https://github.com/iDoris-ai/iDoris/milestone/2 | 「接线与发布收尾」；描述要求组合验收后才正式 release |
| Agent24 公开仓 | https://github.com/iDoris-ai/Agent24 | PR980 MERGED；PR981/982 OPEN；Issue #974 OPEN |
| CLI / 契约源码 | `crates/idoris-router/src/cli.rs`、`key_issue.rs`、`lib.rs`；`crates/idoris-contracts/src/version.rs` | 命令面与 wire 版本 |

**访问缺口**：本 box 可 clone `iDoris-ai/Agent24` 并读公开 PR/issue。Agent24 Desktop 真机、配对 launcher、真实 Keychain 存 key、干净 macOS 组合验收 **未在本审计中执行**。PR980 明确：consumer pointer 指向 iDoris PR516，**不是第二份接口规范**；IDORIS-01 仅为计划/指针 bootstrap。

---

## 2. v0.3.0 已发布资产 vs v0.3.1 未发布

### 2.1 v0.3.0（已发布）

- Tag / Release：`v0.3.0`（published 2026-10-15 UTC），解引用提交在 onboarding 指南中记为 `03c079c70611331a8c85a1666e32a75df6359d09`。
- 资产（仅两项，`gh release view` 核实）：
  - `idoris-v0.3.0-aarch64-apple-darwin.tar.gz`
  - `idoris-v0.3.0-aarch64-apple-darwin.tar.gz.sha256`
  - 归档 SHA256（#507 指南记录）：`ce54063f78b2d092c0ba820be4729635a5fe1df2ec91debeec10ef5dd582dfad`
- **支持**：macOS Apple Silicon（`aarch64-apple-darwin`）。README 与 workflow 一致。
- **不存在 / 未确认**：
  - **无 DMG**（release.yml / smoke：包内只要 `idoris` + 四份 config）。
  - **无 Windows EXE / Linux 预编译包**（#507 指南明示；本审计未发现对应资产）。
  - **模型权重不随包**；oMLX 不随包（`scripts/release-smoke.py` 断言 archive 仅含二进制与四配置）。
- Workspace crate 版本在本基线为 `0.3.0`（`crates/idoris-router/Cargo.toml`）。
- wire：`CONTRACT_VERSION = "1.0.1"`（`crates/idoris-contracts/src/version.rs`）。

### 2.2 v0.3.1（尚未发布）

- **无** tag `v0.3.1`，**无** GitHub Release，**无** `docs/releases/v0.3.1.md`（本检出 `test ! -f` 通过）。
- Milestone「v0.3.1 接线与发布收尾」OPEN；描述要求 Agent24↔iDoris 规范一致、安装初始化及真实模型调用、分配额度实际准入、完整组合验收与正式 release。
- 本审计 **不** 声称 v0.3.1 产品验收 PASS。

### 2.3 相对 v0.3.0 发布点，本 `base_sha` 已合入但未发新 tag 的相关变更

| 项 | 证据 | 含义 |
|---|---|---|
| #511 精确容量基础 | MERGED → `9395a96` | main 有精确字节 ledger 基础；≠ 用户已拿到新二进制 |
| #512 启动预算接线 | MERGED → `1c4cebd`（本审计 base） | `IDORIS` 分配字节可进入 runtime spawn；仍需 Agent24 侧交付预算值（见 PR982） |
| #508 key ceiling | 已合入 main（PR516 §4.1） | 补丁语义在源码，**未**以新 release 资产交付 |
| #510 role binding | **仍 OPEN** | 基线 `dispatch.rs` **无** `idoris_role_models` / `role_models` |
| #507 onboarding 文档 | **仍 OPEN** | 安装指南尚未在 main |

---

## 3. 用户旅程（逐步：真实路径 / 命令 / 支持 vs 缺口）

以下命令与路径来自 README、CLI 源码、#507 指南与本检出验证。**不编造**未在源码出现的子命令。

### 3.1 下载

| | |
|---|---|
| **支持** | `curl -LO https://github.com/iDoris-ai/iDoris/releases/download/v0.3.0/idoris-v0.3.0-aarch64-apple-darwin.tar.gz`（及对应 `.sha256`）。README / #507。 |
| **缺口** | 无 Windows / Linux / Intel Mac 预编译；无 DMG。源码 zip 需自备 Rust 与 config 布局。 |

### 3.2 校验

| | |
|---|---|
| **支持** | `shasum -a 256 -c idoris-v0.3.0-aarch64-apple-darwin.tar.gz.sha256`。 |
| **缺口** | 无签名公证流程文档化为发布门禁；仅 SHA256。 |

### 3.3 解压与启动

| | |
|---|---|
| **支持** | `tar xzf …`；保留旁路 `config/`；`./idoris` 或 `./idoris serve`；默认 `127.0.0.1:8740`；`curl http://127.0.0.1:8740/health`。CLI USAGE（本机 debug 二进制实测打印）：`idoris [serve [--admin-token-stdin]] \| idoris admin <status\|backends\|models\|roles\|runtimes> --token-stdin \| idoris key issue --spec-stdin`。 |
| **缺口** | 无 `idoris init` / 交互向导。`/health` **不**证明模型后端可用。macOS Gatekeeper：`xattr -d com.apple.quarantine idoris`（README）。本 box 为 Linux，**未**跑正式 darwin 归档。 |

**本机验证**：`./target/debug/idoris nope` → exit `1`，stderr 打印上述 USAGE。

### 3.4 签发 key（“sign key”）

| | |
|---|---|
| **支持** | `idoris key issue --spec-stdin`（`crates/idoris-router/src/key_issue.rs`）。Spec 字段：`owner, tenant_id?, allowed_privacy, allowed_roles, expires_at_ms?, admin_scopes`。需同一 `IDORIS_DB_PATH` 供签发与服务共享。#507 给出 personal `local_only` 全角色示例。Admin：`printf token \| ./idoris serve --admin-token-stdin`；只读 `idoris admin … --token-stdin`；控制台 `http://127.0.0.1:8741/admin/`。 |
| **缺口** | 无独立 “sign” 子命令名；密钥明文一次写出，operator 自管。`admin_scopes` 实际只能为空（规范/PR516）。四类凭证（数据 key / Admin token / oMLX key / 订阅）不得混用。 |

### 3.5 初始化

| | |
|---|---|
| **支持** | 首次签发/启动按需建 SQLite；Personal 应用独立 DB（#507 / #506）。 |
| **缺口** | **无** `idoris init`。Tenant↔Personal 迁移见 `docs/releases/v0.3.0.md`（#507 链接）；完整迁移 UX 仍依赖文档纪律。 |

### 3.6 模型发现

| | |
|---|---|
| **支持** | `GET /v1/models`（`lib.rs` 路由）；需 virtual key（Bearer）。`GET /capabilities` 存在，形状为旧条目（PR516 §4）。Admin 只读 `models`。默认上游 oMLX `http://127.0.0.1:8000`（`config/components/omlx.yaml`）。 |
| **缺口** | catalog（`config/catalog.yaml`）是推荐元数据，**≠** 已下载/可加载。无内置 `model pull`。晚启动后端可能出现 models 200 但 chat 持续 503（#507 文档化；需重启 iDoris）。 |

### 3.7 角色绑定

| | |
|---|---|
| **现状（本基线）** | 请求可带 `idoris/<role>`；**未合入**运维 `extensions.idoris_role_models`。#507/#506/#510：默认路径会把角色解析成错误身份（如 provider id `omlx`），引擎要真实模型名时 **503**。 |
| **目标（未验收）** | PR510（OPEN）：显式绑定 HTTP role→具体 backend model ID；缺绑定 fail-closed；auto 也必须绑定。Agent24 只发角色，不写 YAML。 |
| **验收命令（待 #510 合入后，非本快照 PASS）** | 有绑定：`idoris/daily` load/chat 上游具体 ID 一致；未绑定：503 且零上游 POST（见 PR516 §4 / PR510 描述）。 |

### 3.8 调用（invoke）

| | |
|---|---|
| **文本网关（已实现路由）** | `POST /v1/chat/completions`、`/v1/embeddings`、`/v1/rerank`、`/v1/messages`、`/v1/feedback`；隐私头 `X-iDoris-Privacy` 等。成功期望头：`X-iDoris-Served-Locality`、`X-iDoris-Record-Id`（#507）。默认 Supervisor 拒绝未支持采样字段如 `max_tokens`（400 `unsupported_field`）。 |
| **缺口** | 角色别名路径阻塞（上节）。付费 embeddings/rerank settlement 未完备时拒绝（`lib.rs` 文案）。真实小模型离线调用需 macOS+oMLX+已缓存权重；**本审计未宣称实测 200**。 |

### 3.9 失败处理

| | |
|---|---|
| **支持** | `local_only` fail-closed，不静默上云（README / 规范）。401/403 权限；503 后端/加载/容量；402 预算（policy 有 Fallback 分支，但 router 传 `budget: None` 时实际仍 402——PR516 B1，**实现差距**）。`load.pending` 围栏恢复见 README 已知限制。 |
| **缺口** | 显式 `X-iDoris-Fallback` 免费回落 **未在 router 接线**（PR516 B1）。结构化 readiness 协议不存在（仅 listening 文本）。 |

---

## 4. 文本网关 vs 图片 / 视频

| 能力 | 证据 | 结论 |
|---|---|---|
| 文本 chat / embeddings / rerank / messages | `crates/idoris-router/src/lib.rs` `Router::new()` 路由表 | **当前文本网关存在**（实现≠用户真机已通） |
| 视觉理解（多模态输入） | catalog 有 `qwen2.5-vl-7b` `roles: [vision]`；属推荐条目 | **未**在本审计验证加载/调用；≠ image generation |
| `/v1/images*`、`/v1/videos*`、image/video generation | `rg` 路由表 **无** 匹配 | **无证据**；不得声称已接线 |
| 用户候选生图/视频模型 | 见 §6 | 一律 **未验证** |

---

## 5. v0.3.1 blocker 清单（拥有仓 + 验收命令方向）

下列为 **发布/组合验收前** 仍需关闭的阻塞项（对照 milestone #2、#506、PR516 §4、开放 PR）。**不是**产品 PASS 列表。

| # | Blocker | 拥有仓 | 状态（审计时） | 验收命令 / 证据方向 |
|---|---|---|---|---|
| B1 | `idoris/<role>` → 真实 backend model 绑定 | iDoris | #510 OPEN；基线无 `role_models` | 绑定后 daily：上游 load/chat ID 一致；未绑定 auto/deep：503 + 零 POST（PR510 / PR516 §4） |
| B2 | 安装/首次调用权威文档合入 main | iDoris | #507 OPEN | 文档相对链接存在；`python3 scripts/test_onboarding_smoke.py <release-idoris>` 在 darwin 归档上（脚本在源码树，不在发布包） |
| B3 | 历史九端点拒绝 / 默认认证验收补测 | iDoris | #515 OPEN | PR 自带 focused 测试矩阵；合入后 CI + 主线复跑 |
| B4 | 租户 key 权威 / 事件前校验（历史 #485 等残留） | iDoris | #485 OPEN（部分能力已由 #508 等承接，仍须独立审阅） | 跨租户/未绑定/匿名零副作用；scope-before-`request.received` |
| B5 | 规范修订 1.2 合入 + R9 owner 决策 | iDoris + owner | #516 OPEN；R9 PENDING | 文档合并；OD-1/2/3 书面决定；**不**借文档授权实现 launcher/下载器 |
| B6 | Agent24 独占 data-plane provider + locality 绊线 | Agent24 | #974 OPEN；PR981 OPEN（编解码提取） | 配置 `IDORIS_URL` 后请求走 iDoris；改 OMLX_URL **不是**集成；缺失/非 loopback 不向模块返回 |
| B7 | Agent24 接线计划已立，实现未完 | Agent24 | PR980 MERGED（指针/计划）；非运行时交付 | 不得把 PR980 当 provider 已通 |
| B8 | 扣除 core 后的字节预算交给独立 iDoris | Agent24 → iDoris | PR982 OPEN；iDoris #512 已合入源码 | Agent24 发送分配字节；iDoris `allocated_budget_bytes` 并发 load 只准入配额内（runtime 单测名见 `runtime.rs`） |
| B9 | 可信 launcher / Admin FD 组合 | 双方（R9 OD-1） | 未交付 producer；Admin consumer 部分在 Agent24 | 匿名 FD intake + 错误 FD/继承/超时；无 launcher 时 Admin 可 unavailable，数据面可独立 |
| B10 | 干净 macOS arm64 真机组合验收 + 打 `v0.3.1` tag | iDoris release | 无 tag | tag 在 main 祖先；release.yml 构建/smoke；owner 真机成功调用——**本审计未做** |
| B11 | Fallback 预算回落接线（规范已接受，router 未接） | iDoris | PR516 B1 记录差距 | 显式 `X-iDoris-Fallback` 时免费回落并回传降级；无声明仍 402（不可反写规范） |

---

## 6. 用户候选模型（一律未验证 / 不能当作已接线）

| 用户候选 | 场景标签 | 仓库内痕迹 | 审计标记 |
|---|---|---|---|
| Qwen3.6-35B-A3B | 办公 | catalog id `qwen3.6-35b-a3b`，`roles: [deep]`，量化选项为 GGUF 风格标签；默认卡为 oMLX/MLX | **未验证** — catalog ≠ 已下载/可加载/已绑定 |
| Qwen3.8-Flash-Next | 复杂 | catalog 仅有 `qwen3.8-27b`（非 Flash-Next 名） | **未验证** — 名称未作为可调用 ID 证实 |
| Qwen-Image-2.1 | 图片 | catalog / 路由 **无** 该 ID；无 image generation 路由 | **未验证** |
| FLUX.2-klein-4B | 快图 | 全仓 config **无** FLUX 条目 | **未验证** |
| Wan2.2-TI2V-5B | 视频 | 全仓 config **无** Wan/TI2V 条目；无 video 路由 | **未验证** |

**纪律**：不得因出现在口头清单或 catalog 推荐表，就声称 v0.3.1 wire 已接通。

---

## 7. 本审计执行的验证（真实 exit_code）

环境：Linux box；Rust 1.95；检出 `1c4cebd`。macOS 正式包 **未**下载执行。

| 测试名 | 命令 / 检查 | exit_code | pass/fail |
|---|---|---|---|
| workflow_release_exists | `test -f .github/workflows/release.yml` | 0 | pass |
| workflow_ci_exists | `test -f .github/workflows/ci.yml` | 0 | pass |
| readme_install_section | `rg -q 'aarch64-apple-darwin' README.md` | 0 | pass |
| v030_release_notes_exist | `test -f docs/releases/v0.3.0.md` | 0 | pass |
| no_v031_release_notes | `test ! -f docs/releases/v0.3.1.md` | 0 | pass |
| cli_usage_string_present | `rg -q 'idoris key issue --spec-stdin' crates/idoris-router/src/cli.rs` | 0 | pass |
| key_issue_module_present | `test -f crates/idoris-router/src/key_issue.rs` | 0 | pass |
| release_only_apple_darwin | `rg -q 'aarch64-apple-darwin' .github/workflows/release.yml` | 0 | pass |
| no_dmg_windows_in_release_yml | `! rg -qi 'dmg\|windows\|x86_64-pc-windows' .github/workflows/release.yml` | 0 | pass |
| catalog_has_qwen36 | `rg -q 'qwen3.6-35b-a3b' config/catalog.yaml` | 0 | pass |
| catalog_no_flux_wan_image_gen | `! rg -qi 'flux\|wan2\|qwen-image' config/catalog.yaml` | 0 | pass |
| wire_version_101 | `rg -q '1.0.1' crates/idoris-contracts/src/version.rs` | 0 | pass |
| no_role_models_on_base | `! rg -q 'idoris_role_models\|role_models' config/components/omlx.yaml crates/idoris-router/src/dispatch.rs`（dispatch 无符号） | 0 | pass |
| crate_version_030 | `rg -q 'version = "0.3.0"' crates/idoris-router/Cargo.toml` | 0 | pass |
| release_assets_darwin_only | `gh release view v0.3.0` 资产名仅两条 darwin | 0 | pass |
| no_v031_tag | `gh release view v0.3.1` → not found | n/a | pass（缺口确认） |
| cli_unit_parse | `cargo test -p idoris-router --locked --lib parses_serve_admin` | 0 | pass |
| health_shape_unit | `cargo test -p idoris-router --locked --lib health_has_the_pr46` | 0 | pass |
| cli_bad_args_usage | `./target/debug/idoris nope` | 1 | pass（USAGE + 非零） |
| no_image_video_routes | `rg` `/v1/images` `/v1/video` in `lib.rs` | 无匹配 | pass（缺口确认） |

### coverage_gaps

- 未在 macOS 上解压/校验/启动正式 `v0.3.0` 归档。
- 未启动真实 oMLX，未做真实模型 chat 200。
- 未跑 Agent24 Desktop / PR981/982 代码路径。
- 未声称 #507/#510/#516 已 merge。
- 单元测试 PASS ≠ 产品 / 组合验收 PASS。

---

## 8. 明确非声明

- 本文件 **不是** v0.3.1 发布说明，**不是** 第二份 Agent24 契约。
- **不** 把 PR516 文档修订当作 runtime 已实现。
- **不** 把 catalog 或用户口头模型清单当作已接线。
- **不** 把文本网关存在当作 image/video 能力存在。
- **不** 把 cloud 文档交付当作 acceptance PASS。

---

## 9. 建议下一步（给人，不自动执行发布）

1. 合入并主线验收 #510 角色绑定后，按新语义更新 #507 onboarding / smoke 期望。  
2. 推进 Agent24 #974 / PR981→provider、PR982 预算交付，对照 PR516 consumer pointer。  
3. Owner 关闭 R9 OD-1/2/3；再考虑 launcher / 安装执行。  
4. 干净 macOS arm64：校验包 → 自备权重 → 具体模型调用 → 再评角色路径 → 最后才考虑 `v0.3.1` tag。  

---

*审计执行：Baton task `idoris-cloud-grok-release-wire-audit-20261015`，分支 `baton/grokbot-20261015-v031-release-wire-audit`。*
