import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { ChatProxy } from "../src/proxy.js";
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
const chatHeaders = { "content-type": "application/json", "x-idoris-privacy": "any", "x-idoris-intent": "chat" };

/**
 * H2：单个请求的畸形输入或未预期异常绝不能带垮整个进程。
 *
 * 每条用例都在同一个 Router 实例上先发一个"坏"请求，再发一个正常请求，
 * 用第二个请求的成功来证明进程/server 没有因为第一个请求崩掉——这是这条
 * 验收标准的量纲：断言的不是"这次响应长什么样"，而是"服务还活着"。
 */
describe("H2：畸形请求体不能让进程崩溃", () => {
  for (const [label, rawBody] of [
    ["null", "null"],
    ["数组", "[]"],
    ["数字", "1"],
  ] as const) {
    it(`请求体是 ${label} → 400 invalid_body（带 Record-Id），路由器仍然存活`, async () => {
      running = await startRouter({ componentsDir: fixtures, routingPolicyPath });
      const res = await fetch(url(running, "/v1/chat/completions"), {
        method: "POST",
        headers: chatHeaders,
        body: rawBody,
      });
      expect(res.status).toBe(400);
      const payload = (await res.json()) as { error: { type: string } };
      expect(payload.error.type).toBe("invalid_body");
      expect(res.headers.get("x-idoris-record-id")).toBeTruthy();

      // 正对照：进程/server 没有被前一个请求带崩——后续正常请求（/health）照样成功。
      const health = await fetch(url(running, "/health"));
      expect(health.status).toBe(200);
    });
  }

  it("负对照：合法的 JSON 对象体不会被 invalid_body 误伤（只挡 null/数组/数字）", async () => {
    running = await startRouter({ componentsDir: fixtures }); // 无 routingPolicyPath，走到 policy_unconfigured 就够证明没被 invalid_body 拦下
    const res = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: chatHeaders,
      body: JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(503); // policy_unconfigured，不是 400 invalid_body
  });
});

/**
 * 真正未预期的异常（不是已知业务错误）：用一个会抛错的 proxy 模拟"没想到的 bug"，
 * 证明 handle() 顶层的 try/catch 能兜住它，回 500 且不回显栈，进程照样存活。
 */
describe("H2：未预期异常 → 500 internal_error，不回显栈，进程仍存活", () => {
  it("proxy.forward 抛出未预期异常 → 500，响应体不含内部错误信息/栈", async () => {
    class ThrowingProxy extends ChatProxy {
      override forward(): Promise<never> {
        return Promise.reject(new Error("boom-should-never-reach-caller"));
      }
    }
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy: new ThrowingProxy() });
    const res = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: chatHeaders,
      body: JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(500);
    expect(res.headers.get("x-idoris-record-id")).toBeTruthy();
    const text = await res.text();
    expect(text).not.toContain("boom-should-never-reach-caller");
    expect(text).not.toContain(" at "); // 不是裸 stack trace
    const payload = JSON.parse(text) as { error: { type: string } };
    expect(payload.error.type).toBe("internal_error");

    // 正对照：进程仍然活着，后续请求正常处理。
    const health = await fetch(url(running, "/health"));
    expect(health.status).toBe(200);
  });
});
