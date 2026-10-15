# B0 · 本机 oMLX 没有 `/v1/systemone`

日期：2026-10-15。机器：本机 Apple Silicon。结论：这次 spike 不能产出延迟或准确率。

## 看到的事实

- `~/.omlx/bin/omlx --version` 是 0.6.4。
- 应用内 `/Applications/oMLX.app/Contents/Resources/omlx/_version.py` 也是 0.6.4。
- 同一份 `server.py` 列出的路由是 completions、chat、messages、responses、embeddings、rerank、audio、mcp。全文没有 `systemone`。
- `http://127.0.0.1:8088/v1/models` 返回 401，说明进程在听，本次没有带凭证去打业务接口。
- `~/.omlx/models` 里没有 d3 或 Decision 权重。已有的是聊天、OCR、语音和 embedding 模型。
- 决策文档引用的上游 commit `33e0b0c` 含有 DecisionEngine。这份 0.6.4 安装里没有对应文件。

## 没有做的事

没有升级 oMLX，没有下载 d3-lite，没有把本机凭证写进仓库。这两步都会碰正在使用的推理服务，要单独批准。

## 对后续任务的影响

B2、B3、B4、B5 依赖这次 spike 的线格式和中文筛查，先停。BM、A1、A4、A7 不依赖 d3，继续做。附录 A 只记录这次阻塞，没有性能数字。
