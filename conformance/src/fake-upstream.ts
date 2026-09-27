/**
 * 假的 OpenAI 兼容上游（node:http，不依赖被测实现的任何代码）。
 *
 * 被测服务（TS 参考实现或将来的 Rust 版）把请求转发到这里的
 * `/v1/chat/completions` / `/v1/models`；测试通过 `queueChat()` 注入
 * 500 / 慢响应 / 挂起不回，再用 `chatCount()` 断言"远程出站 N 次"。
 */
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";

export interface ChatBehavior {
  kind: "json" | "sse" | "hang";
  status?: number;
  body?: unknown;
  /** 响应前先等这么久（模拟慢响应）；hang 用不到，因为它压根不响应。 */
  delayMs?: number;
  sseChunks?: string[];
  sseChunkDelayMs?: number;
}

export interface ReceivedRequest {
  method: string;
  path: string;
  bodyText: string;
}

export interface FakeUpstream {
  readonly url: string;
  readonly port: number;
  chatCount(): number;
  modelsCount(): number;
  /** 最近一次 hang 请求是否被客户端断开（用于验证取消传播）。 */
  wasChatAborted(): boolean;
  /** 先进先出：下一次 /v1/chat/completions 命中队首；队空则用默认成功响应兜底。 */
  queueChat(behavior: ChatBehavior): void;
  setModels(models: Array<{ id: string }>): void;
  requests(): readonly ReceivedRequest[];
  close(): Promise<void>;
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

function defaultChatBody(): unknown {
  return {
    id: "chatcmpl-fake",
    object: "chat.completion",
    choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
  };
}

export async function startFakeUpstream(): Promise<FakeUpstream> {
  const queue: ChatBehavior[] = [];
  let models: Array<{ id: string }> = [{ id: "fake-model-1" }];
  let chatCount = 0;
  let modelsCount = 0;
  let aborted = false;
  const received: ReceivedRequest[] = [];

  async function handleChat(req: IncomingMessage, res: ServerResponse): Promise<void> {
    chatCount += 1;
    const behavior = queue.shift() ?? { kind: "json" as const, status: 200, body: defaultChatBody() };
    if (behavior.kind === "hang") {
      req.on("close", () => {
        aborted = true;
      });
      return; // 故意永不响应：模拟上游挂起。
    }
    if (behavior.delayMs !== undefined && behavior.delayMs > 0) await sleep(behavior.delayMs);
    // 客户端可能在慢响应/流式分片中途断开（被测服务把取消传播过来时就是这样）；
    // 断开之后再 res.write()/res.end() 会在底层 socket 上抛错。这不是"上游出了
    // 什么问题"，是预期之内的正常收尾，不该让假上游这个共享 vitest worker 进程
    // 因为一次未捕获异常就整个崩掉、殃及同一个 worker 里其它测试文件。
    if (res.writableEnded || res.destroyed) return;
    if (behavior.kind === "sse") {
      res.writeHead(behavior.status ?? 200, { "content-type": "text/event-stream" });
      for (const chunk of behavior.sseChunks ?? []) {
        if (res.writableEnded || res.destroyed) return;
        res.write(chunk);
        if (behavior.sseChunkDelayMs !== undefined && behavior.sseChunkDelayMs > 0) await sleep(behavior.sseChunkDelayMs);
      }
      res.end();
      return;
    }
    res.writeHead(behavior.status ?? 200, { "content-type": "application/json" });
    res.end(JSON.stringify(behavior.body ?? defaultChatBody()));
  }

  function handleModels(res: ServerResponse): void {
    modelsCount += 1;
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ object: "list", data: models }));
  }

  const server: Server = createServer((req: IncomingMessage, res: ServerResponse) => {
    // 两处防炸：请求/响应流本身出错（客户端断开、连接重置……）不该产生
    // 无人处理的 'error' 事件——Node 对没有监听者的流 'error' 事件会直接抛出，
    // 抛到这个假上游所在的 vitest worker 进程里就是一次硬崩溃。
    req.on("error", () => undefined);
    res.on("error", () => undefined);
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const bodyText = Buffer.concat(chunks).toString("utf8");
      received.push({ method: req.method ?? "", path: req.url ?? "", bodyText });
      if (req.method === "GET" && req.url === "/v1/models") {
        handleModels(res);
        return;
      }
      if (req.method === "POST" && req.url === "/v1/chat/completions") {
        // handleChat 是 async 函数：里面任何一步同步抛出的异常都会变成被拒绝的
        // Promise。`void` 调用不会有人 catch 它——那是一次未处理的 Promise
        // rejection，同样会撂倒整个 vitest worker（同一个 worker 上其它测试
        // 文件也会被牵连）。这里兜底吞掉并打日志，绝不能让假上游的一次异常
        // 波及测试运行本身。
        handleChat(req, res).catch((err: unknown) => {
          console.error("[fake-upstream] handleChat 异常（已吞掉，不影响测试进程）：", err);
        });
        return;
      }
      res.writeHead(404, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "not_found" }));
    });
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const addr = server.address();
  const port = typeof addr === "object" && addr !== null ? addr.port : 0;

  return {
    url: "http://127.0.0.1:" + String(port),
    port,
    chatCount: () => chatCount,
    modelsCount: () => modelsCount,
    wasChatAborted: () => aborted,
    queueChat: (behavior) => {
      queue.push(behavior);
    },
    setModels: (m) => {
      models = m;
    },
    requests: () => received,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}
