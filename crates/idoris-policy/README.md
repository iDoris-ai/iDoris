# idoris-policy

R2-B：纯函数的准入与路由决策管道。**没有 IO，没有随机数** —— 每个阶段都是
`(输入) -> 输出` 的纯函数，同样的输入永远得到同样的输出（proptest 覆盖）。

移植自（并且必须与之行为等价）——这些 TS 文件目前在 `main` 分支，本 crate
所在的 Rust 骨架分支线尚未合并它们，读取时用 `git show origin/main:<path>`：

| Rust 模块 | TS 参考实现 | 职责 |
|---|---|---|
| `role` | `packages/router/src/roles.ts` | `idoris/<role>` 解析、目录角色候选筛选 |
| `card` | — | 决策候选（`ComponentCard` + 角色/硬件/准入元数据） |
| `privacy` | `packages/router/src/locality.ts` | Served-Locality 唯一口径、隐私下限只收紧不放宽 |
| `registry` | `packages/router/src/registry.ts` | 组件卡注册前的纯校验（不含文件系统/沙箱门禁） |
| `budget` | `packages/tenancy/src/budget.ts` | 只读预算查询接口（`BudgetView`），真正的原子 reserve 属于另一个 crate |
| `pipeline` | `packages/router/src/{policy,dispatch}.ts` | 管道入口 `decide()`：隐私 → 角色/能力匹配 → 预算 → admission → 选择 |

## 顺序即语义

`docs/iDoris-总体规划.md` 不变式 #1、接口规范 v1.1 §3.13：

```
隐私 → 角色/能力匹配 → 预算（只作用于付费候选）→ admission → 选择/降级
```

顺序反了就是漏洞：意图/角色匹配绝不能放宽隐私要求；预算只闸付费候选，绝不
在无声地剔除远程候选后改选本地——除非调用方显式声明 `Fallback`（对应
`X-iDoris-Fallback` 请求头），且这次降级必须体现在返回的 `Decision` 里。

## 不做什么

- 不做任何 IO（文件系统、网络、时钟）。
- 不做原子预算 reserve/charge（`budget::BudgetView` 只是只读查询接口）。
- 不做订阅 provider 的部署模式/沙箱门禁（那是 `registration.ts` 的运行时职责，
  依赖 `process.env`，本 crate 不接触环境变量）。
