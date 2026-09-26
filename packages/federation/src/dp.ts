import type { Tensors } from "./payload.js";
import { makeRng, pairSeed, standardNormal } from "./prng.js";

/**
 * 高斯机制的噪声尺度（T3.4.1，Dwork-Roth 近似）：σ = Δ·√(2·ln(1.25/δ)) / ε。
 * ε 越小噪声越大 —— 测试把这个单调关系当成承重判据。
 */
export function gaussianSigma(opts: { epsilon: number; delta: number; sensitivity: number }): number {
  if (!(opts.epsilon > 0)) throw new Error("epsilon must be > 0");
  if (!(opts.delta > 0 && opts.delta < 1)) throw new Error("delta must be in (0,1)");
  if (!(opts.sensitivity > 0)) throw new Error("sensitivity must be > 0");
  return (opts.sensitivity * Math.sqrt(2 * Math.log(1.25 / opts.delta))) / opts.epsilon;
}

/** 对 LoRA 张量加高斯噪声。同 seed ⇒ 同噪声（可复现、可审计）。 */
export function addGaussianNoise(tensors: Tensors, sigma: number, seed: number): Tensors {
  if (!(sigma >= 0)) throw new Error("sigma must be >= 0");
  const rng = makeRng(seed);
  const out: Tensors = {};
  for (const [name, values] of Object.entries(tensors)) {
    out[name] = values.map((v) => v + sigma * standardNormal(rng));
  }
  return out;
}

/**
 * 安全聚合掩码（T3.4.1）：客户端 i 与 j 之间共享一个成对掩码 δ_ij，且 δ_ij = -δ_ji。
 * 求和时掩码相互抵消，服务器拿到的和等于明文之和；但**单个**客户端的上传量
 * 已经被掩码遮住，服务器无法单独读出它。
 */
export function pairwiseMask(tensors: Tensors, selfId: string, peers: readonly string[], seed: number): Tensors {
  const out: Tensors = {};
  for (const [name, values] of Object.entries(tensors)) {
    const acc = [...values];
    for (const peer of peers) {
      if (peer === selfId) continue;
      const rng = makeRng(pairSeed(selfId, peer, seed));
      const sign = selfId < peer ? 1 : -1;
      for (let i = 0; i < acc.length; i += 1) acc[i] = (acc[i] ?? 0) + sign * standardNormal(rng);
    }
    out[name] = acc;
  }
  return out;
}

/** 把一组（已掩码的）上传量相加。 */
export function sumTensors(list: readonly Tensors[]): Tensors {
  const out: Tensors = {};
  for (const tensors of list) {
    for (const [name, values] of Object.entries(tensors)) {
      const acc = out[name] ?? new Array<number>(values.length).fill(0);
      for (let i = 0; i < acc.length; i += 1) acc[i] = (acc[i] ?? 0) + (values[i] ?? 0);
      out[name] = acc;
    }
  }
  return out;
}
