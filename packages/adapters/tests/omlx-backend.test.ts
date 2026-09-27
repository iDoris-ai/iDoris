import { describe, expect, it, vi } from "vitest";
import {
  OmlxBackend,
  OmlxPinStateUnverifiedError,
  OmlxPinUnavailableError,
  OmlxUnexpectedlyPinnedError,
  OmlxVerificationError,
} from "../omlx/omlx-backend.js";

const jsonRes = (body: unknown, status = 200) => ({
  ok: status < 400,
  status,
  json: async () => body,
  text: async () => JSON.stringify(body),
});

/** `/v1/models/status` 的最小合法响应：一条严格通过校验的条目（loaded+pinned 都是 boolean）。 */
const modelsStatusRes = (id: string, loaded: boolean, pinned: boolean) => jsonRes({ models: [{ id, loaded, pinned }] });

const SENTINEL = "SECRET_SENTINEL_DO_NOT_LEAK";

describe("OmlxBackend", () => {
  it("lists models from /v1/models", async () => {
    const f = vi.fn(async (_url: string) => jsonRes({ data: [{ id: "Qwen3-8B" }, { id: "VL-7B" }] }));
    const b = new OmlxBackend({ fetchImpl: f as never });
    expect((await b.list()).map((m) => m.id)).toEqual(["Qwen3-8B", "VL-7B"]);
    expect(f.mock.calls[0]?.[0]).toBe("http://127.0.0.1:8088/v1/models");
  });

  describe("load(resident) pin via PUT /admin/api/models/{id}/settings (0.6.4, body only per openapi schema, untested)", () => {
    it("PUTs the flat {is_pinned:true} body, then复核 GET /v1/models/status 确认 pinned===true 才算成功（M1）", async () => {
      const calls: Array<[string, string | undefined, unknown]> = [];
      const f = vi.fn(async (url: string, init?: { method?: string; body?: string }) => {
        calls.push([url, init?.method, init?.body]);
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", true, true);
        return jsonRes({});
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      await b.load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" });
      expect(calls.map((c) => c[0])).toEqual([
        "http://127.0.0.1:8088/v1/models/Qwen3-8B/load",
        "http://127.0.0.1:8088/admin/api/models/Qwen3-8B/settings",
        "http://127.0.0.1:8088/v1/models/status",
      ]);
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
        code: "OMLX_PIN_UNAVAILABLE",
        causeErrorName: "OmlxHttpError",
        causeHttpStatus: 401, // H1（本轮）：安全的结构化元数据，不是从 message 里解析出来的
        message: expect.stringContaining("常驻"),
      });
      // 一份"把 401 吞掉"的改法（例如 setPinned 内部 catch 后什么都不做）会让上面两个 await 都失败，
      // 因为 promise 会 resolve 而不是 reject —— 这正是这条负对照要抓的回归。
    });

    it("H1（本轮）哨兵测试：底层 fetch/网络层直接抛出的错误（可能携带后端敏感内容）不会通过 cause 泄露进 OmlxPinUnavailableError 的 message/String(err)/JSON.stringify(err)", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.includes("/admin/api/models/")) {
          // 模拟底层 fetch/网络库直接 throw（而不是返回一个 !ok 的 response），
          // 且这个错误的 message 里"意外"带了不该泄露的内容。
          throw new Error(`upstream connection reset, partial body leaked: ${SENTINEL}`);
        }
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      const err = await b
        .load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" })
        .catch((e: unknown) => e);
      expect(err).toBeInstanceOf(OmlxPinUnavailableError);
      expect((err as Error).message).not.toContain(SENTINEL);
      expect(String(err)).not.toContain(SENTINEL);
      expect(JSON.stringify(err)).not.toContain(SENTINEL);
      // 但安全的结构化元数据（错误类名）还是保留了，方便调用方诊断：
      expect((err as OmlxPinUnavailableError).causeErrorName).toBe("Error");
    });

    it("M1（本轮）负对照：PUT /settings 返回 200，但复核 GET /v1/models/status 后 pinned 仍是 false -> 以 OmlxPinUnavailableError reject（不能把任意 2xx 当成 pin 成功）", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.includes("/admin/api/models/")) return jsonRes({}); // PUT 200，但没真的生效
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", true, false); // 复核发现仍是 unpinned
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      const err = await b
        .load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" })
        .catch((e: unknown) => e);
      expect(err).toBeInstanceOf(OmlxPinUnavailableError); // 确认失败：复核读到明确的 pinned===false
      expect((err as OmlxPinUnavailableError).causeHttpStatus).toBeUndefined(); // 不是 HTTP 失败
    });

    it("M1（本轮）正对照：PUT 200 且复核后 pinned===true -> resolve", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.includes("/admin/api/models/")) return jsonRes({});
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", true, true);
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      await expect(
        b.load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" }),
      ).resolves.toBeUndefined();
    });

    it("M2：复核请求本身失败（例如复核时模型被并发卸载）是状态未知，不是确认失败 -> OmlxPinStateUnverifiedError（不是 OmlxPinUnavailableError）", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.includes("/admin/api/models/")) return jsonRes({});
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", false, false); // 并发卸载
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      const promise = b.load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" });
      await expect(promise).rejects.toBeInstanceOf(OmlxPinStateUnverifiedError);
      await expect(promise).rejects.not.toBeInstanceOf(OmlxPinUnavailableError);
      await expect(promise.catch((e) => e)).resolves.toMatchObject({
        modelId: "Qwen3-8B",
        code: "OMLX_PIN_STATE_UNVERIFIED",
        causeReason: "not_loaded", // 复用 OmlxVerificationError 的 reason，而不是猜测/断言"确认失败"
      });
    });

    it("M2：复核请求因响应畸形失败，同样是 OmlxPinStateUnverifiedError，causeReason 对应 OmlxVerificationError 的 reason", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.includes("/admin/api/models/")) return jsonRes({});
        if (url.endsWith("/v1/models/status")) return jsonRes({}); // models 字段缺失
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      const promise = b.load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" });
      await expect(promise).rejects.toBeInstanceOf(OmlxPinStateUnverifiedError);
      await expect(promise.catch((e) => e)).resolves.toMatchObject({ causeReason: "models_missing" });
    });
  });

  describe("load(non-resident) 加载完成后严格核对是否被外部 pin 住（H-a/H1）", () => {
    it("on_demand: 不调用 pin/settings 端点，但会读 GET /v1/models/status", async () => {
      const calls: string[] = [];
      const f = vi.fn(async (url: string) => {
        calls.push(url);
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("VL-7B", true, false);
        return jsonRes({});
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      await b.load("VL-7B", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" });
      expect(calls).toEqual([
        "http://127.0.0.1:8088/v1/models/VL-7B/load",
        "http://127.0.0.1:8088/v1/models/status",
      ]);
    });

    it("未传 policy 时同样会核对 /v1/models/status", async () => {
      const calls: string[] = [];
      const f = vi.fn(async (url: string) => {
        calls.push(url);
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("VL-7B", true, false);
        return jsonRes({});
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      await b.load("VL-7B");
      expect(calls).toEqual([
        "http://127.0.0.1:8088/v1/models/VL-7B/load",
        "http://127.0.0.1:8088/v1/models/status",
      ]);
    });

    it("H-a 正对照：/v1/models/status 报告 loaded:true, pinned:false 时 load(on_demand) 正常 resolve", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", true, false);
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      await expect(
        b.load("Qwen3-8B", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" }),
      ).resolves.toBeUndefined();
    });

    it("H-a 负对照：pinned:true 时 load(on_demand) 以 OmlxUnexpectedlyPinnedError reject（模型可能是被 oMLX 管理页手动 pin 过，或 pin 状态跨重启保留）", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", true, true);
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      const promise = b.load("Qwen3-8B", {
        mode: "on_demand",
        keepalive: { idle_ttl_s: 300 },
        admission: "requires_eviction",
      });
      await expect(promise).rejects.toBeInstanceOf(OmlxUnexpectedlyPinnedError);
      await expect(promise.catch((e) => e)).resolves.toMatchObject({
        modelId: "Qwen3-8B",
        code: "OMLX_UNEXPECTEDLY_PINNED",
        message: expect.stringContaining("pinned"),
      });
    });

    it("evict_to_load 模式也会核对 pinned 状态", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", true, true);
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      await expect(
        b.load("Qwen3-8B", { mode: "evict_to_load", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" }),
      ).rejects.toBeInstanceOf(OmlxUnexpectedlyPinnedError);
    });

    describe("H1 负对照：verifyModelState 严格校验，fail-closed 而不是 fail-open", () => {
      it("并发卸载（loaded:false）-> OmlxVerificationError(reason=not_loaded)，不当成'未被 pin'放行", async () => {
        const f = vi.fn(async (url: string) => {
          if (url.endsWith("/load")) return jsonRes({ status: "ok" });
          if (url.endsWith("/v1/models/status")) return modelsStatusRes("Qwen3-8B", false, false);
          throw new Error("unexpected url: " + url);
        });
        const b = new OmlxBackend({ fetchImpl: f as never });
        const promise = b.load("Qwen3-8B", {
          mode: "on_demand",
          keepalive: { idle_ttl_s: 300 },
          admission: "requires_eviction",
        });
        await expect(promise).rejects.toBeInstanceOf(OmlxVerificationError);
        await expect(promise.catch((e) => e)).resolves.toMatchObject({ reason: "not_loaded" });
      });

      it("模型不在 models 列表里 -> OmlxVerificationError(reason=model_not_found)", async () => {
        const f = vi.fn(async (url: string) => {
          if (url.endsWith("/load")) return jsonRes({ status: "ok" });
          if (url.endsWith("/v1/models/status")) return jsonRes({ models: [{ id: "someone-else", loaded: true, pinned: false }] });
          throw new Error("unexpected url: " + url);
        });
        const b = new OmlxBackend({ fetchImpl: f as never });
        const promise = b.load("Qwen3-8B", {
          mode: "on_demand",
          keepalive: { idle_ttl_s: 300 },
          admission: "requires_eviction",
        });
        await expect(promise).rejects.toBeInstanceOf(OmlxVerificationError);
        await expect(promise.catch((e) => e)).resolves.toMatchObject({ reason: "model_not_found" });
      });

      it("models 字段缺失 -> OmlxVerificationError(reason=models_missing)", async () => {
        const f = vi.fn(async (url: string) => {
          if (url.endsWith("/load")) return jsonRes({ status: "ok" });
          if (url.endsWith("/v1/models/status")) return jsonRes({}); // 没有 models 字段
          throw new Error("unexpected url: " + url);
        });
        const b = new OmlxBackend({ fetchImpl: f as never });
        const promise = b.load("Qwen3-8B", {
          mode: "on_demand",
          keepalive: { idle_ttl_s: 300 },
          admission: "requires_eviction",
        });
        await expect(promise).rejects.toBeInstanceOf(OmlxVerificationError);
        await expect(promise.catch((e) => e)).resolves.toMatchObject({ reason: "models_missing" });
      });

      it("pinned 字段不是 boolean -> OmlxVerificationError(reason=pinned_field_invalid)", async () => {
        const f = vi.fn(async (url: string) => {
          if (url.endsWith("/load")) return jsonRes({ status: "ok" });
          if (url.endsWith("/v1/models/status")) return jsonRes({ models: [{ id: "Qwen3-8B", loaded: true, pinned: "yes" }] });
          throw new Error("unexpected url: " + url);
        });
        const b = new OmlxBackend({ fetchImpl: f as never });
        const promise = b.load("Qwen3-8B", {
          mode: "on_demand",
          keepalive: { idle_ttl_s: 300 },
          admission: "requires_eviction",
        });
        await expect(promise).rejects.toBeInstanceOf(OmlxVerificationError);
        await expect(promise.catch((e) => e)).resolves.toMatchObject({ reason: "pinned_field_invalid" });
      });

      it("loaded 字段不是 boolean -> OmlxVerificationError(reason=loaded_field_invalid)", async () => {
        const f = vi.fn(async (url: string) => {
          if (url.endsWith("/load")) return jsonRes({ status: "ok" });
          if (url.endsWith("/v1/models/status")) return jsonRes({ models: [{ id: "Qwen3-8B", loaded: "yes", pinned: false }] });
          throw new Error("unexpected url: " + url);
        });
        const b = new OmlxBackend({ fetchImpl: f as never });
        const promise = b.load("Qwen3-8B", {
          mode: "on_demand",
          keepalive: { idle_ttl_s: 300 },
          admission: "requires_eviction",
        });
        await expect(promise).rejects.toBeInstanceOf(OmlxVerificationError);
        await expect(promise.catch((e) => e)).resolves.toMatchObject({ reason: "loaded_field_invalid" });
      });

      it("models 数组中有重复 id -> OmlxVerificationError(reason=duplicate_model_entries)", async () => {
        const f = vi.fn(async (url: string) => {
          if (url.endsWith("/load")) return jsonRes({ status: "ok" });
          if (url.endsWith("/v1/models/status"))
            return jsonRes({
              models: [
                { id: "Qwen3-8B", loaded: true, pinned: false },
                { id: "Qwen3-8B", loaded: true, pinned: true },
              ],
            });
          throw new Error("unexpected url: " + url);
        });
        const b = new OmlxBackend({ fetchImpl: f as never });
        const promise = b.load("Qwen3-8B", {
          mode: "on_demand",
          keepalive: { idle_ttl_s: 300 },
          admission: "requires_eviction",
        });
        await expect(promise).rejects.toBeInstanceOf(OmlxVerificationError);
        await expect(promise.catch((e) => e)).resolves.toMatchObject({ reason: "duplicate_model_entries" });
      });
    });

    describe("M1（本轮）负对照：verifyModelState 必须先校验顶层响应是不是一个普通对象", () => {
      const cases: Array<[string, unknown]> = [
        ["null", null],
        ["数字", 42],
        ["字符串", "not an object"],
        ["数组", ["not", "an", "object"]],
      ];
      for (const [label, topLevel] of cases) {
        it(`顶层响应是 ${label} 时 -> OmlxVerificationError(reason=response_invalid)，不抛 TypeError`, async () => {
          const f = vi.fn(async (url: string) => {
            if (url.endsWith("/load")) return jsonRes({ status: "ok" });
            if (url.endsWith("/v1/models/status")) return jsonRes(topLevel);
            throw new Error("unexpected url: " + url);
          });
          const b = new OmlxBackend({ fetchImpl: f as never });
          const promise = b.load("Qwen3-8B", {
            mode: "on_demand",
            keepalive: { idle_ttl_s: 300 },
            admission: "requires_eviction",
          });
          await expect(promise).rejects.toBeInstanceOf(OmlxVerificationError);
          await expect(promise).rejects.not.toBeInstanceOf(TypeError);
          await expect(promise.catch((e) => e)).resolves.toMatchObject({ reason: "response_invalid" });
        });
      }
    });
  });

  describe("maps /api/status", () => {
    it.each([
      ["null", null],
      ["数字", 42],
      ["字符串", "not an object"],
      ["数组", ["not", "an", "object"]],
    ])("M1（本轮）：顶层响应是 %s 时 status() 抛可读错误，不抛 TypeError", async (_label, topLevel) => {
      const f = vi.fn(async () => jsonRes(topLevel));
      const b = new OmlxBackend({ fetchImpl: f as never });
      const err = await b.status().catch((e: unknown) => e);
      expect(err).toBeInstanceOf(Error);
      expect(err).not.toBeInstanceOf(TypeError);
      expect((err as Error).message).toMatch(/JSON 对象/);
    });

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

    it("M-c: pressure not in the ok/soft/hard/ceiling whitelist -> returns 'unknown' + warns, does NOT throw (and loaded is still parsed)", async () => {
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
      try {
        const f = vi.fn(async () =>
          jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: ["A"], pressure: "critical" }),
        );
        const b = new OmlxBackend({ fetchImpl: f as never });
        const status = await b.status();
        expect(status.pressure).toBe("unknown");
        expect(status.loaded).toEqual(["A"]); // pressure 解析失败不连带炸掉 loaded
        expect(warnSpy).toHaveBeenCalledTimes(1);
        const warned = String(warnSpy.mock.calls[0]?.[0]);
        expect(warned).toContain("string"); // H2: 只报告类型
        expect(warned).not.toContain("critical"); // H2: 不把原值写进日志
      } finally {
        warnSpy.mockRestore();
      }
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

    it("M-a: loaded_models 里有非字符串元素时抛错，不能用 filter 静默丢弃", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: [{ id: "A" }] }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      await expect(b.status()).rejects.toThrow(/不是字符串/);
    });

    describe("M3: model_memory_max/used 必须严格是 number（不能用 Number(value) 那种宽松转换）", () => {
      it("缺失时抛错（fail-closed，不当成 0）", async () => {
        const f = vi.fn(async () => jsonRes({ model_memory_used: 0, loaded_models: [] }));
        const b = new OmlxBackend({ fetchImpl: f as never });
        await expect(b.status()).rejects.toThrow(/model_memory_max/);
      });

      it("数字字符串（例如 \"17179869184\"）被拒绝，不当成合法数字接受", async () => {
        const f = vi.fn(async () =>
          jsonRes({ model_memory_max: "17179869184", model_memory_used: 0, loaded_models: [] }),
        );
        const b = new OmlxBackend({ fetchImpl: f as never });
        await expect(b.status()).rejects.toThrow(/model_memory_max/);
      });

      it("boolean（true）被拒绝", async () => {
        const f = vi.fn(async () => jsonRes({ model_memory_max: true, model_memory_used: 0, loaded_models: [] }));
        const b = new OmlxBackend({ fetchImpl: f as never });
        await expect(b.status()).rejects.toThrow(/model_memory_max/);
      });

      it("数组（[]）被拒绝", async () => {
        const f = vi.fn(async () => jsonRes({ model_memory_max: [], model_memory_used: 0, loaded_models: [] }));
        const b = new OmlxBackend({ fetchImpl: f as never });
        await expect(b.status()).rejects.toThrow(/model_memory_max/);
      });

      it("负数被拒绝", async () => {
        const f = vi.fn(async () => jsonRes({ model_memory_max: -1, model_memory_used: 0, loaded_models: [] }));
        const b = new OmlxBackend({ fetchImpl: f as never });
        await expect(b.status()).rejects.toThrow(/model_memory_max/);
      });

      it("Infinity 被拒绝（不是有限数字）", async () => {
        const f = vi.fn(async () =>
          jsonRes({ model_memory_max: Number.POSITIVE_INFINITY, model_memory_used: 0, loaded_models: [] }),
        );
        const b = new OmlxBackend({ fetchImpl: f as never });
        await expect(b.status()).rejects.toThrow(/model_memory_max/);
      });
    });
  });

  describe("H2 哨兵测试：错误信息与 console 输出不含后端原始载荷", () => {
    it("parseLoaded 的\"不是数组\"错误不包含响应中的哨兵字符串", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: SENTINEL }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      const err = await b.status().catch((e: unknown) => e);
      expect(err).toBeInstanceOf(Error);
      expect((err as Error).message).not.toContain(SENTINEL);
    });

    it("parseLoaded 的\"元素不是字符串\"错误不包含响应中的哨兵字符串", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: [{ note: SENTINEL }] }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      const err = await b.status().catch((e: unknown) => e);
      expect(err).toBeInstanceOf(Error);
      expect((err as Error).message).not.toContain(SENTINEL);
    });

    it("parseMemoryGb 的错误不包含响应中的哨兵字符串", async () => {
      const f = vi.fn(async () =>
        jsonRes({ model_memory_max: SENTINEL, model_memory_used: 0, loaded_models: [] }),
      );
      const b = new OmlxBackend({ fetchImpl: f as never });
      const err = await b.status().catch((e: unknown) => e);
      expect(err).toBeInstanceOf(Error);
      expect((err as Error).message).not.toContain(SENTINEL);
    });

    it("parsePressure 的 console.warn 不包含响应中的哨兵字符串", async () => {
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
      try {
        const f = vi.fn(async () =>
          jsonRes({ model_memory_max: 0, model_memory_used: 0, loaded_models: [], pressure: SENTINEL }),
        );
        const b = new OmlxBackend({ fetchImpl: f as never });
        await b.status();
        expect(warnSpy).toHaveBeenCalledTimes(1);
        expect(String(warnSpy.mock.calls[0]?.[0])).not.toContain(SENTINEL);
      } finally {
        warnSpy.mockRestore();
      }
    });

    it("verifyModelState 的 OmlxVerificationError 不包含响应中的哨兵字符串（即便哨兵出现在无关字段里）", async () => {
      const f = vi.fn(async (url: string) => {
        if (url.endsWith("/load")) return jsonRes({ status: "ok" });
        if (url.endsWith("/v1/models/status"))
          return jsonRes({ models: [{ id: "Qwen3-8B", loaded: true, pinned: SENTINEL, note: SENTINEL }] });
        throw new Error("unexpected url: " + url);
      });
      const b = new OmlxBackend({ fetchImpl: f as never });
      const err = await b
        .load("Qwen3-8B", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" })
        .catch((e: unknown) => e);
      expect(err).toBeInstanceOf(OmlxVerificationError);
      expect((err as Error).message).not.toContain(SENTINEL);
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
