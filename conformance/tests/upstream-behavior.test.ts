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
  /**
   * 已知 bug（本轮 conformance 复审发现，未修复，见 PR 描述与
   * tests/known-spec-conflicts.test.ts）：接口规范 §3.11 说取消"已实现"，
   * 但 packages/router/src/server.ts 里那行 `req.on("close", () => controller.abort())`
   * 监听的是**请求对象**（客户端 → 路由器）的 close，不是响应对象/socket 的 close。
   * 请求体在走到这行代码之前已经被 `readBody(req)` 完整读完，Node 的
   * IncomingMessage 在读完之后会自己很快触发一次 'close'——跟客户端到底有没有
   * 真的断开连接毫无关系。等这行代码执行、挂上监听器的时候，那次"自然 close"
   * 往往已经发生过了（用 `req.destroyed`/`req.complete` 实测确认过），
   * 监听器后挂上去不会补触发一次。净效果：`controller.abort()` 在真实客户端
   * 断开时基本不会被调用，取消不会传播到上游。
   *
   * **用变异测试验证过两层**（细节见 PR 描述）：
   * 1. 把 server.ts 那一行临时改成 `res.on("close", () => { if (!res.writableEnded)
   *    controller.abort(); })` 再重新 build：下面这条测试如果断言 `true`，会立刻变绿
   *    （约 300ms 内），证明取消传播只要用对监听对象就是好的，也证明本条测试本身
   *    有识别力，不是又一次假阳性。
   * 2. 在这个修好的基础上，再临时去掉 proxy.ts 里 `init.signal = opts.signal`
   *    那一行：测试重新变红——证明 proxy.ts 把 signal 转发给上游 fetch 这一步
   *    同样是必要环节，不是摆设。
   * 两次临时改动都已还原，本 PR 不改任何 packages/router 源码。
   *
   * 所以这里如实锁定**当前**（有 bug 的）行为：abort 之后 `wasAborted()` 保持
   * `false`。等 server.ts 那一行按上面验证过的方式修好后，这条测试要连同注释
   * 一起改回正向断言（`toBe(true)`）。
   */
  it("已知问题：客户端 abort 后取消目前不会传播到上游（wasAborted 保持 false）", async () => {
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

    // 修好之后（见上面变异测试）大约 300ms 内就会变 true；这里多等一点
    // （2s）确认它不是"还没来得及"，而是稳定停在 false。
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    expect(handle.wasAborted()).toBe(false);
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
