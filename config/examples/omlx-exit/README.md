# Mac 上 oMLX 不可用时恢复文本 chat（A7）

从仓库根目录执行。选择 Ollama 或 `mlx_lm.server` 中的一条路径。
这份 runbook 只切换显式组件卡，不修改默认配置或自动探测。

## 准备已验证的本地引擎和模型

1. 记录原来的 `IDORIS_COMPONENTS_DIR`、`IDORIS_LOCAL_RUNTIME_CONFIG`、引擎版本和模型 revision/量化，保留原卡。停止待切换的 iDoris 实例。
2. 准备已安装且通过本机验证的引擎和完整本地模型。Ollama 的模型工件与 MLX 权重不能互换。模型下载须在恢复服务前单独完成；以下命令不下载权重。
3. 将所选卡的 `version_pin` 中 `REPLACE_WITH_TESTED_VERSION` 换成实际验收版本。该字段是声明，启动校验不会探测安装版本或验证权重。版本升级按 [docs/18 §2.1 的 FU-16 清单](../../../docs/18-推理后端与决策模型-vLLM与Decision3.0评估.md#21-omlx-依赖清单与退出边界a7) 复测。

## 路径一：Ollama

使用 [ollama/components/ollama.yaml](ollama/components/ollama.yaml)。在独立终端启动已有的 Ollama，保持 loopback，并关闭云模型及云功能。若已有服务占用该端口，先确认其配置，不再起第二个服务。

```sh
OLLAMA_HOST=127.0.0.1:11434 OLLAMA_NO_CLOUD=1 ollama serve
```

确认启动日志包含 `Ollama cloud disabled: true`。该设置来自 [Ollama FAQ](https://docs.ollama.com/faq#how-do-i-disable-ollamas-cloud-features)。所用版本必须支持它。
在运行 iDoris 的终端选择此卡目录。

```sh
export IDORIS_COMPONENTS_DIR="$PWD/config/examples/omlx-exit/ollama/components"
exit_upstream=http://127.0.0.1:11434
```

## 路径二：mlx_lm.server

使用 [mlx-lm/components/mlx-lm.yaml](mlx-lm/components/mlx-lm.yaml)。在独立终端启动已安装的 MLX-LM，替换绝对模型路径。
这里只用本地工件；不启用 remote code。

```sh
HF_HUB_OFFLINE=1 mlx_lm.server \
  --model /ABSOLUTE/PATH/TO/MLX-MODEL \
  --host 127.0.0.1 --port 18101
```

参数依据 [MLX-LM server 源码](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/server.py)。在运行 iDoris 的终端选择此卡目录。

```sh
export IDORIS_COMPONENTS_DIR="$PWD/config/examples/omlx-exit/mlx-lm/components"
exit_upstream=http://127.0.0.1:18101
```

如需 iDoris 托管 `mlx_lm.server` 的进程生命周期，使用另一个 [multibackend 样例](../multibackend/README.md)，不能把它的 runtime 配置绑定到这里的 resident 卡。

## 核对模型并启动 iDoris

1. 查询选定服务的模型列表，记录返回的完整 `id`。Ollama 必须使用已在本地安装的模型，不能使用 cloud 模型。mlx_lm 使用该服务返回的 ID，可能是完整本地路径。

   ```sh
   curl --noproxy '*' --fail --silent --show-error "$exit_upstream/v1/models"
   ```

2. 清除原来的进程托管配置，使用单一退路目录启动。保留现有 virtual key、存储与预算配置。`endpoint` 是服务根地址，不附加 `/v1`。

   ```sh
   unset IDORIS_LOCAL_RUNTIME_CONFIG
   IDORIS_BIND_HOST=127.0.0.1 IDORIS_PORT=8080 \
     IDORIS_ROUTING_POLICY="$PWD/config/routing-policy.yaml" \
     IDORIS_CATALOG="$PWD/config/catalog.yaml" \
     idoris serve
   ```

3. 在另一个终端检查 `/health` 返回 `status: ok`、`components: 1`，再查询 iDoris 的 `/v1/models`。使用已有 virtual key，不把 key 写进卡或日志。

   ```sh
   curl --noproxy '*' --fail --silent --show-error http://127.0.0.1:8080/health
   curl --noproxy '*' --fail --silent --show-error \
     -H "Authorization: Bearer ${IDORIS_CLIENT_KEY:?set an existing virtual key}" \
     http://127.0.0.1:8080/v1/models
   ```

4. 将下面的 model 占位符换成步骤 1 返回的完整 ID，发送一次非流式文本请求。确认响应内容与模型身份符合预期。resident 代理原样转发 model；这里使用具体 ID，不用 `idoris/daily` 或 `idoris/auto`，也不用卡的 `ollama-exit` / `mlx-lm-exit` 注册 ID。

   ```sh
   curl --noproxy '*' --fail --silent --show-error \
     -H "Authorization: Bearer ${IDORIS_CLIENT_KEY:?set an existing virtual key}" \
     -H 'Content-Type: application/json' -H 'X-iDoris-Privacy: local_only' \
     http://127.0.0.1:8080/v1/chat/completions \
     -d '{"model":"REPLACE_WITH_EXACT_SERVED_MODEL_ID","messages":[{"role":"user","content":"你好"}],"stream":false}'
   ```

## 核对退化与回退

两张卡只声明 `chat`、`local_only`、loopback egress 和 fail-closed。`resident` 选择现有 `ChatProxy` 直连；`pinned: true` 是卡契约要求，不是引擎 pin 的实测结果。iDoris 不在此路径启动或停止引擎，也不提供 Supervisor 的 load/unload、pin 后置条件、驱逐和内存压力准入。按所用模型、上下文和并发预留内存，并自行维持服务存活。

此退路不提供 embedding、rerank、Anthropic messages 或 `/v1/systemone`。必要 judge 不可用时保留失败语义，不能用 chat 模拟决策结果。

若模型列表、真实 chat 或资源检查不通过，停止该 iDoris 实例，恢复步骤 1 保存的两个配置值，并用旧 pin 对应的已验证 oMLX 服务重启。仅停止本次自行启动的退路服务。oMLX 仍不可用时保持故障状态，不自动转到远程服务或修改隐私限制。

## 重跑启动校验

```sh
cargo test --locked -p idoris-router --test portable_startup omlx_exit_example_cards_pass_startup_validation
```

测试直接从磁盘加载这两个原始卡目录，经过组件注册校验，再分别启动真实 `idoris`，断言 `/health` 为 `ok` 且加载了一个组件。测试不启动推理引擎，不下载权重，也不发真实推理请求；通过仅证明样例的启动校验，实机可用性仍由以上模型和 chat 检查验收。

HTTP 子集依据 [Ollama OpenAI compatibility](https://docs.ollama.com/api/openai-compatibility) 和 [MLX-LM server 实现](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/server.py)。共享套件的端点边界见 [conformance 分类](../../../conformance/README.md#上游依赖分类a7)。
