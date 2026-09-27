import { describe, expect, it } from "vitest";
import {
  FederationError,
  LocalClient,
  assertWeightsOnly,
  fedAvg,
  runRound,
  type AdapterUpdate,
  type LocalSample,
} from "../src/index.js";

/** 只在原始样本里出现的哨兵：它跨机出现 = 原始数据泄漏。 */
const SENTINEL = "SENTINEL_RAW_SAMPLE_9137";

const base = {
  base_model_id: "Qwen3-4B-mlx",
  base_digest: "sha256:" + "a".repeat(64),
  tokenizer_digest: "sha256:" + "b".repeat(64),
};

const samples = (n: number, tag: string): LocalSample[] =>
  Array.from({ length: n }, (_, i) => ({
    prompt: SENTINEL + " prompt " + tag + " " + i,
    response: "response " + tag + " " + i,
  }));

const twoClients = () => {
  const a = new LocalClient({ clientId: "client-a", base });
  const b = new LocalClient({ clientId: "client-b", base });
  a.addSamples(samples(3, "a"));
  b.addSamples(samples(1, "b"));
  return [a, b];
};

describe("T3.3.1 两个本地客户端 + 合成数据跑通一轮聚合", () => {
  it("一轮跑通：2 个客户端 → FedAvg，样本数加权", () => {
    const won: AdapterUpdate[] = [];
    const { aggregate, transmitted } = runRound(twoClients(), (u) => won.push(u));
    expect(transmitted).toHaveLength(2);
    expect(won).toHaveLength(2);
    expect(aggregate.clients).toEqual(["client-a", "client-b"]);
    expect(aggregate.totalSamples).toBe(4);
    expect(Object.keys(aggregate.tensors)).toEqual(["layers.0.q_proj.lora_a"]);
  });

  it("**传输载荷只含 adapter 权重**：原始样本哨兵不出现在任何跨机字节里", () => {
    const wire: string[] = [];
    const { transmitted } = runRound(twoClients(), (u) => wire.push(JSON.stringify(u)));
    const payload = wire.join("\n");
    // 哨兵确实在本地样本里（否则这条断言不承重）
    expect(SENTINEL).toBeTruthy();
    expect(samples(1, "a")[0]?.prompt).toContain(SENTINEL);
    expect(payload.length).toBeGreaterThan(0);
    expect(payload).not.toContain(SENTINEL);
    expect(payload).not.toContain("prompt");
    expect(payload).not.toContain("response");
    for (const u of transmitted) {
      expect(Object.keys(u)).toEqual(["client_id", "base", "rank", "data_class", "sample_count", "tensors"]);
    }
  });

  it("同一批客户端与样本 → 聚合结果可复现", () => {
    const first = runRound(twoClients(), () => {}).aggregate.tensors;
    const second = runRound(twoClients(), () => {}).aggregate.tensors;
    expect(first).toEqual(second);
  });

  it("样本数真的进入加权（不是简单平均）", () => {
    const a = new LocalClient({ clientId: "a", base });
    const b = new LocalClient({ clientId: "b", base });
    // 两个客户端的权重不同：a 的 sample_count=3, b 的 =1
    a.addSamples([{ prompt: "a1", response: "r" }, { prompt: "a2", response: "r" }, { prompt: "a3", response: "r" }]);
    b.addSamples([{ prompt: "b1", response: "r" }]);
    const updates = [a.train(), b.train()];
    const plain = updates[0]?.tensors["layers.0.q_proj.lora_a"]?.[0] ?? 0;
    const avg = fedAvg(updates, base).tensors["layers.0.q_proj.lora_a"]?.[0] ?? 0;
    const naive = ((updates[0]?.tensors["layers.0.q_proj.lora_a"]?.[0] ?? 0) + (updates[1]?.tensors["layers.0.q_proj.lora_a"]?.[0] ?? 0)) / 2;
    expect(avg).not.toBeCloseTo(naive, 10);
    expect(plain).toBeGreaterThan(0);
  });

  it("不同底座不得一起平均（复用 T3.2.1 指纹门禁）", () => {
    const a = new LocalClient({ clientId: "a", base });
    const b = new LocalClient({ clientId: "b", base: { ...base, base_digest: "sha256:" + "c".repeat(64) } });
    a.addSamples(samples(1, "a"));
    b.addSamples(samples(1, "b"));
    expect(() => fedAvg([a.train(), b.train()], base)).toThrow(/same name is not the same weights/);
  });

  it("载荷里出现 samples/messages 字段 → 直接拒绝", () => {
    const client = new LocalClient({ clientId: "a", base });
    client.addSamples(samples(1, "a"));
    const update = client.train();
    const leaky = { ...update, samples: [{ prompt: SENTINEL }] } as unknown as AdapterUpdate;
    expect(() => assertWeightsOnly(leaky)).toThrow(FederationError);
  });

  it("没有本地样本的客户端无法产出一轮更新", () => {
    const empty = new LocalClient({ clientId: "empty", base });
    expect(() => empty.train()).toThrow(FederationError);
  });
});

describe("T3.4.2 真实数据准入门禁（报错，不是跳过）", () => {
  const realClient = () => {
    const c = new LocalClient({ clientId: "real-1", base, dataClass: "real" });
    c.addSamples(samples(2, "real"));
    return c;
  };

  it("隐私层未启用 → 真实数据客户端报错，且**不发出任何载荷**", () => {
    const wire: string[] = [];
    try {
      runRound([realClient()], (u) => wire.push(JSON.stringify(u)), false);
      throw new Error("should have thrown");
    } catch (err) {
      expect(err).toBeInstanceOf(FederationError);
      expect((err as FederationError).code).toBe("REAL_DATA_NOT_ADMISSIBLE");
    }
    expect(wire).toEqual([]);
  });

  it("隐私层启用后同一客户端可参与", () => {
    const wire: string[] = [];
    const { aggregate } = runRound([realClient()], (u) => wire.push(JSON.stringify(u)), true);
    expect(wire).toHaveLength(1);
    expect(aggregate.totalSamples).toBe(2);
  });

  it("合成客户端在隐私层未启用时不受影响", () => {
    const wire: string[] = [];
    runRound(twoClients(), (u) => wire.push(JSON.stringify(u)), false);
    expect(wire).toHaveLength(2);
  });
});
