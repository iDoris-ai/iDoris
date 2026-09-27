import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { trainingSampleSchema } from "@idoris/contracts";
import { describe, expect, it } from "vitest";
import {
  DataLake,
  DataLakeError,
  RefineError,
  SYNTHETIC_SEEDS,
  refineToTrainingSamples,
  synthesizeUsage,
  type UsageEvent,
} from "../src/index.js";

const NOW = 1_700_000_000_000;
const newLake = (opts: { privacyLayerEnabled?: boolean } = {}) =>
  new DataLake(mkdtempSync(join(tmpdir(), "idoris-lake-")), opts);
const events = (): UsageEvent[] => synthesizeUsage(SYNTHETIC_SEEDS, { repeats: 2, now: NOW });
const realEvent = (): UsageEvent => ({
  event_id: "real-1", ts_utc: 1, intent: "chat", privacy: "local_only",
  data_class: "real", outcome: "accepted", prompt: "p", response: "r",
});

describe("T3.1.1 使用 → 数据湖 → 提炼 三段跑通", () => {
  it("合成语料跑通三段，产出的训练样本 schema 合法", () => {
    const lake = newLake();
    for (const e of events()) lake.append(e);
    expect(lake.size()).toBe(8);
    const { samples, skipped } = refineToTrainingSamples(lake.all());
    expect(skipped).toBe(1);
    expect(samples).toHaveLength(7);
    for (const s of samples) {
      expect(trainingSampleSchema.safeParse(s).success).toBe(true);
      expect(s.messages.map((m) => m.role)).toEqual(["user", "assistant"]);
      expect(s.group_id).toBeDefined();
    }
  });

  it("湖是本地 append-only JSONL：落盘、可重开读回", () => {
    const dir = mkdtempSync(join(tmpdir(), "idoris-lake-"));
    const lake = new DataLake(dir);
    for (const e of events()) lake.append(e);
    const lines = readFileSync(join(dir, "events.jsonl"), "utf8").split("\n").filter((l) => l !== "");
    expect(lines).toHaveLength(8);
    expect(new DataLake(dir).size()).toBe(8);
    expect(new DataLake(dir).byIntent("coding")).toHaveLength(2);
  });

  it("合成是确定性的（同样入参同样产物）", () => {
    expect(synthesizeUsage(SYNTHETIC_SEEDS, { repeats: 2, now: NOW })).toEqual(events());
  });
});

describe("T3.1.1 产物不含真实数据标记（F3.4 硬门禁）", () => {
  it("提炼产物里没有任何 real 标记", () => {
    const { samples } = refineToTrainingSamples(events());
    expect(samples.length).toBeGreaterThan(0);
    expect(samples.filter((s) => s.data_class === "real")).toEqual([]);
    expect(new Set(samples.map((s) => s.data_class))).toEqual(new Set(["synthetic"]));
    expect(samples.filter((s) => String(s.source.kind).includes("real"))).toEqual([]);
  });

  it("标记为 real 的事件进湖即报错（报错，不是静默丢弃）", () => {
    const lake = newLake();
    try {
      lake.append(realEvent());
      throw new Error("should have thrown");
    } catch (err) {
      expect(err).toBeInstanceOf(DataLakeError);
      expect((err as DataLakeError).code).toBe("REAL_DATA_NOT_ADMISSIBLE");
    }
    expect(lake.size()).toBe(0);
  });

  it("real 混在提炼输入里也报错而不是跳过", () => {
    expect(() => refineToTrainingSamples([...events(), realEvent()])).toThrow(RefineError);
  });

  it("隐私层启用后 real 才可入湖（F3.4.2 的钩子）", () => {
    const lake = newLake({ privacyLayerEnabled: true });
    expect(() => lake.append(realEvent())).not.toThrow();
    expect(lake.size()).toBe(1);
  });

  it("缺 event_id 的事件被拒绝", () => {
    const lake = newLake();
    expect(() => lake.append({ ...realEvent(), data_class: "synthetic", event_id: "" })).toThrow(DataLakeError);
  });
});
