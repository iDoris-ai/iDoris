import { describe, expect, it, vi } from "vitest";
// M2：从包入口（src/index.ts，`.` 导出映射到它）import，而不是直接从
// `omlx/omlx-backend.js` 导入——验证这几个错误类型确实是公开 API 的一部分，
// 不是只有包内部代码才够得着。
import { OmlxBackend, OmlxPinUnavailableError, OmlxUnexpectedlyPinnedError, OmlxVerificationError } from "../src/index.js";

const jsonRes = (body: unknown, status = 200) => ({
  ok: status < 400,
  status,
  json: async () => body,
  text: async () => JSON.stringify(body),
});

describe("@idoris/adapters 包入口导出 oMLX 错误类型（M2）", () => {
  it("OmlxPinUnavailableError：instanceof 判断成立，code 字段稳定", async () => {
    const f = vi.fn(async (url: string) => {
      if (url.endsWith("/load")) return jsonRes({ status: "ok" });
      return jsonRes({ detail: "Admin authentication required" }, 401);
    });
    const b = new OmlxBackend({ fetchImpl: f as never });
    const err = await b
      .load("Qwen3-8B", { mode: "resident", keepalive: { pinned: true }, admission: "coexist" })
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(OmlxPinUnavailableError);
    expect((err as OmlxPinUnavailableError).code).toBe("OMLX_PIN_UNAVAILABLE");
  });

  it("OmlxUnexpectedlyPinnedError：instanceof 判断成立，code 字段稳定", async () => {
    const f = vi.fn(async (url: string) => {
      if (url.endsWith("/load")) return jsonRes({ status: "ok" });
      if (url.endsWith("/v1/models/status")) return jsonRes({ models: [{ id: "Qwen3-8B", loaded: true, pinned: true }] });
      throw new Error("unexpected url: " + url);
    });
    const b = new OmlxBackend({ fetchImpl: f as never });
    const err = await b
      .load("Qwen3-8B", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" })
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(OmlxUnexpectedlyPinnedError);
    expect((err as OmlxUnexpectedlyPinnedError).code).toBe("OMLX_UNEXPECTEDLY_PINNED");
  });

  it("OmlxVerificationError：instanceof 判断成立，code 字段稳定，reason 可读", async () => {
    const f = vi.fn(async (url: string) => {
      if (url.endsWith("/load")) return jsonRes({ status: "ok" });
      if (url.endsWith("/v1/models/status")) return jsonRes({ models: [] }); // model_not_found
      throw new Error("unexpected url: " + url);
    });
    const b = new OmlxBackend({ fetchImpl: f as never });
    const err = await b
      .load("Qwen3-8B", { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" })
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(OmlxVerificationError);
    expect((err as OmlxVerificationError).code).toBe("OMLX_VERIFICATION_FAILED");
    expect((err as OmlxVerificationError).reason).toBe("model_not_found");
  });
});
