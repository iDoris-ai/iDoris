import { describe, expect, it, vi } from "vitest";
import { OmlxBackend } from "../omlx/omlx-backend.js";

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

  it("load(resident) pins and pins=on_demand unpins", async () => {
    const calls: Array<[string, unknown]> = [];
    const f = vi.fn(async (url: string, init?: { body?: string }) => {
      calls.push([url, init?.body]);
      return jsonRes({});
    });
    const b = new OmlxBackend({ fetchImpl: f as never });
    await b.load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" });
    await b.load("VL-7B", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" });
    expect(calls[0]?.[0]).toBe("http://127.0.0.1:8088/v1/models/Qwen3-8B/load");
    expect(String(calls[1]?.[1])).toContain('"is_pinned":true');
    expect(String(calls[3]?.[1])).toContain('"is_pinned":false');
  });

  it("maps /api/status", async () => {
    const f = vi.fn(async () => jsonRes({ model_memory_max: 16, model_memory_used: 13.25, loaded: ["Qwen3-8B"], pressure: "soft" }));
    const b = new OmlxBackend({ fetchImpl: f as never });
    expect(await b.status()).toMatchObject({ pressure: "soft", usedGb: 13.25, modelMemoryMaxGb: 16, loaded: ["Qwen3-8B"] });
  });

  it("admission=coexist when loaded, else requires_eviction", async () => {
    const f = vi.fn(async () => jsonRes({ loaded: ["A"], model_memory_max: 16 }));
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
