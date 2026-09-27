import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { ChatProxy, type FetchLike } from "../src/proxy.js";
import { startRouter, type Router } from "../src/server.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "good");
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const routingPolicyPath = join(repoRoot, "config", "routing-policy.yaml");

let running: Router | undefined;

afterEach(async () => {
  if (running) {
    await running.close();
    running = undefined;
  }
});

const url = (r: Router, path: string): string => "http://127.0.0.1:" + r.port + path;
const chatHeaders = { "content-type": "application/json" };
const chatBody = JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }] });

/**
 * 模拟"上游挂起不回"的假 fetch：只有在收到 abort 时才 settle（reject），
 * 其余情况永不 resolve/reject——用真实 HTTP 层面验证"客户端断开是否真的
 * 传播成了对上游请求的 abort"，而不是只测 ChatProxy 单元内部的 signal 透传。
 */
function hangingFetchImpl(onAbort: () => void): FetchLike {
  return (_url, init) =>
    new Promise((_resolve, reject) => {
      init.signal?.addEventListener("abort", () => {
        onAbort();
        reject(new Error("aborted by test fetchImpl"));
      });
    });
}

/**
 * §3.11 回归测试：R0 conformance 套件
 * （conformance/tests/upstream-behavior.test.ts）用变异测试发现，这里原来
 * 监听的是 req（客户端 → 路由器方向）的 'close'，而请求体读完之后 req 会自己
 * 很快触发一次 'close'，跟客户端有没有真的断开连接无关——净效果是真实客户端
 * 断开时上游请求几乎不会被 abort，接口规范 §3.11 说"已实现"的取消传播实际
 * 没有兑现。已改成监听 res（响应对象/底层 socket）的 'close'，并判断
 * `!res.writableEnded`（避免把"响应已经正常写完"误判成"被取消"）。
 */
describe("客户端断开 => 向上游传播取消（§3.11）", () => {
  it("客户端中途断开连接后，转发给上游的请求会被 abort", async () => {
    let upstreamAborted = false;
    const proxy = new ChatProxy({
      fetchImpl: hangingFetchImpl(() => {
        upstreamAborted = true;
      }),
    });
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy });

    const controller = new AbortController();
    const pending = fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: chatHeaders,
      body: chatBody,
      signal: controller.signal,
    });

    // 给路由器一点时间把请求真的转发出去（进入 proxy.forward() 的 fetchImpl 调用），
    // 再断开——否则可能在路由器还没来得及转发之前就把客户端这一侧取消了，那样
    // 就算取消传播的链路是坏的，也从没机会证明它坏在哪。
    await new Promise((resolve) => setTimeout(resolve, 100));
    controller.abort();
    await expect(pending).rejects.toBeDefined();

    const deadline = Date.now() + 5_000;
    while (!upstreamAborted && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    expect(upstreamAborted).toBe(true);
  });

  // 负对照：不主动断开时不该被误判成"已取消"——这条锁的正是原来那个 bug
  // 会制造的假阳性方向的反面：确保修复没有变成"不管断没断开都报 true"。
  it("负对照：客户端不断开时，上游请求不会被 abort", async () => {
    let upstreamAborted = false;
    const proxy = new ChatProxy({
      fetchImpl: hangingFetchImpl(() => {
        upstreamAborted = true;
      }),
    });
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy });

    const controller = new AbortController();
    // 这个请求永远不会自然 resolve/reject（上游故意挂起，我们也不主动 abort）；
    // 断言完之后自己 abort 掉用来清理，不能让它挂到测试进程退出。
    const pending = fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: chatHeaders,
      body: chatBody,
      signal: controller.signal,
    }).catch(() => undefined);

    await new Promise((resolve) => setTimeout(resolve, 500));
    expect(upstreamAborted).toBe(false);

    controller.abort();
    await pending;
  });
});
