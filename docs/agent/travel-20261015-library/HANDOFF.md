# 合盖接棒：iDoris，2026-10-15

## 两条车道与可检查完成判据

只处理 iDoris，不进入 Agent24 工作区。Fast/priority 禁止；Codex default/standard。不得启动本机或 Mini PR Review Watcher，远程 PRD 是批准来源。夜班不得合并 PR、改 main、发布/改 tag、force-push 或覆盖已有脏工作。当前仓库任务分支由 Mini 独占；Grok Bot 只在自己的 baton/grokbot 分支提交。不能调用无关任务或用另一车道绕过平台拒绝。

Mini：迁移历史 PR #485 的独立防御测试到当前 main 基线，保留其中 9 个端点拒绝/默认无认证矩阵。先取 gh pr view 485 的原始 diff/base/head，逐项对照；对比 #513 中已迁移的 #483/#484，保留独立覆盖，不重复生产实现。只允许必要测试/测试 helper 与接棒验收报告，不改生产认证规则。实现位于记下的 ops/travel-20261015-library 分支，不更新历史 PR。运行实际目标 Rust 测试及 fmt/受影响 clippy，记录原始历史断言到新测试的映射、命令、结果和准确 SHA，提交并推送自己的分支后停止。完成判据：有已推送提交、#485 全部独立断言覆盖证据、实际测试通过；真实阻塞则记录后停止，禁止假报完成。

Grok Bot：只独立复核下面已保存的 #507/#479/#510 修复增量，不修改生产代码、不更新三个 PR。分别读取原 reviewed head 到保存 head 的 diff，检查报告中行为证据和遗漏；可在云电脑的独立 checkout 运行最小相关测试，不下载模型、不调用付费模型 API。特别关注 #479 nullable cap / n 未乘的既有风险、#507 late-backend 重启真实行为、#510 订阅固定 relay 与协议 model 绑定和 conformance。向自己的分支写中文独立复核报告及 baton-decisions.tsv，列准确 head、实测与仅源码结论、阻断/非阻断发现。完成判据：三项逐项有证据的独立报告已推送；阻塞有清楚原因。不改 Mini 的分支，不 SSH 到 Mini，不重复 #485 迁移。

每次 status/CI/review 检查间隔 5 分钟；无变化不重跑测试、不再派工。不得据 raw tokens 推算金额，未知 usage 就写未知。保留 disabled jobs。每次 scheduled check 末行用真实布尔写 LOOPX_CHECK_RESULT {"material_change":false,"advanced":false}（有真实变化才改值）。用户离场不是停工，但夜班按 baton 每轮90分钟、最多4轮/真实额度闸门。

## 发布与主线已发生事实（快照，需 gh 复核新变化）

v0.3.0 已发布：https://github.com/iDoris-ai/iDoris/releases/tag/v0.3.0 。不可变 tag source 03c079c70611331a8c85a1666e32a75df6359d09。macOS arm64 tar.gz CLI（非 DMG/EXE、无模型/后端），正式 sha256 ce54063f78b2d092c0ba820be4729635a5fe1df2ec91debeec10ef5dd582dfad。
#508 已以当前 head APPROVE、9/9 CI、正常 merge 合并，main=959337d444a9b0df371e43a7e0a2854ae894f8e3。不是 v0.3.0 发布产物内容。v0.3.1 没发布，真实 Agent24 provider 接线尚未实现完，不得宣称 M4/两端集成全验收结束。

## 本机修复已提交但尚未更新原 PR：已保存独立远程分支

- #507 原 reviewed a232f8391107263408589a5c846dda524ebd0421；保存 a69f954293c0dcddc4195dd08debdefe9fbd8d57，远程 handoff/20261015-pr507-fix。含 main959、启动后端顺序、晚启动需 restart、真实 forwarded alias、capabilities Bearer 等；实际正式030二进制 smoke 和 latebackend 通过。
- #479 原 reviewed b8f3214d4bc79a319f5b722015d68b4ead71c61c；保存 e3426f657328d21f7afd8901206de7593f02de1a，远程 handoff/20261015-pr479-null-fix。null 当 absent，非 null 严格 u64<=2^53；supervisor_parameters 4及 budget41、clippy/fmt 通过。n multiplier 与任意汇率浮点金融精度是明确未解决限制。
- #510 原 reviewed bbf99ec34950867814ebb57ba33763dc8d96d536；保存 f4651f3c00aa6ba64f8564925ddbcdfcce9464f1，远程 handoff/20261015-pr510-ci-fix。含 main959；8 role 控制与真正 scripts/conformance-rust.sh、scripts/check-subscription.sh 通过，每实现136 pass/8原 TODO；Linux strace 未在 macOS 跑。
- 权威接口 docs 修订保存 a1a08c003decda0f6a710dfc2f13c162429a8bd2，远程 handoff/20261015-contract-v12-wip。仅3docs，doc rev1.2、wire仍1.0.1。未发布且需先修正文中 #508 仍未合并的过期状态。Agent24 consumer pointer 尚未提交；OD1/2/3 是提案未擅自批准。

