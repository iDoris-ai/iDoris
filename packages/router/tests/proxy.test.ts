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
