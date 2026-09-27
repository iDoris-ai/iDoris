import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { MockBackend } from "@idoris/adapters";
import { validateComponentCard } from "@idoris/contracts";
import { ChatProxy } from "../src/proxy.js";
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

/**
 * C1（PR #46 复审）端到端复现：两张组件卡共用同一个物理 endpoint 字符串——
 * 现实中会是"本地隧道转发到云端"这类配置：一张卡声明 tier=remote/locality=remote，
 * 另一张声明 tier=local/locality=loopback/privacy_class=local_only，两者 endpoint
 * 写的是同一个 127.0.0.1 地址。
 *
 * 修复前：只按 tenant+endpoint+requestId 做缓存键，第二个请求（local_only，选中
 * loopback 卡）会命中第一个请求（remote 卡）写下的缓存，把远程产生的响应内容当
 * 本地响应吐回去，Served-Locality 却按"这次选中的卡"算成 loopback——响应体是
 * 远程来的，头却说是 loopback。
 */
describe("C1 端到端：共享 endpoint 的两张卡不会互相污染彼此的缓存", () => {
  let policyDir: string;

  afterEach(() => {
    if (policyDir) rmSync(policyDir, { recursive: true, force: true });
  });

  it("local_only 请求选中 loopback 卡后，绝不会拿到另一张 remote 卡缓存下的响应", async () => {
    policyDir = mkdtempSync(join(tmpdir(), "idoris-c1-shared-endpoint-"));
    const sharedPolicyPath = join(policyDir, "policy.yaml");
    writeFileSync(
      sharedPolicyPath,
      [
        "routing_policy:",
        "  version: 1",
        "  rules:",
        "    - if: { complexity: complex }",
        "      then: { tiers: [remote] }",
        "  default: { tiers: [local], fail_closed: true }",
        "",
      ].join("\n"),
    );

    const sharedEndpoint = "http://127.0.0.1:9500";
    const remoteCard = validateComponentCard({
      provider: {
        id: "shared-endpoint-remote",
        family: "other",
        tier: "remote",
        capabilities: ["chat"],
        privacy_class: "any",
        cost: { input_per_m: 0, output_per_m: 0 },
        locality: "remote",
      },
      form: "http_service",
      endpoint: sharedEndpoint,
      version_pin: "remote@1",
      privacy_class: "any",
      allowed_egress: ["internet"],
      fallback_policy: "next_in_chain",
      fail_closed: false,
    });
    const loopbackCard = validateComponentCard({
      provider: {
        id: "shared-endpoint-loopback",
        family: "local",
        tier: "local",
        capabilities: ["chat"],
        privacy_class: "local_only",
        cost: { input_per_m: 0, output_per_m: 0 },
        locality: "loopback",
      },
      form: "http_service",
      endpoint: sharedEndpoint, // 跟上面那张远程卡物理上是同一个地址
      version_pin: "loopback@1",
      privacy_class: "local_only",
      allowed_egress: ["none"],
      fallback_policy: "fail_closed",
      fail_closed: true,
      load_policy: { mode: "resident", keepalive: { pinned: true }, admission: "coexist" },
    });
    const registered: Registered[] = [
      { card: remoteCard, backend: new MockBackend({ memoryMaxGb: 1, models: [] }) },
      { card: loopbackCard, backend: new MockBackend({ memoryMaxGb: 1, models: [] }) },
    ];

    let served = 0;
    const proxy = new ChatProxy({
      fetchImpl: async () => {
        served += 1;
        return { ok: true, status: 200, text: async () => JSON.stringify({ from: "call-" + String(served) }), body: null };
      },
      now: () => 1_000,
    });

    running = await startRouter({ componentsDir: fixtures, routingPolicyPath: sharedPolicyPath, registered, proxy });

    const sharedRequestId = "shared-request-id-across-both-providers";
    const remoteRes = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-idoris-privacy": "any",
        "x-idoris-complexity": "complex",
        "x-idoris-request-id": sharedRequestId,
      },
      body: chatBody,
    });
    expect(remoteRes.status).toBe(200);
    expect(remoteRes.headers.get("x-idoris-served-locality")).toBe("remote");
    const remoteBody = await remoteRes.text();

    const loopbackRes = await fetch(url(running, "/v1/chat/completions"), {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-idoris-privacy": "local_only",
        "x-idoris-request-id": sharedRequestId, // 故意复用同一个 requestId
      },
      body: chatBody,
    });
    expect(loopbackRes.status).toBe(200);
    // 核心断言：Served-Locality 是真的 loopback（这张卡真实是 loopback），
    // 而且响应体不是上一条 remote 请求缓存下来的内容——不是"远程内容被贴上
    // loopback 标签"，是两次都各自发生了真实的、各自的推理。
    expect(loopbackRes.headers.get("x-idoris-served-locality")).toBe("loopback");
    const loopbackBody = await loopbackRes.text();
    expect(loopbackBody).not.toBe(remoteBody);
    // 且不是缓存命中——是真的走了一遍新的 fetch。
    expect(loopbackRes.headers.get("x-idoris-cached")).toBeNull();
    expect(served).toBe(2);
  });
});
