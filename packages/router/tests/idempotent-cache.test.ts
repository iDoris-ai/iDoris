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
const chatHeaders = {
  "content-type": "application/json",
  "x-idoris-privacy": "any",
  "x-idoris-intent": "chat",
  "x-idoris-request-id": "same-request-id-for-both-calls",
};
const chatBody = JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }] });

function fakeProxy(): ChatProxy {
  let served = 0;
  return new ChatProxy({
    fetchImpl: async () => {
      served += 1;
      return { ok: true, status: 200, text: async () => JSON.stringify({ n: served }), body: null };
    },
    now: () => 1_000, // 固定时钟：缓存窗口内，第二次必然命中
  });
}

/**
 * L1：幂等缓存命中时如实回报——`X-iDoris-Cached: true` + 指回第一次落地那条
 * 记录的 `X-iDoris-Origin-Record-Id`。Record-Id 本身永远是新生成的（每请求一个），
 * 所以「缓存命中」这件事必须靠单独的头来说明，不能让调用方误以为两次是独立的新请求。
 */
describe("X-iDoris-Cached / X-iDoris-Origin-Record-Id", () => {
  it("同一个 X-iDoris-Request-Id 第二次命中缓存：带 Cached=true 和指回第一条的 Origin-Record-Id", async () => {
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy: fakeProxy() });

    const first = await fetch(url(running, "/v1/chat/completions"), { method: "POST", headers: chatHeaders, body: chatBody });
    expect(first.status).toBe(200);
    const firstRecordId = first.headers.get("x-idoris-record-id");
    expect(firstRecordId).toBeTruthy();
    // 负对照：第一次是真实推理，不是缓存命中，不应该带这两个头。
    expect(first.headers.get("x-idoris-cached")).toBeNull();
    expect(first.headers.get("x-idoris-origin-record-id")).toBeNull();

    const second = await fetch(url(running, "/v1/chat/completions"), { method: "POST", headers: chatHeaders, body: chatBody });
    expect(second.status).toBe(200);
    const secondRecordId = second.headers.get("x-idoris-record-id");
    expect(second.headers.get("x-idoris-cached")).toBe("true");
    expect(second.headers.get("x-idoris-origin-record-id")).toBe(firstRecordId);
    // Record-Id 本身永远是这次请求新生成的，即便命中缓存也不会退化成复用第一条的。
    expect(secondRecordId).not.toBe(firstRecordId);
  });

  it("负对照：不同的 X-iDoris-Request-Id 不会互相命中缓存", async () => {
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy: fakeProxy() });
    const a = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: { ...chatHeaders, "x-idoris-request-id": "id-a" },
      body: chatBody,
    });
    const b = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: { ...chatHeaders, "x-idoris-request-id": "id-b" },
      body: chatBody,
    });
    expect(a.status).toBe(200);
    expect(b.status).toBe(200);
    expect(b.headers.get("x-idoris-cached")).toBeNull();
  });
});
