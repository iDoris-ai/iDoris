# 未发布。意图识别只在 idoris/auto 下运行

这次改动还没有发布标签。它不升契约版本。

## 调用方会看到的变化

没有 `X-iDoris-Intent` 时，只有 `model` 为 `idoris/auto` 的请求会做意图识别。

下面这些请求不再识别意图。`intent` 保持默认 `chat`。角色、能力和调用方写下的模型 ID 保持原样。

- `idoris/daily`、`idoris/fast`、`idoris/deep`、`idoris/vision`、`idoris/embed`、`idoris/rerank`、`idoris/decide`
- 不透明模型 ID
- 请求里没有 `model`

带了 `X-iDoris-Intent` 的请求，无论 `model` 是什么，都不做意图识别。空白或非法的该头仍然是 400。

只带 `X-iDoris-Complexity` 不会关掉 `idoris/auto` 的意图识别。识别结果可以改 `intent`，不能改已经声明的 complexity。complexity 目前只记录 header 或 default 两种来源，没有 detected 来源。

`local_only` 在聊天、自动路由和调用方声明三种模式下都会去掉远程候选。

仓库自带的 `config/routing-policy.yaml` 不按 `web_search`、`coding`、`email`、`agent_task` 选模型。聊天模式在这份默认策略下的选择与改动前一致。

## 自定义策略需要改的地方

如果一份自定义路由策略靠「非 auto 请求被自动识别成 coding、email、web_search 或 agent_task」来选模型，这份策略现在不会再命中。要恢复那个效果，把 `model` 改成 `idoris/auto`，或者自己发送 `X-iDoris-Intent`。

显式角色不会被改写成 daily 或 chat。`idoris/daily` 再带 coding 意图，仍然留在 daily，不会因此换成别的角色。

回滚时恢复原先的识别调用条件即可。complexity 的 header 与 default 来源是内部元数据，不改写调用方已经声明的 complexity。
