/**
 * T2.1.1 — 内存评估公式（docs/07 §1–§3；Apple 预算按 docs/13 §3.1 修正版）。
 *
 *   footprint(GB) = params_total_b × bpp(quant) + KV(ctx) + overhead
 *
 * 单位约定（与源头实测对齐，务必不要"顺手统一"）：
 *  - 权重：HF 实测文件字节数是**十进制 GB**（1 GB = 1e9 B），
 *    `weights_gb` 直接采用；只有 bpp 时按 params_total_b × bpp 得到十进制 GB。
 *  - KV：U0-LOG 实测的内存读数"MB"是 **MiB（2^20 B）**。
 *    Qwen3-8B（36 层 / 8 KV head / head_dim 128）的 KV = **9.00 MiB / 64 token**，
 *    因此 KV 的 MB 读数用 `bytesToMib`，预算合计用十进制 GB（`bytesToGb`）。
 *  - 预算：RAM 的标称 GB 与 docs/13 §3.1 表格一致（24GB → conservative 15.84GB）。
 *
 * 公式真源：docs/07 §1/§2/§3，修正公式：docs/13 §3.1。
 */

export type WiredMode = "conservative" | "moderate" | "aggressive";

/** KV 量化档：fp16 = 2B/elem，q8 = 1B，q4 = 0.5B（docs/07 §1）。 */
export type KvQuant = "fp16" | "q8" | "q4";

export interface ModelArch {
  n_layers: number;
  n_kv_heads: number;
  head_dim: number;
}

/** 一个量化选项：`weights_gb`（HF 实测）优先，否则用 `bpp` × params。 */
export interface QuantSpec {
  label?: string;
  bpp?: number;
  weights_gb?: number;
  quality: number;
}

export interface QuantEntry {
  bpp: number;
  quality: number;
}

/** docs/07 §2 的量化表：bytes/参数 + 质量保留。 */
export const QUANT_TABLE: Readonly<Record<string, QuantEntry>> = Object.freeze({
  fp16: { bpp: 2.0, quality: 1.0 },
  q8_0: { bpp: 1.0, quality: 0.998 },
  q6_k: { bpp: 0.82, quality: 0.995 },
  q5_k_m: { bpp: 0.69, quality: 0.99 },
  q4_k_m: { bpp: 0.55, quality: 0.98 },
  q3_k: { bpp: 0.43, quality: 0.95 },
  q2_k: { bpp: 0.3, quality: 0.85 },
});

export const KV_BYTES_PER_ELEMENT: Readonly<Record<KvQuant, number>> = Object.freeze({
  fp16: 2,
  q8: 1,
  q4: 0.5,
});

/** 运行时/激活/碎片开销，docs/07 §1 给 0.5–1.5GB；docs/13 §5 表格统一用 1.0GB。 */
export const DEFAULT_OVERHEAD_GB = 1.0;

const BYTES_PER_GB = 1e9;
const BYTES_PER_MIB = 1024 * 1024;
const BYTES_PER_GIB = 1024 * 1024 * 1024;

export function bytesToGb(bytes: number): number {
  return bytes / BYTES_PER_GB;
}

/** U0-LOG / macOS 内存工具的“MB”读数口径。 */
export function bytesToMib(bytes: number): number {
  return bytes / BYTES_PER_MIB;
}

export function bytesToGib(bytes: number): number {
  return bytes / BYTES_PER_GIB;
}

/** 权重字节数：weights_gb 优先，否则 params_total_b × bpp。 */
export function weightBytes(paramsTotalB: number, spec: QuantSpec): number {
  if (spec.weights_gb !== undefined) {
    return spec.weights_gb * BYTES_PER_GB;
  }
  if (spec.bpp !== undefined) {
    // params_total_b 以 10 亿为单位，bpp 是 bytes/参数，两者相乘即十进制 GB。
    return paramsTotalB * spec.bpp * BYTES_PER_GB;
  }
  throw new Error("quant spec 缺少 weights_gb 或 bpp");
}

export function weightsGb(paramsTotalB: number, spec: QuantSpec): number {
  return bytesToGb(weightBytes(paramsTotalB, spec));
}

/**
 * KV 缓存字节数：2 × n_layers × n_kv_heads × head_dim × ctx × kv_bytes。
 * GQA 下 n_kv_heads 很小；MoE 内存按总参数算，但 KV 只由 arch 决定。
 */
export function kvBytes(arch: ModelArch, ctx: number, kvQuant: KvQuant = "fp16"): number {
  return 2 * arch.n_layers * arch.n_kv_heads * arch.head_dim * ctx * KV_BYTES_PER_ELEMENT[kvQuant];
}

export function kvCacheGb(arch: ModelArch, ctx: number, kvQuant: KvQuant = "fp16"): number {
  return bytesToGb(kvBytes(arch, ctx, kvQuant));
}

export interface FootprintInput {
  params_total_b: number;
  quant: QuantSpec;
  arch: ModelArch;
  ctx: number;
  kv_quant?: KvQuant;
  overhead_gb?: number;
}

/** footprint = 权重 + KV(ctx) + 开销（GB，十进制）。 */
export function footprintGb(input: FootprintInput): number {
  const weights = weightsGb(input.params_total_b, input.quant);
  const kv = kvCacheGb(input.arch, input.ctx, input.kv_quant ?? "fp16");
  const overhead = input.overhead_gb ?? DEFAULT_OVERHEAD_GB;
  return weights + kv + overhead;
}

// ---------------------------------------------------------------------------
// Apple Silicon 可用预算（docs/13 §3.1 修正 docs/07 §3）
// ---------------------------------------------------------------------------

export const WIRED_PCT: Readonly<Record<WiredMode, number>> = Object.freeze({
  conservative: 0.66,
  moderate: 0.7,
  aggressive: 0.75,
});

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/** 给系统的绝对保留：clamp(RAM × 0.30, 3, 16)。 */
export function appleReserveGb(ramGb: number): number {
  return clamp(ramGb * 0.3, 3, 16);
}

/** usable = min(RAM × pct, RAM − reserve)。 */
export function appleUsableGb(ramGb: number, mode: WiredMode = "conservative"): number {
  return Math.min(ramGb * WIRED_PCT[mode], ramGb - appleReserveGb(ramGb));
}

/** docs/07 §5.3：iogpu.wired_limit_mb = usable × 1024。 */
export function recommendedWiredLimitMb(usableGb: number): number {
  return Math.round(usableGb * 1024);
}

/** 按 label 在量化表里取 bpp/quality；未知 label 返回 undefined。 */
export function lookupQuant(label: string): QuantEntry | undefined {
  return QUANT_TABLE[label];
}