这四个保存分支只是传递已提交成果，没有更新原 PR head，也不代表新 CI/外部批准。回收后由协调者 source review、正确 pre-pr-check、确认无 pr-daemon:reviewing 再一次推完整批次，并更新精确 finding/检测行。不可把保存当已落地。

## 活跃 PR 与历史清理

#509 历史73逐项审计，最新8609321f532b91f2744d3ed51ede656046395ded。完整公开表 docs/agent/PR-DISPOSITION-2026-10-15.md。原73中已关闭8：422/462/464/466/481/486/487/488，理由是明确历史归档或有完整 active carrier，绝非全当已合并。余65原队列：61 M5含491 CI、4 M4测试483/484/485/496。
#513 storage replacement=a3cc0c77336766457faff430c829c235ed0c323c，完整迁移483/484，独立验收通过，本机与 main959 composition已测；外部审核/合并未完成。#514 paid event replacement=0e5fee082bf5863c5e8c33a2046bbb7b82d76379，覆盖496，实际正向reservation后event失败503/零egress/账本delta0，独立通过。485仍需迁移。不要先关闭/删除有未承接内容的历史工作。
#511 budget foundation=e1187aa888d84e0fee1ef35257fe3b8a1dd43e67；#512 wiring=168502be577a8cc6fe7567cda509ff32c165d506（base511），预算 exact per-model CapacityCharge(u128)、prospective global plan、CAS、一致 admission；两者组合旧 main 上 tree=35567b6b8a7bb3de3caaba42d65e86c946c734f7，最终独立接受。需要按 reviewer label 许可吸收新 main959到511再512及复审，不能宣称当前已在 main。
#425 input_limit_exceeded spanless/BLOCK 全 schema/Rust/Zod 修复58a7ed已批准；后又只backport已批准main Rustls0.23.46+已有 unsafe-libyaml unmaintained例外，最新a5b3272已推，cargo deny offline locked advisories ok；需新head复审/CI。禁止扩大 advisories ignore。原 base 闭合栈未来仍需迁移。
#479 b8旧修复已普通 merge/atomic push传播到480、闭合481（489仍用base）、489/490/492/497；null新修复e342尚未传播，回收后需依赖图顺序验证推进，不能force/盲目merge whole main进老M5栈。

已删远程无消费者分支464/466/486/487，保留所有依赖基分支，备份在笔记本 ~/.codex/backups/idoris-postrelease/20261015-pr-reconciliation/unused-closed-pr-heads.bundle（4,278,765bytes）。此前13worktrees已清；不要再从 branch名推测可删，必须dirty/consumer/head/backup证明。Agent24本机有7无关untracked，保留。

## 后续跨仓库设计（本夜班不越界）

iDoris 规范做唯一跨仓库权威，Agent24短说明+链接。Agent24负责硬件事实/整机allocated预算；iDoris catalog推荐、精确model ID、lifecycle/预算内admission。不把物理RAM改成allocated，不重复扣OS余量。None保留兼容。
Agent24目前没有完整 data-plane provider，只有Admin消费者；不得说已有真实模型选择调用。可信共同launcher另议，不把daemon归Agent24；Admin64hex只匿名FD/pipe，不进argv/env/file/log/renderer；cloud默认off、local_only loopback，4类凭据分开。30s/120s timeout gap仍需实现。M5 REDACT仅本地console用户单次确认，caller长期授权未来再做。
Agent24 AGENTS要求task branch ab/<direction>-NN，PR base integration、worktree~/Dev/auraai、integration exact protected且最终main PR人工开并真机验收。ab/idoris未被 ruleset保护、未创建，不能直接绕过写main，不能无许可往Mini派Agent24任务。#974协作issue、iDoris#506总协调issue。

## 本机恢复索引

LoopX goal idoris-m4-v030-release-goal（名字历史，实际包含031/M5）。todo06f8c4c677a6 integration、70161a843695 auth已508merged待语义收口、2f5d852bb0e0 historical、1a372148bf84持续monitor。quota guard idoris-pr-reconcile-20261015T0720 尚未refresh/spend，旧guard已花1slot；不能编造usage。旧automation73242508-4821-465d-8f3d-90ffa6efcf8f disabled保留，新031job尚未创建，private check_material.py并非已运行计划。
Orca run run_9ecca15324a6：4本机worker已完成并归档release，避免假装合盖后仍由本机运行。根root checkout仍docs/handoff-2026-10-11-night，不切换不覆盖。
原始详细证据在本机 /tmp/idoris-pr-disposition-20261015、/tmp/idoris-postrelease-coordination。本包的 evidence 子目录复制关键作者与独立报告以便远端真能读取；/tmp日志未复制就不能声称云端已有。
