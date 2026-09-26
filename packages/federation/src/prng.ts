/** 字符串 → 32 位 hash（FNV-1a）。 */
export function hashString(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i += 1) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

/** xorshift32：确定性 PRNG。同 seed 同噪声 —— 联邦实验要能复现与审计。 */
export function makeRng(seed: number): () => number {
  let x = seed === 0 ? 0x9e3779b9 : seed >>> 0;
  return () => {
    x ^= x << 13;
    x >>>= 0;
    x ^= x >>> 17;
    x ^= x << 5;
    x >>>= 0;
    return x / 0x100000000;
  };
}

/** Box-Muller，标准正态。 */
export function standardNormal(rng: () => number): number {
  const u1 = Math.max(rng(), Number.MIN_VALUE);
  const u2 = rng();
  return Math.sqrt(-2 * Math.log(u1)) * Math.cos(2 * Math.PI * u2);
}

/** 无序对 (a,b) 的共享种子：两台机器算出同一个值，这是掩码能抵消的前提。 */
export function pairSeed(a: string, b: string, seed: number): number {
  const [x, y] = a < b ? [a, b] : [b, a];
  return (hashString(x + "|" + y) ^ (seed >>> 0)) >>> 0;
}
