import { describe, expect, it } from "vitest";
import {
  AdapterGateError,
  assertAdapterMountable,
  assertAdaptersAggregatable,
  validateAdapterManifest,
  type BaseIdentity,
} from "../../src/adapter-gate.js";

const D = (c: string): string => "sha256:" + c.repeat(64);
const loaded: BaseIdentity = { base_model_id: "Qwen3-4B-mlx", base_digest: D("a"), tokenizer_digest: D("b") };
const manifest = (over: Record<string, unknown> = {}) => ({
  adapter_id: "lora-1",
  base_model_id: loaded.base_model_id,
  base_digest: loaded.base_digest,
  tokenizer_digest: loaded.tokenizer_digest,
  rank: 16,
  data_class: "synthetic",
  ...over,
});

/** 只取值以便断言，避免 try/catch 吞掉「抛了别的异常」这种情况。 */
const code = (fn: () => unknown): string => {
  try {
    fn();
  } catch (err) {
    return err instanceof AdapterGateError ? err.code : "NOT_GATE_ERROR:" + String(err);
  }
  return "NO_THROW";
};

describe("T3.2.1 adapter manifest 结构校验", () => {
  it("合法 manifest 通过并保留字段", () => {
    const m = validateAdapterManifest(manifest());
    expect(m.adapter_id).toBe("lora-1");
    expect(m.rank).toBe(16);
    expect(m.data_class).toBe("synthetic");
  });
  it("指纹格式不合法即拒绝（不接受「短一点的 sha256」）", () => {
    expect(code(() => validateAdapterManifest(manifest({ base_digest: "sha256:abc" })))).toBe("INVALID_MANIFEST");
  });
  it("缺 data_class 即拒绝（训练数据来源必须声明）", () => {
    const { data_class: _drop, ...rest } = manifest();
    expect(code(() => validateAdapterManifest(rest))).toBe("INVALID_MANIFEST");
  });
  it("rank 越界即拒绝", () => {
    expect(code(() => validateAdapterManifest(manifest({ rank: 0 })))).toBe("INVALID_MANIFEST");
    expect(code(() => validateAdapterManifest(manifest({ rank: 300 })))).toBe("INVALID_MANIFEST");
  });
});

describe("T3.2.1 挂载门禁：指纹而不是名字", () => {
  it("base/tokenizer 全一致才放行", () => {
    expect(assertAdapterMountable(manifest(), loaded).adapter_id).toBe("lora-1");
  });
  it("**同名但 base_digest 不同 → 拒绝**（这是「一次静默升级毁掉整批 LoRA」）", () => {
    const silentlyUpgraded: BaseIdentity = { ...loaded, base_digest: D("c") };
    expect(code(() => assertAdapterMountable(manifest(), silentlyUpgraded))).toBe("BASE_DIGEST_MISMATCH");
  });
  it("tokenizer_digest 不同 → 拒绝", () => {
    expect(code(() => assertAdapterMountable(manifest(), { ...loaded, tokenizer_digest: D("d") }))).toBe("TOKENIZER_DIGEST_MISMATCH");
  });
  it("base_model_id 不同 → 拒绝", () => {
    expect(code(() => assertAdapterMountable(manifest(), { ...loaded, base_model_id: "Qwen3-8B-mlx" }))).toBe("BASE_MODEL_MISMATCH");
  });
  it("目标没有真指纹 → 拒绝，绝不「没得比就放行」", () => {
    expect(code(() => assertAdapterMountable(manifest(), { ...loaded, base_digest: "" }))).toBe("MISSING_BASE_IDENTITY");
    expect(code(() => assertAdapterMountable(manifest(), { ...loaded, base_digest: "unknown" }))).toBe("MISSING_BASE_IDENTITY");
    expect(code(() => assertAdapterMountable(manifest(), { ...loaded, tokenizer_digest: "" }))).toBe("MISSING_BASE_IDENTITY");
    expect(code(() => assertAdapterMountable(manifest(), { ...loaded, base_model_id: "" }))).toBe("MISSING_BASE_IDENTITY");
  });
});

describe("T3.2.1 聚合门禁：不同底座不得一起平均", () => {
  it("全部同底座 → 返回全部", () => {
    const out = assertAdaptersAggregatable([manifest(), manifest({ adapter_id: "lora-2" })], loaded);
    expect(out.map((m) => m.adapter_id)).toEqual(["lora-1", "lora-2"]);
  });
  it("有一条 base_digest 不同 → 整体拒绝（不静默跳过）", () => {
    const mixed = [manifest(), manifest({ adapter_id: "lora-2", base_digest: D("c") })];
    expect(code(() => assertAdaptersAggregatable(mixed, loaded))).toBe("AGGREGATION_BASE_MISMATCH");
  });
  it("有一条 tokenizer_digest 不同 → 整体拒绝", () => {
    const mixed = [manifest(), manifest({ adapter_id: "lora-2", tokenizer_digest: D("d") })];
    expect(code(() => assertAdaptersAggregatable(mixed, loaded))).toBe("AGGREGATION_BASE_MISMATCH");
  });
  it("拒绝时带上是哪一条 adapter（否则定位不到）", () => {
    try {
      assertAdaptersAggregatable([manifest(), manifest({ adapter_id: "lora-2", base_digest: D("c") })], loaded);
      throw new Error("should have thrown");
    } catch (err) {
      expect(err).toBeInstanceOf(AdapterGateError);
      expect((err as AdapterGateError).adapterId).toBe("lora-2");
    }
  });
  it("空列表且底座合法 → 空数组（无操作，不是错误）", () => {
    expect(assertAdaptersAggregatable([], loaded)).toEqual([]);
  });
  it("空列表但底座缺指纹 → 仍然拒绝（门禁不因「没什么可校验」而失效）", () => {
    expect(code(() => assertAdaptersAggregatable([], { ...loaded, base_digest: "" }))).toBe("MISSING_BASE_IDENTITY");
  });
});
