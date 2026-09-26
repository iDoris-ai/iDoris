import { describe, expect, it, vi } from "vitest";
import { ChatProxy, type FetchResponseLike } from "../src/proxy.js";

const ok = (text: string): FetchResponseLike => ({ status: 200, ok: true, text: async () => text, body: null });
const err = (status: number): FetchResponseLike => ({ status, ok: false, text: async () => "boom", body: null });

describe("ChatProxy (T1.3.4)", () => {
  it("retries a 5xx up to 2 times, then succeeds", async () => {
    const f = vi.fn<(url: string) => Promise<FetchResponseLike>>();
    f.mockResolvedValueOnce(err(503)).mockResolvedValueOnce(err(503)).mockResolvedValueOnce(ok("done"));
    const proxy = new ChatProxy({ fetchImpl: f as never, sleep: async () => {} , now: () => 0 });
    const r = await proxy.forward("http://up", undefined, { model: "m" }, { stream: false });
    expect(r.status).toBe(200);
    expect(r.retries).toBe(2);
    expect(f).toHaveBeenCalledTimes(3);
  });

  it("does NOT retry once a stream has started", async () => {
    const body = { getReader: () => ({ read: async () => ({ done: true }), cancel: async () => {} }) };
    const f = vi.fn(async () => ({ status: 200, ok: true, text: async () => "", body }));
    const proxy = new ChatProxy({ fetchImpl: f as never });
    const r = await proxy.forward("http://up", undefined, { model: "m" }, { stream: true });
    expect(r.stream).not.toBeNull();
    expect(f).toHaveBeenCalledTimes(1);
  });

  it("caches an idempotent non-stream result by request id", async () => {
    const f = vi.fn(async () => ok("cached-body"));
    const proxy = new ChatProxy({ fetchImpl: f as never, now: () => 1000 });
    const a = await proxy.forward("http://up", undefined, { model: "m" }, { stream: false, requestId: "r1" });
    const b = await proxy.forward("http://up", undefined, { model: "m" }, { stream: false, requestId: "r1" });
    expect(a.cached).toBe(false);
    expect(b.cached).toBe(true);
    expect(f).toHaveBeenCalledTimes(1);
  });

  it("reports client_closed when the caller aborts", async () => {
    const controller = new AbortController();
    controller.abort();
    const f = vi.fn(async () => {
      throw new Error("aborted");
    });
    const proxy = new ChatProxy({ fetchImpl: f as never, sleep: async () => {}, now: () => 0 });
    const r = await proxy.forward("http://up", undefined, { model: "m" }, { stream: false, signal: controller.signal });
    expect(r.status).toBe(499);
    expect(f).toHaveBeenCalledTimes(1);
  });
});

describe("幂等缓存的租户隔离（回归：评审 PR #25 实测到的跨租户泄漏）", () => {
  /**
   * 这一组断言的是**内容不串味**，不是「键里含 tenant」。
   * 后者是实现细节，改个实现就失效；前者是产品承诺。
   */
  const mkProxy = (bodies: string[]): ChatProxy => {
    let i = 0;
    return new ChatProxy({
      fetchImpl: async () => ({
        ok: true,
        status: 200,
        text: async () => bodies[i++] ?? "EXHAUSTED",
        body: null,
      }),
      now: () => 1_000,
    });
  };

  it("同一个 requestId、不同租户 → 各自拿到自己的响应，绝不互串", async () => {
    const proxy = mkProxy(["TENANT_A_SECRET", "TENANT_B_SECRET"]);
    const a = await proxy.forward("http://up", undefined, {}, {
      stream: false, requestId: "shared-id", tenantId: "tenant-a",
    });
    const b = await proxy.forward("http://up", undefined, {}, {
      stream: false, requestId: "shared-id", tenantId: "tenant-b",
    });
    expect(a.text).toBe("TENANT_A_SECRET");
    expect(a.cached).toBe(false);
    // 泄漏时这里会是 TENANT_A_SECRET 且 cached=true —— 这正是评审实测到的形态。
    expect(b.text).toBe("TENANT_B_SECRET");
    expect(b.cached).toBe(false);
    expect(b.text).not.toBe("TENANT_A_SECRET");
  });

  it("同一个租户 + 同一个 requestId → 仍然命中缓存（幂等语义不被修坏）", async () => {
    const proxy = mkProxy(["ONCE", "SHOULD_NOT_BE_FETCHED"]);
    const first = await proxy.forward("http://up", undefined, {}, {
      stream: false, requestId: "same-id", tenantId: "tenant-a",
    });
    const second = await proxy.forward("http://up", undefined, {}, {
      stream: false, requestId: "same-id", tenantId: "tenant-a",
    });
    expect(first.cached).toBe(false);
    expect(second.cached).toBe(true);
    expect(second.text).toBe("ONCE");
  });

  it("同一个租户、同一个 requestId，但路由到不同 provider → 不返回另一个 provider 的响应", async () => {
    const proxy = mkProxy(["FROM_UPSTREAM_1", "FROM_UPSTREAM_2"]);
    const one = await proxy.forward("http://up1", undefined, {}, {
      stream: false, requestId: "same-id", tenantId: "tenant-a",
    });
    const two = await proxy.forward("http://up2", undefined, {}, {
      stream: false, requestId: "same-id", tenantId: "tenant-a",
    });
    expect(one.text).toBe("FROM_UPSTREAM_1");
    expect(two.text).toBe("FROM_UPSTREAM_2");
    expect(two.cached).toBe(false);
  });

  it("personal 模式（无 tenantId）不与任何具名租户共享键空间", async () => {
    const proxy = mkProxy(["PERSONAL", "NAMED_TENANT"]);
    const p = await proxy.forward("http://up", undefined, {}, {
      stream: false, requestId: "same-id",
    });
    const t = await proxy.forward("http://up", undefined, {}, {
      stream: false, requestId: "same-id", tenantId: "tenant-a",
    });
    expect(p.text).toBe("PERSONAL");
    expect(t.text).toBe("NAMED_TENANT");
    expect(t.cached).toBe(false);
  });
});
