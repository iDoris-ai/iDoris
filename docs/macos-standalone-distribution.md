# macOS 独立 CLI 签名发行

`scripts/macos_signed_release.py` 消费已经构建的 iDoris Mach-O 和四份公开配置，产出独立下载用的签名、已公证并附票的 DMG。它不调用 Cargo，不创建凭据，不安装服务，不上传 Release，不更改已有 tag/assets；v0.3.0 保持不可变。Agent24 外层签名不能替代此发行链，也不需要新增 app launcher。

## Apple 依据（2026-10-15 核对）

- [Customizing the notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow)：公证服务接受 UDIF disk image、signed flat installer package、ZIP。DMG 可以 staple；ZIP 和裸 Mach-O 不能直接 staple。因此这里选择 DMG，不能把 tar.gz 或裸二进制宣称为已附票。
- [Resolving common notarization issues](https://developer.apple.com/documentation/security/resolving-common-notarization-issues)：CLI/Mach-O/DMG 使用 Developer ID Application；可执行文件需要 secure timestamp 和 hardened runtime。此工具不添加调试或宽松 entitlement。
- [Testing a Notarised Product（Apple DTS）](https://developer.apple.com/forums/thread/130560)：DMG 用 `spctl --assess --type open --context context:primary-signature`；非 app 的代码用 `codesign --verify -R=notarized --check-notarization`。脚本同时核对挂载后的实际 payload。
- [Resolving Gatekeeper Problems（Apple DTS）](https://developer.apple.com/forums/thread/706379)：Finder 双击 CLI 与 Terminal 执行路径不同，可能触发文档检查问题。本产品按 CLI 手动运行，不承诺 Finder 双击启动。

## 操作者输入与边界

要求 macOS、Python 3.9+、Xcode command-line tools（`xcrun notarytool`/`stapler`）、已有且有效的 Developer ID Application keychain identity、已有 notarytool profile alias，以及网络连接。identity 输入是证书 SHA1 指纹，team ID 必须与实际证书、签名元数据一致。profile alias 由操作者确认；未知时停止，不猜测、不创建 profile。不要传密码、私钥、个人账号或在日志中 dump 环境。

输入 binary 只接受指定架构的 thin 64-bit Mach-O executable（aarch64 或 x86_64）；不接受 tar/ZIP、脚本、dylib、fat/universal。`--binary-sha256` 固定构建产物字节，签名只修改 staging 副本。`--source-sha` 和 `--version` 是构建负责人提供的来源声明，脚本不能从任意 Mach-O 证明源码 SHA/语义版本；构建 provenance 和精确版本 `/health` smoke 是发布前置门禁。

`--config-root` 下仅复制 `components/omlx.yaml`、`components/subscription.yaml`、`routing-policy.yaml`、`catalog.yaml`，不会遍历打包整个仓库。调用者必须提供已审核的公开配置；工具不判断 YAML 中是否含秘密。输入路径不得经过符号链接；输出必须是一个不存在的新目录，父目录必须已存在且不经过符号链接（macOS 临时目录可先解析物理路径）。任意原始文件名都通过 subprocess argv 传递，不经过 shell。

以下变量由构建/发行负责人提供；这不是实际签名记录。先加 `--plan` 可只验证输入并打印 `acceptance: not evaluated`，不会调用 Apple 工具、签名、公证或生成产物。正式执行移除该选项。

```sh
python3 scripts/macos_signed_release.py \
  --binary "$BUILT_IDORIS" \
  --binary-sha256 "$BUILT_SHA256" \
  --config-root config \
  --version 0.3.1 \
  --source-sha "$SOURCE_COMMIT" \
  --arch aarch64 \
  --identity "$DEVELOPER_ID_APPLICATION_SHA1" \
  --team-id "$DEVELOPER_TEAM_ID" \
  --notary-profile "$EXISTING_NOTARY_PROFILE" \
  --output "$NEW_ARTIFACT_DIRECTORY" \
  --plan
```

## 执行顺序及失败处理

1. 校验参数、输入 SHA256、Mach-O 架构/type、配置路径和输出位置；查询有效 identity 并核对 Developer ID Application/team。
2. 复制 CLI 和配置；用 `codesign --timestamp --options runtime` 签 CLI，验证严格签名、team、authority、timestamp、runtime。
3. `hdiutil create -format UDZO` 生成 UDIF DMG，再用同一 identity 和 timestamp 签 DMG 并核对签名。
4. `notarytool submit --keychain-profile … --output-format json`，只将上传响应中的有效 submission UUID 视为收据，立即调用 `info` 再查询直到 `Accepted`；拒绝、未知状态、坏 JSON、ID 不一致或超时均停止。默认每 15 秒查询、接受期限一小时，这是一次发布的公证事务轮询，不是 LoopX 定时状态检查。
5. staple、validate、再次严格核对 DMG 签名、`hdiutil verify` 和 `spctl` open assessment；只读挂载，核对 CLI/四配置内容哈希、CLI 签名和公证状态，再 detach。attach 发生挂载副作用后即使超时也尽力 detach 本次挂载；主错误保留，清理错误追加。已有挂载路径直接拒绝，不卸载别人的挂载。
6. **最后**计算附票后 DMG 的 SHA256，写 checksum 与公开 manifest，随后以独占 mkdir 预留最终输出目录，再用 O_EXCL 复制三个产物并复核摘要与 manifest；遇到目录竞争时失败，不覆盖现有内容。失败只清理本次创建的未完成文件，保留其他写者的 sentinel。manifest 同时记录输入与签名后 CLI SHA256、配置 SHA256、source/version 声明、submission UUID 和实际通过的工具检查。

输出名称固定为 `idoris-v0.3.1-aarch64-apple-darwin.dmg`、同名 `.sha256`、`.manifest.json`（架构/版本随显式参数变化）。签名时间、公证票据和文件系统使字节不保证可复现；固定的是名称和输入来源约束。manifest 本身不是签名证明，下载者应检查 DMG 签名/附票以及可信来源的 checksum。

每条子命令默认 300 秒上限（可用 `--command-timeout` 调整），stdin 关闭，输出在临时文件中捕获，读取最多 1 MiB。错误只报告阶段、退出码或已验证的 submission UUID，不输出原始工具诊断、路径、alias、证书姓名或环境；更详细的 Apple 日志应在操作者本机私下查看。公证期限检查在请求之间进行，最后一条命令最多另占一个 command timeout。超时退出不取消 Apple 后台任务，UUID 已提交后可用现有 profile 查询 `notarytool info/log`，不要盲目重复提交。上传响应丢失时 UUID 可能未知，应先本地查 notarytool history。

失败不发布最终输出目录，无 unsigned fallback。挂载后的失败会尝试 detach；如果 attach 超时或 detach 失败，操作者应本地检查残留挂载，解决后使用新输出目录重试。不要删除 quarantine、禁用 Gatekeeper 或绕过失败。

## 最小 workflow 接入（由协调者实施）

本切片不修改 `.github/workflows/release.yml`。保留现有已评审源码/tag ancestry、workspace tests、精确源码构建和版本门禁，在 `Package binary + sha256` 位置调用上述工具；先验证 exact v0.3.1 build，再对这个相同 binary 签名。已有身份/profile 的受控签名环境由协调者提供，不要求临时生成新身份。upload-artifact/release job 改为收集 DMG、最终 checksum、manifest；只发布新 v0.3.1 资产，不能覆盖 v0.3.0。

现有 `scripts/release-smoke.py` 只接受五个 regular-file 成员的 tar.gz，解包后从空 cwd、清除 IDORIS_* 环境启动并检查 `/health` 的版本、状态和组件数。它不能直接接收 DMG，也不能作为 v0.3.1 新行为证明。后续 hook 应将启动/health 检查抽成“已验证 payload 目录”的公共入口，DMG 只读挂载后调用；保留 tar.gz 成员安全检查供旧格式使用，不要把 DMG 改名为 tar.gz。

在最终 v0.3.1 source cut 上，另需运行新 native 参数透传和 deadline/cancellation 的产品门禁、Agent24 实际 provider wiring/请求路由验收，再检查签名后挂载或手动复制的同一 CLI 的启动及精确 `/health` 版本。当前 `scripts/smoke-agent24.mjs` 仍引用旧 `packages/router/dist/index.js` 和模拟 upstream，只是 iDoris 侧契约验证，不能替代 Agent24 仓库对真实 Rust v0.3.1 的接入证明。已有 v0.3.0 smoke 不可计作这些新门禁通过。

下载验收还需在从未见过该产品的干净 macOS/VM 上用 Safari 获取 DMG，保留 quarantine，断网测试附票后的挂载、按现有手动流程复制 CLI+config、从 Terminal 启动。随后联网执行需要网络的产品 smoke。CLI 安装位置、配置和进程管理继续由现有手动流程负责。工具检查与单测均不能代替这份用户路径验收。

## 本切片验证

```sh
/usr/bin/python3 --version
python3 --version
/usr/bin/python3 -B -m unittest discover -s scripts -p test_macos_signed_release.py -v
python3 -B -m unittest discover -s scripts -p test_macos_signed_release.py -v
/usr/bin/python3 -B scripts/macos_signed_release.py --help
python3 -B scripts/macos_signed_release.py --help
git diff --check
```

行为测试使用 Apple tool doubles 覆盖各阶段错误后停止、拒绝和公证 deadline、坏响应、非法输入/路径/归档、挂载内容损坏、最终附票字节 checksum 顺序、plan 不调用工具、真实子进程失败/超时不泄漏输出。它们不是真实 codesign、公证服务、stapler 或 Gatekeeper 验收。本切片未执行任何签名、上传公证、credential 修改、Cargo build、tag、Release 或发布动作；实际发行仍等待 v0.3.1 产品门禁与受控签名运行。


### Python 兼容性与异步提交回归（2026-10-15）

在基线 `cf013947` 上实际使用 `/usr/bin/python3` 3.9.6 运行原 14 项测试，复现 10 个 `hashlib.file_digest` 不存在的错误；该 API 不适用于系统 Python。现改为每次最多读取 1 MiB、逐块更新 SHA256，覆盖空文件、短文件和跨块文件。fixture 临时根已使用 `Path.resolve()`；生产 symlink/path guards 保持原样。

新增 ID-only、id/message/path 上传收据、后续坏 `info`、缺失/非法状态回归后，在修复前的 Python 3.11.9 上实际得到 18 项中 5 failures 和 1 error；修复后上述两个解释器均通过全部 18 项行为测试和 CLI help。完整流水线的 Apple double 也改为返回不带 status 的上传收据，避免只有独立单元测试覆盖真实异步流程。

本机只读核对了 `xcrun notarytool --version`（1.1.2 build 41）、`submit --help`、`info --help` 和 `man notarytool`：默认 `--no-wait` 只等上传，submit 返回 UUID，info 才负责获取处理状态，JSON 是支持的输出格式。前述 Apple workflow 文档也分别展示只有 id/path 的上传收据及带 status 的处理结果；这些文档没有承诺所有版本完整 JSON 键集合，因此解析只依赖有效 UUID，忽略 message/path，始终从同 ID 的 info 响应确认 Accepted。submit 若显式携带未知/拒绝状态同样停止；info 缺失状态绝不视为接受。此核对未请求凭据、未执行真实提交，真实 Apple 服务与用户下载路径验收仍待协调者完成。
