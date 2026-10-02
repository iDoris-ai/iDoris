# 结算待办与恢复（K06）

文件账本路径先解析符号链接并规范化，再追加 `.settlements.sqlite3` 作为持久待办路径。
例如真实文件 `budget.sqlite3` 使用同目录的 `budget.sqlite3.settlements.sqlite3`；
通过真实路径或文件符号链接打开的实例共享该 sidecar。备份与恢复时应停写并同时保留两库；
不能仅恢复主账本或删除 sidecar。内存账本仅用于测试。

成功的上游响应仍正常返回，`ChatOutcome.settlement_status` 区分：

- `Committed`：真实金额已入账，包括 `OverageTooLarge` 已提交的超额金额。
- `Pending`：真实金额已经持久化，主账本暂时不能写入；不发送已入账金额头。
- `PersistenceFailed`：未确认入账或待办持久化；日志包含预留 ID、金额及错误，
  真实费用保留在进程内供重试，准入保持关闭，不能把此状态当作待办已保存。
- `NotRequired`：没有需要结算的已完成付费调用。

`BudgetLedger::retry_settlements()` 在打开账本时执行，也可由维护任务调用；
准入前仅重试当前租户的待办并检查该租户的执行意图。
重放按预留 ID 和实际金额核对已提交结果，避免崩溃后重复扣款。
sidecar 插入失败时，在协调写事务仍持锁的情况下直接尝试主库结算；
结算失败则尝试将实际费用持久化到主库 reservation 的 `actual_cost_minor`。
目标分支旧版 `budget_emergency_settlements` 的持久记录仍会恢复，且不能被新认领或不同费用覆盖。
这些记录同样在启动和准入前重试，尚未成功结算时拒绝该租户的新消费。
已经验证 dispatch 所有权的内存金额先以独立 sidecar 事务提交，再读取主账本和重放；
主表不可读或其他未验证结果核验失败不能回滚这次持久化。
首次归属查询失败时，完成金额按预留 ID 和声明租户分开保留在内存，意图转为待核对。
恢复时先重新验证归属，再保存和补记真实费用；错误租户的结果不能占用真实租户的待办。
一般存储错误和 Busy 保留待办；确定无效的终态、金额或归属冲突会移至隔离表，
保留原因并让其余待办继续处理。已验证的真实费用与 released 状态冲突时，
金额仍持久保留并阻断所属租户，必须核对后处理。

付费 dispatch 在调用上游前必须成功写入 sidecar `settlement_intents`。
若两库均无法保存实际费用，当前进程保留费用并重试；若此时进程退出，
重启后根据未确认的意图拒绝所属租户的准入，TTL 到期也不会解除此保护；
其他预算正常的租户仍可准入。未知结果的准入错误不包含预留 ID。
此时需停写并从日志或上游核对实际金额，再由维护流程修复持久待办。
首次提交 dispatch 费用必须持有本实例的所有权，另一实例不能仅凭租户 ID 提交，
重启后也不能冒充旧所有者。只有确认未执行的调用才可由所有者调用
`release` 或 `release_confirmed_unexecuted`；未确认结果使用 `abandon_dispatch`
撤销本机执行标记并保留持久意图。确认释放仍不能覆盖已知完成金额或持久待办。
请求取消本身不能证明上游未执行。无法写入任何持久存储时，不能承诺自动恢复未知金额。
确认释放会先保存可重试的取消结果；sidecar 恢复后后台重试可持久化确认并释放占用。
未确认的结果则持续阻止所属租户的新准入，不能靠 TTL 到期绕过核对。
Supervisor 明确在调用 adapter 前拒绝的请求走确认释放；进入 adapter 后的错误、
超时、panic 或回包丢失都视为结果未知。不能仅凭通用 Busy 错误码确认未执行。
每次认领使用唯一 token 和 OS 文件锁；其他实例探测到仍持锁的所有者时允许
同租户并发，孤立的意图则阻止该租户继续准入。普通未开始 dispatch 的预留仍遵循原有 TTL 规则。

排查时用 SQLite 打开 sidecar：

```sql
SELECT * FROM pending_settlements;
SELECT * FROM settlement_intents;
SELECT * FROM quarantined_settlements ORDER BY quarantined_at;
```

主库的备用金额可查询 `SELECT id, tenant_id, actual_cost_minor FROM reservations WHERE status IN ('active', 'expired') AND actual_cost_minor IS NOT NULL;`；
旧版备用待办可查询 `SELECT * FROM budget_emergency_settlements;`。

隔离记录恢复步骤：先按预留 ID 核对主账本的 `tenant_id`、`status` 和
`actual_cost_minor`，并核对上游实际用量。若已正确结算，保留核对记录后删除该隔离项。
若预留仍为 `active` 或 `expired`，先核对 dispatch 所有权，再由持有者提交或停写修复待办；
仅在返回已提交或持久待办后删除对应隔离项。`released` 表示调用已取消，
不能直接改成 active 或盲目重新入队；需先查清为何出现矛盾的完成记录。
所有删除都必须限定具体 `reservation_id` 和 `tenant_id`，不要清空隔离表。
