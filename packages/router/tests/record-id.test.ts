import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { startRouter, type Router } from "../src/server.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "good");

let running: Router | undefined;

afterEach(async () => {
  if (running) {
    await running.close();
    running = undefined;
  }
});

const url = (r: Router, path: string): string => "http://127.0.0.1:" + r.port + path;

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * T4.1：`X-iDoris-Record-Id`（接口规范 §3.7/§3.12）——服务端为每个请求生成，
 * 和调用方自己填的 `X-iDoris-Request-Id` 没有任何关系，调用方指定不了它。
 */
describe("X-iDoris-Record-Id", () => {
  it("成功响应（/health）带服务端生成的 Record-Id", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const res = await fetch(url(running, "/health"));
    expect(res.headers.get("x-idoris-record-id")).toMatch(UUID_RE);
  });

  it("错误响应（404）也带 Record-Id", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const res = await fetch(url(running, "/nope"));
    expect(res.status).toBe(404);
    expect(res.headers.get("x-idoris-record-id")).toMatch(UUID_RE);
  });

  it("调用方填的 X-iDoris-Request-Id 不会被当成/回显为 Record-Id", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const callerRequestId = "caller-picked-this-value";
    const res = await fetch(url(running, "/health"), {
      headers: { "x-idoris-request-id": callerRequestId },
    });
    const recordId = res.headers.get("x-idoris-record-id");
    expect(recordId).toMatch(UUID_RE);
    expect(recordId).not.toBe(callerRequestId);
  });

  it("M3：调用方直接填 X-iDoris-Record-Id 也不会被采纳——服务端照样自己生成一个新的", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const callerRecordId = "caller-picked-this-record-id";
    const res = await fetch(url(running, "/health"), {
      headers: { "x-idoris-record-id": callerRecordId },
    });
    const recordId = res.headers.get("x-idoris-record-id");
    expect(recordId).toMatch(UUID_RE);
    expect(recordId).not.toBe(callerRecordId);
  });

  it("负对照：两次请求的 Record-Id 不同（不是退化成固定值）", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const a = (await fetch(url(running, "/health"))).headers.get("x-idoris-record-id");
    const b = (await fetch(url(running, "/health"))).headers.get("x-idoris-record-id");
    expect(a).not.toBeNull();
    expect(a).not.toBe(b);
  });

  // M3：mutation coverage——400/503 这两条常见错误路径也必须带 Record-Id，
  // 不能因为「错误响应」就被判断逻辑漏掉。
  it("400 invalid_json 响应带 Record-Id", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const res = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{not-json",
    });
    expect(res.status).toBe(400);
    expect(res.headers.get("x-idoris-record-id")).toMatch(UUID_RE);
  });

  it("503 policy_unconfigured 响应带 Record-Id", async () => {
    running = await startRouter({ componentsDir: fixtures }); // 不传 routingPolicyPath
    const res = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(503);
    expect(res.headers.get("x-idoris-record-id")).toMatch(UUID_RE);
  });
});
