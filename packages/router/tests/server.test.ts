import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { HealthTracker } from "../src/health.js";
import { startRouter, type Router } from "../src/server.js";
import { ChatProxy } from "../src/proxy.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "good");
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
let running: Router | undefined;

afterEach(async () => {
  if (running) {
    await running.close();
    running = undefined;
  }
});

const url = (r: Router, path: string): string => "http://127.0.0.1:" + r.port + path;

describe("startRouter", () => {
  it("binds loopback only and serves /v1/models with data", async () => {
    running = await startRouter({ componentsDir: fixtures });
    expect(running.host).toBe("127.0.0.1");
    const res = await fetch(url(running, "/v1/models"));
    expect(res.status).toBe(200);
    const body = (await res.json()) as { data: unknown[] };
    expect(Array.isArray(body.data)).toBe(true);
    expect(body.data.length).toBeGreaterThan(0);
  });
  it("serves /health", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const body = (await (await fetch(url(running, "/health"))).json()) as { status: string };
    expect(body.status).toBe("ok");
  });
  it("records health for a backend", async () => {
    const health = new HealthTracker();
    running = await startRouter({ componentsDir: fixtures, health });
    await fetch(url(running, "/v1/models"));
    expect(typeof health.snapshot().mock).toBe("object");
  });
});

/**
 * server 层的租户接线（回归：合并 #32 时差点把 #25 的泄漏放回去，**而没有任何测试会红**）。
 *
 * `proxy.test.ts` 那四条跨租户测试测的是 `ChatProxy` 单元 —— 它们**显式把 tenantId
 * 传进去**，所以「server 有没有真的把 tenantId 传给 proxy」它们一个字都没测。
 * 实测过：把 `server.ts` 那行 `tenantId = resolved.tenantId` 删掉，全套 119 条照样全绿。
 *
 * 这条守的就是那段接线本身，端到端走真实 HTTP。它同时守住第二处：
 * `resolveProfile` 必须收到由**注入的 env** 算出的 deployMode —— 不传就会回落到
 * `process.env`，tenant 模式静默降级成 personal，`X-iDoris-Tenant` 被忽略、缓存又串。
 */
describe("server 把 tenantId 接到幂等缓存上（跨租户不串味）", () => {
  it("同一个 X-iDoris-Request-Id、两个租户 → 各自拿到自己的响应", async () => {
    let served = 0;
    const proxy = new ChatProxy({
      fetchImpl: async () => {
        served += 1;
        return {
          ok: true,
          status: 200,
          text: async () => JSON.stringify({ secret: "for-caller-" + String(served) }),
          body: null,
        };
      },
      now: () => 1_000,
    });
    running = await startRouter({
      componentsDir: fixtures,
      routingPolicyPath: join(repoRoot, "config", "routing-policy.yaml"),
      proxy,
      env: { IDORIS_DEPLOY_MODE: "tenant" } as NodeJS.ProcessEnv,
    });
    const active = running;

    const call = async (tenant: string): Promise<string> => {
      const res = await fetch("http://127.0.0.1:" + active.port + "/v1/chat/completions", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "x-idoris-request-id": "SHARED-ID",
          "x-idoris-tenant": tenant,
          "x-idoris-privacy": "local_only",
          "x-idoris-intent": "chat",
        },
        body: JSON.stringify({ model: "mock-small", messages: [{ role: "user", content: "hi" }] }),
      });
      return res.text();
    };

    const a = await call("tenant-a");
    const b = await call("tenant-b");

    // 泄漏时 b 会等于 a（命中 A 的缓存），且 served 停在 1。
    expect(a).not.toBe(b);
    expect(served).toBe(2);
  });
});
