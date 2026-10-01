# 结算待办与恢复（K06）

文件账本 `budget.sqlite3` 的持久待办位于同目录
`budget.sqlite3.settlements.sqlite3`。备份与恢复时应停写并同时保留两库；
不能仅恢复主账本或删除 sidecar。内存账本仅用于测试。

成功的上游响应仍正常返回，`ChatOutcome.settlement_status` 区分：

- `Committed`：真实金额已入账，包括 `OverageTooLarge` 已提交的超额金额。
- `Pending`：真实金额已经持久化，主账本暂时不能写入；不发送已入账金额头。
- `PersistenceFailed`：未确认入账或待办持久化；日志包含租户、预留 ID、金额及错误，
  真实费用保留在进程内供重试，准入保持关闭，不能把此状态当作待办已保存。
- `NotRequired`：没有需要结算的已完成付费调用。

`BudgetLedger::retry_settlements()` 在打开账本和准入前执行，也可由维护任务调用。
重放按预留 ID 和实际金额核对已提交结果，避免崩溃后重复扣款。
sidecar 插入失败时，在协调写事务仍持锁的情况下直接尝试主库结算；
结算失败则尝试将实际费用持久化到主库 `budget_emergency_settlements`。
这些记录同样在启动和准入前重试，尚未成功结算时拒绝新消费。
一般存储错误和 Busy 保留待办；确定无效的终态、金额或归属冲突会移至隔离表，
保留原因并让其余待办继续处理。

付费 dispatch 在调用上游前必须成功写入 sidecar `settlement_intents`。
若两库均无法保存实际费用，当前进程保留费用并重试；若此时进程退出，
重启后根据未确认的意图拒绝准入，TTL 到期也不会解除此保护。
此时需从日志或上游核对实际金额，再调用 `settle_durable`；只有确认未执行的调用
才可 `release`。无法写入任何持久存储时，不能承诺自动恢复未知金额。
另一账本实例也将正在执行的意图视为待核对，因此共享账本的实例会保守地暂停准入，
直到该调用结算或释放。普通未开始 dispatch 的预留仍遵循原有 TTL 规则。

排查时用 SQLite 打开 sidecar：

```sql
SELECT * FROM pending_settlements;
SELECT * FROM settlement_intents;
SELECT * FROM quarantined_settlements ORDER BY quarantined_at;
```

主库的备用待办可查询 `SELECT * FROM budget_emergency_settlements;`。

隔离记录恢复步骤：先按预留 ID 核对主账本的 `tenant_id`、`status` 和
`actual_cost_minor`，并核对上游实际用量。若已正确结算，保留核对记录后删除该隔离项。
若预留仍为 `active` 或 `expired`，使用正确租户和金额调用 `settle_durable`；
仅在返回已提交或持久待办后删除对应隔离项。`released` 表示调用已取消，
不能直接改成 active 或盲目重新入队；需先查清为何出现矛盾的完成记录。
所有删除都必须限定具体 `reservation_id` 和 `tenant_id`，不要清空隔离表。
