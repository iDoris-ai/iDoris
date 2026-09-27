import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { spawnConformanceServer, routingPolicyFixturePath, type RunningServer } from "../src/harness.js";
import { localComponent, makeComponentsDir } from "../src/fixtures.js";

let upstream: FakeUpstream;
let server: RunningServer;
let reqSeq = 0;
const nextRequestId = (): string => "ub-" + String(reqSeq++);

const postChat = (headers: Record<string, string> = {}) =>
  fetch(server.baseUrl + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
  });

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
  server = await spawnConformanceServer({ componentsDir, routingPolicyPath: routingPolicyFixturePath });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

describe("上游 5xx 重试与错误映射", () => {
  it("前两次 500，第三次成功 => 客户端拿到 200，上游总共被打 3 次", async () => {
    const before = upstream.chatCount();
    upstream.queueChat({ kind: "json", status: 500, body: { error: "boom-1" } });
    upstream.queueChat({ kind: "json", status: 500, body: { error: "boom-2" } });
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const res = await postChat();
    expect(res.status).toBe(200);
    expect(upstream.chatCount() - before).toBe(3);
  });

  it("持续 5xx（超过重试上限）=> 原样透传最后一次的响应体，仍只打 3 次（1 次 + 2 次重试）", async () => {
    const before = upstream.chatCount();
    for (let i = 0; i < 3; i += 1) upstream.queueChat({ kind: "json", status: 503, body: { error: "always-503" } });
    const res = await postChat();
    expect(res.status).toBe(503);
    const body = (await res.json()) as { error: string };
    expect(body.error).toBe("always-503");
    expect(upstream.chatCount() - before).toBe(3);
  });
});

describe("X-iDoris-Request-Id 幂等（60s 窗口）", () => {
  it("同一个 Request-Id 连续发两次 => 上游只打一次，两次响应体一致", async () => {
    const before = upstream.chatCount();
    upstream.queueChat({ kind: "json", status: 200, body: { marker: "idem-" + String(Date.now()) } });
    const id = nextRequestId();
    const a = await (await postChat({ "x-idoris-request-id": id })).text();
    const b = await (await postChat({ "x-idoris-request-id": id })).text();
    expect(a).toBe(b);
    expect(upstream.chatCount() - before).toBe(1);
  });

  it("正控：不同 Request-Id 各自触发一次上游调用", async () => {
    const before = upstream.chatCount();
    upstream.queueChat({ kind: "json", status: 200, body: { marker: "a" } });
    upstream.queueChat({ kind: "json", status: 200, body: { marker: "b" } });
    await postChat({ "x-idoris-request-id": nextRequestId() });
    await postChat({ "x-idoris-request-id": nextRequestId() });
    expect(upstream.chatCount() - before).toBe(2);
  });
});

describe("慢响应不会被提前掐断", () => {
  it("上游延迟 800ms 才回 200 => 客户端仍拿到 200，耗时 >= 750ms", async () => {
    upstream.queueChat({
      kind: "json",
      status: 200,
      delayMs: 800,
      body: { choices: [{ message: { content: "slow" } }] },
    });
    const start = Date.now();
    const res = await postChat();
    const elapsed = Date.now() - start;
    expect(res.status).toBe(200);
    expect(elapsed).toBeGreaterThanOrEqual(750);
  });
});

describe("客户端断开 => 向上游传播取消", () => {
  it("客户端主动 abort 后，上游连接最终被关闭", async () => {
    upstream.queueChat({ kind: "hang" });
    const controller = new AbortController();
    const pending = fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
      signal: controller.signal,
    });
    setTimeout(() => controller.abort(), 300);
    await expect(pending).rejects.toBeDefined();

    const deadline = Date.now() + 5000;
    while (!upstream.wasChatAborted() && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    expect(upstream.wasChatAborted()).toBe(true);
  });
});
