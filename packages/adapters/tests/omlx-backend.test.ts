import { describe, expect, it, vi } from "vitest";
import { OmlxBackend, OmlxPinUnavailableError } from "../omlx/omlx-backend.js";

const jsonRes = (body: unknown, status = 200) => ({
  ok: status < 400,
  status,
  json: async () => body,
  text: async () => JSON.stringify(body),
});

describe("OmlxBackend", () => {
  it("lists models from /v1/models", async () => {
    const f = vi.fn(async (_url: string) => jsonRes({ data: [{ id: "Qwen3-8B" }, { id: "VL-7B" }] }));
    const b = new OmlxBackend({ fetchImpl: f as never });
    expect((await b.list()).map((m) => m.id)).toEqual(["Qwen3-8B", "VL-7B"]);
    expect(f.mock.calls[0]?.[0]).toBe("http://127.0.0.1:8088/v1/models");
  });

  describe("load(resident) pin via PUT /admin/api/models/{id}/settings (0.6.4, body only per openapi schema, untested)", () => {
    it("PUTs the flat {is_pinned:true} body to the new endpoint", async () => {
      const calls: Array<[string, string | undefined, unknown]> = [];
      const f = vi.fn(async (url: string, init?: { method?: string; body?: string }) => {
        calls.push([url, init?.method, init?.body]);
        return jsonRes({});
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      await b.load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" });
      expect(calls[0]?.[0]).toBe("http://127.0.0.1:8088/v1/models/Qwen3-8B/load");
      expect(calls[1]?.[0]).toBe("http://127.0.0.1:8088/admin/api/models/Qwen3-8B/settings");
      expect(calls[1]?.[1]).toBe("PUT"); // L2: 显式断言 method=PUT，不是 POST
      expect(String(calls[1]?.[2])).toBe('{"is_pinned":true}');
    });

    it("H2 negative control: /load succeeds (200) but /settings is 401 (admin auth required) -> load() rejects with OmlxPinUnavailableError, not silently swallowed", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" }, 200);
        if (url.includes("/admin/api/models/")) {
          return jsonRes({ detail: "Admin authentication required" }, 401);
        }
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      const promise = b.load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" });
      await expect(promise).rejects.toBeInstanceOf(OmlxPinUnavailableError);
      await expect(promise.catch((e) => e)).resolves.toMatchObject({
        modelId: "Qwen3-8B",
        message: expect.stringContaining("已加载"),
      });
      // 一份"把 401 吞掉"的改法（例如 setPinned 内部 catch 后什么都不做）会让上面两个 await 都失败，
      // 因为 promise 会 resolve 而不是 reject —— 这正是这条负对照要抓的回归。
    });
  });

  it("load(on_demand) does NOT call the pin/settings endpoint at all (H1: unpinned is 0.6.4's default state for a freshly-loaded model)", async () => {
    const calls: string[] = [];
    const f = vi.fn(async (url: string) => {
      calls.push(url);
      return jsonRes({});
    });
    const b = new OmlxBackend({ fetchImpl: f as never });
    await b.load("VL-7B", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" });
    expect(calls).toEqual(["http://127.0.0.1:8088/v1/models/VL-7B/load"]);
  });

  it("load(no policy) does NOT call the pin/settings endpoint either", async () => {
    const calls: string[] = [];
    const f = vi.fn(async (url: string) => {
      calls.push(url);
      return jsonRes({});
    });
    const b = new OmlxBackend({ fetchImpl: f as never });
    await b.load("VL-7B");
    expect(calls).toEqual(["http://127.0.0.1:8088/v1/models/VL-7B/load"]);
  });

  describe("maps /api/status", () => {
    it("0.6.4 field names + M1 byte->GiB conversion + valid pressure passthrough", async () => {
      const f = vi.fn(async () =>
        jsonRes({
          model_memory_max: 16 * 1024 ** 3, // 16 GiB in bytes
          model_memory_used: 8 * 1024 ** 3, // 8 GiB in bytes
          loaded_models: ["Qwen3-8B"],
          pressure: "soft",
        }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      expect(await b.status()).toMatchObject({
        pressure: "soft",
        usedGb: 8,
        modelMemoryMaxGb: 16,
        loaded: ["Qwen3-8B"],
      });
    });

    it("H3: pressure missing -> explicit 'unknown', not fail-open 'ok'", async () => {
      const f = vi.fn(async () => jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: [] }));
      const b = new OmlxBackend({ fetchImpl: f as never });
      expect((await b.status()).pressure).toBe("unknown");
    });

    it("H3: pressure not in the ok/soft/hard/ceiling whitelist -> throws instead of `as Pressure`", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: [], pressure: "critical" }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      await expect(b.status()).rejects.toThrow(/不认识的 pressure/);
    });

    it("M2: both loaded_models and loaded present -> loaded_models wins", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: ["A"], loaded: ["B"] }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      expect((await b.status()).loaded).toEqual(["A"]);
    });

    it("M2: loaded_models is null -> falls back to loaded", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: null, loaded: ["B"] }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      expect((await b.status()).loaded).toEqual(["B"]);
    });

    it("M2: loaded_models is a non-array value (e.g. \"x\") -> throws, does NOT fall back to loaded", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: "x", loaded: ["B"] }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      await expect(b.status()).rejects.toThrow(/不是数组/);
    });

    it("M2: both loaded_models and loaded missing -> throws, does NOT silently return []", async () => {
      const f = vi.fn(async () => jsonRes({ model_memory_max: 0, model_memory_used: 0 }));
      const b = new OmlxBackend({ fetchImpl: f as never });
      await expect(b.status()).rejects.toThrow(/缺少 loaded_models 与 loaded/);
    });
  });

  it("admission=coexist when loaded, else requires_eviction", async () => {
    const f = vi.fn(async () => jsonRes({ loaded_models: ["A"], model_memory_max: 0, model_memory_used: 0 }));
    const b = new OmlxBackend({ fetchImpl: f as never });
    expect(await b.admission("A")).toBe("coexist");
    expect(await b.admission("B")).toBe("requires_eviction");
  });

  it("chat maps OpenAI-compat response", async () => {
    const f = vi.fn(async () => jsonRes({ choices: [{ message: { content: "hello" } }] }));
    const b = new OmlxBackend({ fetchImpl: f as never });
    expect((await b.chat({ model: "A", messages: [{ role: "user", content: "hi" }] })).content).toBe("hello");
  });

  it("throws on non-2xx", async () => {
    const f = vi.fn(async () => jsonRes({}, 500));
    const b = new OmlxBackend({ fetchImpl: f as never });
    await expect(b.list()).rejects.toThrow(/HTTP 500/);
  });
});
