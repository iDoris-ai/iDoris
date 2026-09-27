import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { MockBackend } from "@idoris/adapters";
import type { ComponentCard } from "@idoris/contracts";
import { ChatProxy, type FetchResponseLike } from "../src/proxy.js";
import type { Registered } from "../src/registry.js";
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

// privacy: any → dispatch 不按 locality 过滤候选（只有 local_only 才过滤），
// 这样即便测试卡的 locality 是非法值，请求依然能走到「已选中某个后端」这一步，
// 从而验证响应头本身的 fail-closed 兜底，而不是被 dispatch 提前拦下。
const chatHeaders = { "content-type": "application/json", "x-idoris-privacy": "any", "x-idoris-intent": "chat" };
const chatBody = JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }] });

function nonStreamFakeProxy(): ChatProxy {
  return new ChatProxy({
    fetchImpl: async () => ({
      ok: true,
      status: 200,
      text: async () => JSON.stringify({ choices: [{ message: { content: "hello" } }] }),
      body: null,
    }),
  });
}

function streamFakeProxy(): ChatProxy {
  const chunk = new TextEncoder().encode('data: {"choices":[{"delta":{"content":"hi"}}]}\n\n');
  const streamBody: FetchResponseLike["body"] = {
    getReader() {
      let sent = false;
      return {
        read: async () => {
          if (sent) return { done: true };
          sent = true;
          return { done: false, value: chunk };
        },
        cancel: async () => undefined,
      };
    },
  };
  return new ChatProxy({
    fetchImpl: async () => ({ ok: true, status: 200, text: async () => "", body: streamBody }),
  });
}

/** fixtures/good 的组件卡，但把 locality 换成任意值（绕过 schema 校验，模拟运行期异常数据）。 */
function makeRegisteredWithLocality(locality: unknown): Registered[] {
  const card = {
    provider: {
      id: "mock",
      family: "local",
      tier: "local",
      capabilities: ["chat"],
      privacy_class: "local_only",
      cost: { input_per_m: 0, output_per_m: 0 },
      locality,
    },
    form: "http_service",
    endpoint: "mock://in-memory",
    version_pin: "mock@0.1.0",
    privacy_class: "local_only",
    allowed_egress: ["none"],
    fallback_policy: "fail_closed",
    fail_closed: true,
    load_policy: { mode: "resident", keepalive: { pinned: true }, admission: "coexist" },
  } as unknown as ComponentCard;
  return [{ card, backend: new MockBackend({ memoryMaxGb: 16, models: [{ id: "mock-small", memoryGb: 2 }] }) }];
}

/**
 * T4.1：`X-iDoris-Served-Locality`（接口规范 §3.5/§3.12）——只在真的有后端服务
 * 处理了这次推理时才出现，取值来自该后端组件卡的 provider.locality。
 */
describe("X-iDoris-Served-Locality", () => {
  it("成功的非流式响应带实际服务方的 locality（fixtures/good 的 mock 卡是 loopback）", async () => {
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy: nonStreamFakeProxy() });
    const res = await fetch(url(running, "/v1/chat/completions"), { method: "POST", headers: chatHeaders, body: chatBody });
    expect(res.status).toBe(200);
    expect(res.headers.get("x-idoris-served-locality")).toBe("loopback");
  });

  it("成功的流式响应也带 locality", async () => {
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy: streamFakeProxy() });
    const res = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: chatHeaders,
      body: JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }], stream: true }),
    });
    expect(res.headers.get("x-idoris-served-locality")).toBe("loopback");
    await res.text(); // 排空 SSE body，避免连接悬挂
  });

  it("负对照：locality 缺失时按 remote 回报，绝不默认 loopback（fail-closed）", async () => {
    running = await startRouter({
      componentsDir: fixtures,
      routingPolicyPath,
      registered: makeRegisteredWithLocality(undefined),
      proxy: nonStreamFakeProxy(),
    });
    const res = await fetch(url(running, "/v1/chat/completions"), { method: "POST", headers: chatHeaders, body: chatBody });
    expect(res.status).toBe(200);
    expect(res.headers.get("x-idoris-served-locality")).toBe("remote");
  });

  it("负对照：locality 是非三值之一时按 remote 回报", async () => {
    running = await startRouter({
      componentsDir: fixtures,
      routingPolicyPath,
      registered: makeRegisteredWithLocality("not-a-real-locality"),
      proxy: nonStreamFakeProxy(),
    });
    const res = await fetch(url(running, "/v1/chat/completions"), { method: "POST", headers: chatHeaders, body: chatBody });
    expect(res.headers.get("x-idoris-served-locality")).toBe("remote");
  });

  it("负对照：没有后端服务的错误响应（策略未配置）不带 Served-Locality", async () => {
    running = await startRouter({ componentsDir: fixtures }); // 不传 routingPolicyPath
    const res = await fetch(url(running, "/v1/chat/completions"), { method: "POST", headers: chatHeaders, body: chatBody });
    expect(res.status).toBe(503);
    expect(res.headers.get("x-idoris-served-locality")).toBeNull();
  });
});
