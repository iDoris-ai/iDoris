import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { spawnConformanceServer, routingPolicyFixturePath, type RunningServer } from "../src/harness.js";
import { localComponent, makeComponentsDir } from "../src/fixtures.js";

let upstream: FakeUpstream;
let server: RunningServer;

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
  server = await spawnConformanceServer({ componentsDir, routingPolicyPath: routingPolicyFixturePath });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

async function readAll(res: Response): Promise<string> {
  const reader = res.body?.getReader();
  if (reader === undefined) return "";
  const decoder = new TextDecoder();
  let out = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    if (value !== undefined) out += decoder.decode(value, { stream: true });
  }
  return out;
}

describe("流式 SSE 透传", () => {
  it("上游流式分片原样透传给客户端", async () => {
    const before = upstream.chatCount();
    upstream.queueChat({
      kind: "sse",
      sseChunks: [
        'data: {"choices":[{"delta":{"content":"hel"}}]}\n\n',
        'data: {"choices":[{"delta":{"content":"lo"}}]}\n\n',
        "data: [DONE]\n\n",
      ],
      sseChunkDelayMs: 20,
    });
    const res = await fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "idoris/daily", stream: true, messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/event-stream");
    const text = await readAll(res);
    expect(text).toContain('"hel"');
    expect(text).toContain('"lo"');
    expect(text).toContain("[DONE]");
    expect(upstream.chatCount() - before).toBe(1);
  });

  it("负对照：流式请求若上游直接返回 5xx，不重试，客户端收到 JSON 错误而非 SSE", async () => {
    const before = upstream.chatCount();
    upstream.queueChat({ kind: "json", status: 502, body: { error: "upstream-down" } });
    const res = await fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "idoris/daily", stream: true, messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(502);
    expect(res.headers.get("content-type")).toContain("application/json");
    const body = (await res.json()) as { error: string };
    expect(body.error).toBe("upstream-down");
    expect(upstream.chatCount() - before).toBe(1); // 流式失败不重试
  });
});
