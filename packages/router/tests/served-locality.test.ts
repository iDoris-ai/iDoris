import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MockBackend, SubscriptionRelayError, type ModelBackend } from "@idoris/adapters";
import { validateComponentCard, type ComponentCard } from "@idoris/contracts";
import { parse } from "yaml";
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

/** M3：上游真的响应了但状态码非 2xx——依然是"某后端服务了这次请求"，得带 Served-Locality。 */
function failingUpstreamProxy(): ChatProxy {
  return new ChatProxy({
    fetchImpl: async () => ({
      ok: false,
      status: 500,
      text: async () => JSON.stringify({ error: { type: "upstream_error" } }),
      body: null,
    }),
    retryDelaysMs: [], // 测试不等重试退避
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

// C1：加载真实的 config/components/subscription.yaml——它的 provider.locality
// 写的是 loopback，但那个字段在这张卡上表达的是"调用来源必须是 loopback"
// （见文件里的注释），不是"推理发生在本机"；真实推理由 form: spawn_cli 背后的
// 云端 CLI 完成。servedLocalityOf 必须认出这一点，不能被卡面上的 locality 字段骗到。
const subscriptionCardFromRealConfig = validateComponentCard(
  parse(readFileSync(join(repoRoot, "config", "components", "subscription.yaml"), "utf8")),
);

function subscriptionRegistered(chat: ModelBackend["chat"]): Registered[] {
  const backend: ModelBackend = {
    list: async () => [],
    load: async () => undefined,
    unload: async () => undefined,
    admission: async () => "coexist",
    status: async () => ({ pressure: "ok", usedGb: 0, modelMemoryMaxGb: 0, loaded: [] }),
    chat,
  };
  return [{ card: subscriptionCardFromRealConfig, backend }];
}

// 订阅卡 tier=remote，要走 routing-policy.yaml 的 { complexity: complex } 规则
// （tiers: [local, remote]）才会进候选集；privacy: any 是因为 local_only 请求
// 允许的 tier 集合根本不含 remote（policy.ts）。
const subscriptionChatHeaders = { "content-type": "application/json", "x-idoris-privacy": "any", "x-idoris-complexity": "complex" };
const subscriptionChatBody = JSON.stringify({ model: "subscription", messages: [{ role: "user", content: "hi" }] });

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

  it("M3：上游返回非 2xx 时状态码透传，但依然带 Served-Locality（已经落到具体后端了）", async () => {
    running = await startRouter({ componentsDir: fixtures, routingPolicyPath, proxy: failingUpstreamProxy() });
    const res = await fetch(url(running, "/v1/chat/completions"), { method: "POST", headers: chatHeaders, body: chatBody });
    expect(res.status).toBe(500);
    expect(res.headers.get("x-idoris-served-locality")).toBe("loopback");
  });

  it("C1 回归：真实 subscription.yaml（provider.locality: loopback）推理成功也回报 remote，不是 loopback", async () => {
    running = await startRouter({
      componentsDir: fixtures,
      routingPolicyPath,
      registered: subscriptionRegistered(async (req) => ({ model: req.model, content: "ok" })),
      env: { IDORIS_DEPLOY_MODE: "personal" },
    });
    const res = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: subscriptionChatHeaders,
      body: subscriptionChatBody,
    });
    expect(res.status).toBe(200);
    expect(res.headers.get("x-idoris-served-locality")).toBe("remote");
  });

  it("M3：订阅中转失败（502）也带 Record-Id，且 Served-Locality 仍是 remote", async () => {
    running = await startRouter({
      componentsDir: fixtures,
      routingPolicyPath,
      registered: subscriptionRegistered(async () => {
        throw new Error("cli crashed");
      }),
      env: { IDORIS_DEPLOY_MODE: "personal" },
    });
    const res = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: subscriptionChatHeaders,
      body: subscriptionChatBody,
    });
    expect(res.status).toBe(502);
    expect(res.headers.get("x-idoris-served-locality")).toBe("remote");
    expect(res.headers.get("x-idoris-record-id")).toBeTruthy();
  });

  /**
   * M2（PR #46 复审）：订阅 CLI 的原始诊断内容（stderr 摘要，可能带路径/栈）
   * 绝不能透传给调用方——只允许写服务端 stderr 日志。用一个哨兵字符串模拟
   * "CLI 报错里带了敏感细节"，断言它只出现在服务端日志里，不出现在 HTTP 响应里。
   */
  it("M2/H2：订阅 CLI 的原始 stderr 既不出现在 HTTP 响应里，也不出现在任何 console 输出里", async () => {
    const sentinel = "SENTINEL_STDERR_LEAK_CHECK_98765_at_/Users/attacker/secret/path";
    // 模拟 relay.ts 真实会做的事：CLI 的原始 stderr（含哨兵）只用来算白名单
    // 元数据（退出码/字节数/sha256 摘要前 12 位），哨兵本身从不被传到 router 层。
    const rawStderr = "boom " + sentinel;
    const stderrDigest = createHash("sha256").update(rawStderr, "utf8").digest("hex").slice(0, 12);
    const consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => undefined);
    try {
      running = await startRouter({
        componentsDir: fixtures,
        routingPolicyPath,
        registered: subscriptionRegistered(async () => {
          throw new SubscriptionRelayError("RELAY_CLI_FAILED", "subscription CLI exited with code 1", {
            exitCode: 1,
            stderrBytes: Buffer.byteLength(rawStderr, "utf8"),
            stderrDigest,
          });
        }),
        env: { IDORIS_DEPLOY_MODE: "personal" },
      });
      const res = await fetch(url(running, "/v1/chat/completions"), {
        method: "POST",
        headers: subscriptionChatHeaders,
        body: subscriptionChatBody,
      });
      expect(res.status).toBe(502);
      const text = await res.text();
      expect(text).not.toContain(sentinel);
      expect(text).not.toContain("attacker");
      const payload = JSON.parse(text) as { error: { type: string; reason_code: string; message: string } };
      expect(payload.error.type).toBe("subscription_relay_failed");
      expect(payload.error.reason_code).toBe("RELAY_CLI_FAILED");
      expect(payload.error.message).toBe("subscription relay failed"); // 固定文案，不是 CLI 原始输出

      // H2 核心断言：哨兵字符串既不在 HTTP 响应里，也不在任何 console 输出里——
      // 不再有"脱敏后的自由文本"这个中间状态可以泄露内容。
      const allConsoleOutput = consoleErrorSpy.mock.calls
        .flat()
        .map((arg) => (typeof arg === "string" ? arg : JSON.stringify(arg)))
        .join("\n");
      expect(allConsoleOutput).not.toContain(sentinel);
      expect(allConsoleOutput).not.toContain("attacker");
      // 正对照：白名单摘要确实被记下来了——不是"干脆什么都不记"，诊断能力还在。
      expect(allConsoleOutput).toContain(stderrDigest);
    } finally {
      consoleErrorSpy.mockRestore();
    }
  });
});
