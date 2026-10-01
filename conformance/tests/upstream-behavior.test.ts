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
  // PR #153 已修复响应连接的 close 监听；通过真实 HTTP 验证取消能到达上游。
  it("客户端 abort 后取消传播到上游", async () => {
    const handle = upstream.queueChat({ kind: "hang" });
    const before = upstream.chatCount();
    const controller = new AbortController();
    const pending = fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
      signal: controller.signal,
    });

    // 先确认请求真的已经转发到了假上游（chatCount 增长）再 abort——否则可能在
    // 路由器还没来得及把请求转发出去之前就把客户端这一侧取消了：那样即便
    // "取消传播"这条链路本身是坏的，假上游也压根没收到过这次请求，
    // 跟"取消确实传播到了但我们判断错了"长得一模一样，会掩盖真正的问题。
    const reachedDeadline = Date.now() + 5_000;
    while (upstream.chatCount() === before && Date.now() < reachedDeadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    expect(upstream.chatCount() - before).toBe(1);

    controller.abort();
    await expect(pending).rejects.toBeDefined();

    // 给连接关闭和上游取消传播留出时间；持续观察真实上游连接状态。
    const abortedDeadline = Date.now() + 2_000;
    while (!handle.wasAborted() && Date.now() < abortedDeadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    expect(handle.wasAborted()).toBe(true);
  });

  // 负对照：不主动断开时不该被误判成"已取消"——这条锁的正是原来那个 bug
  // （监听 req 而不是 res 的 'close'）会制造的假阳性：不管有没有真的 abort，
  // 旧实现都会报 true。
  it("负对照：hang 请求不主动断开时，wasAborted() 保持 false", async () => {
    const handle = upstream.queueChat({ kind: "hang" });
    const before = upstream.chatCount();
    const controller = new AbortController();
    // 这个 fetch 永远不会自然 resolve/reject（上游故意挂起，我们也不主动
    // abort）；断言完之后自己 abort 掉用来清理，不能让它挂到测试进程退出。
    const pending = fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
      signal: controller.signal,
    }).catch(() => undefined);

    const reachedDeadline = Date.now() + 5_000;
    while (upstream.chatCount() === before && Date.now() < reachedDeadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    expect(upstream.chatCount() - before).toBe(1);

    await new Promise((resolve) => setTimeout(resolve, 500));
    expect(handle.wasAborted()).toBe(false);

    controller.abort();
    await pending;
  });
});
