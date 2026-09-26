# @idoris/contracts — 契约成熟度

> 真源：[`schema/*.schema.json`](schema)（JSON Schema，D-3 授权）。
> 产物：[`src/generated/*.ts`](src/generated)（`node scripts/gen-contracts.mjs`）。
> 零漂移门：`pnpm check:contract-drift`。分级口径见 [`docs/06 §10.10`](../../docs/06-组件接口契约与互换标准.md)。

| 契约 | 级别 | 说明 |
|---|---|---|
| ProviderDescriptor | **L1** | 强制字段+语义；完整 I/O 与黄金测试待 U1 |
| ComponentCard | **L1** | 另有 `validateComponentCard` 策略校验（T1.1.3，涉安全） |
| LoadPolicy | **L1** | 抽象语义；mock 适配器（T1.2.1）后具备可替换凭证 |
| RoutingPolicy | **L1** | 声明式、版本化；解释引擎见 T1.3.2 |
| TaskProfile | **L1** | 控制面 → 内部表示；保守默认 `local_only` |
| TenantContext / DeployMode | **L1** | 对外契约 [`docs/agent/contract-tenancy.md`](../../docs/agent/contract-tenancy.md) v1 |

> 口径：声明「可替换」**只对达到 L2 + L3 的能力成立**；未达的诚实标注 L0/L1，不假装。
