import { describe, expect, it } from "vitest";
import { addGaussianNoise, gaussianSigma, pairwiseMask, sumTensors, type Tensors } from "../src/index.js";

const plain: Tensors = { "layers.0.q_proj.lora_a": [0.5, -1.25, 3.0, 0.0] };

describe("T3.4.1 高斯机制噪声尺度", () => {
  it("ε 越小噪声越大（单调，这是承重判据）", () => {
    const s = (epsilon: number) => gaussianSigma({ epsilon, delta: 1e-5, sensitivity: 1 });
    expect(s(0.1)).toBeGreaterThan(s(1));
    expect(s(1)).toBeGreaterThan(s(10));
    expect(s(10)).toBeGreaterThan(0);
  });

  it("δ 越小噪声越大", () => {
    const s = (delta: number) => gaussianSigma({ epsilon: 1, delta, sensitivity: 1 });
    expect(s(1e-8)).toBeGreaterThan(s(1e-3));
  });

  it("与解析式一致（钉住公式，而不是只钉住方向）", () => {
    const sigma = gaussianSigma({ epsilon: 0.5, delta: 1e-5, sensitivity: 2 });
    expect(sigma).toBeCloseTo((2 * Math.sqrt(2 * Math.log(1.25 / 1e-5))) / 0.5, 10);
  });

  it("非法参数直接拒绝", () => {
    expect(() => gaussianSigma({ epsilon: 0, delta: 1e-5, sensitivity: 1 })).toThrow();
    expect(() => gaussianSigma({ epsilon: 1, delta: 1, sensitivity: 1 })).toThrow();
    expect(() => gaussianSigma({ epsilon: 1, delta: 1e-5, sensitivity: 0 })).toThrow();
  });
});

describe("T3.4.1 加噪与可复现", () => {
  it("同 seed 同噪声，不同 seed 不同噪声", () => {
    const a = addGaussianNoise(plain, 0.1, 42);
    const b = addGaussianNoise(plain, 0.1, 42);
    const c = addGaussianNoise(plain, 0.1, 43);
    expect(a).toEqual(b);
    expect(a).not.toEqual(c);
  });

  it("σ=0 时原样返回（噪声是天平，不是变形）", () => {
    expect(addGaussianNoise(plain, 0, 7)).toEqual(plain);
  });

  it("噪声确实改变了权重，但形状与张量名不变", () => {
    const noisy = addGaussianNoise(plain, 0.5, 1);
    expect(Object.keys(noisy)).toEqual(Object.keys(plain));
    expect(noisy["layers.0.q_proj.lora_a"]).toHaveLength(4);
    expect(noisy["layers.0.q_proj.lora_a"]).not.toEqual(plain["layers.0.q_proj.lora_a"]);
  });
});

describe("T3.4.1 安全聚合：掩码在求和时抵消，单个上传量被遮住", () => {
  const ids = ["c1", "c2", "c3"];
  const updates: Tensors[] = [
    { t: [1, 2, 3, 4] },
    { t: [10, 20, 30, 40] },
    { t: [100, 200, 300, 400] },
  ];

  it("掩码后的和 == 明文的和（掩码真的抵消）", () => {
    const masked = updates.map((u, i) => pairwiseMask(u, ids[i] as string, ids, 2026));
    const maskedSum = sumTensors(masked).t?.[0] ?? 0;
    const plainSum = sumTensors(updates).t?.[0] ?? 0;
    expect(maskedSum).toBeCloseTo(plainSum, 9);
  });

  it("单个客户端的掩码上传量 ≠ 明文（服务器读不出这一份）", () => {
    const masked = pairwiseMask(updates[0] as Tensors, ids[0] as string, ids, 2026);
    expect(masked.t).not.toEqual(updates[0]?.t);
  });

  it("对端算出的掩码符号相反（δ_ij = -δ_ji 的实现前提）", () => {
    const ij = pairwiseMask({ t: [0] }, "c1", ["c2"], 2026).t?.[0] ?? 0;
    const ji = pairwiseMask({ t: [0] }, "c2", ["c1"], 2026).t?.[0] ?? 0;
    expect(ij + ji).toBeCloseTo(0, 12);
  });

  it("三客户端两两掩码全加起来仍然精确抵消", () => {
    const masked = updates.map((u, i) => pairwiseMask(u, ids[i] as string, ids, 7));
    const sum = sumTensors(masked).t ?? [];
    const expectSum = sumTensors(updates).t ?? [];
    for (let i = 0; i < sum.length; i += 1) expect(sum[i]).toBeCloseTo(expectSum[i] ?? 0, 9);
  });
});
