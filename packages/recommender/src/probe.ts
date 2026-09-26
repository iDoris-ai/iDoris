import { cpus, platform, totalmem } from "node:os";

/**
 * T2.1.1 — 硬件探测（可注入）。
 *
 * 设计原则：**推荐模块的决策只读 `HostFacts`**，真实探测（`node:os` /
 * `sysctl` / `system_profiler`）只是把外部世界收敛成 `HostFacts` 的一个适配器。
 * 测试全部注入固定 facts，因此不依赖测试机硬件。
 *
 * 探测来源（docs/07 §5.1 / docs/13 §3）：
 *  - `os.totalmem()` → ram_gb（按 2^30 归一到标称 GB）
 *  - `os.cpus()[0].model` → chip（"Apple M4" → "M4"）
 *  - `system_profiler SPDisplaysDataType` → gpu_cores
 */

export interface HostFacts {
  /** 标称内存，如 24 / 64（GB）。 */
  ram_gb: number;
  /** 芯片型号，如 "M4" / "M1 Max"；未知为 "unknown"。 */
  chip: string;
  /** GPU 核心数；探测不到为 null。 */
  gpu_cores: number | null;
  /** 平台，如 "darwin" / "linux" / "win32"。 */
  os: string;
  /** facts 的来源：注入 or 真实探测。 */
  source: "injected" | "probe";
}

/** 可注入的原始输入：只要求 ram_gb，其它给默认值。 */
export interface HostFactsInput {
  ram_gb: number;
  chip?: string;
  gpu_cores?: number | null;
  os?: string;
}

export const UNKNOWN_CHIP = "unknown";

/** os.totalmem() 的字节数 → 标称 GB（Apple 的内存标称按 GiB 取整）。 */
export function nominalRamGb(totalMemBytes: number): number {
  if (!Number.isFinite(totalMemBytes) || totalMemBytes <= 0) {
    throw new Error("totalMemBytes 必须是正数");
  }
  return Math.round(totalMemBytes / 1024 ** 3);
}

/** "Apple M4" / "Apple M1 Max" → "M4" / "M1 Max"。 */
export function parseChip(cpuModel: string | undefined): string {
  if (!cpuModel) return UNKNOWN_CHIP;
  const match = /Apple\s+(M\d+(?:\s+(?:Pro|Max|Ultra))?)/.exec(cpuModel);
  if (match?.[1]) return match[1];
  return cpuModel.replace(/^Apple\s+/, "").trim() || UNKNOWN_CHIP;
}

/** 从 `system_profiler SPDisplaysDataType` 文本里取 GPU 核数。 */
export function parseSystemProfilerGpuCores(output: string): number | null {
  const match = /Total Number of Cores:\s*(\d+)/.exec(output);
  if (!match?.[1]) return null;
  const cores = Number.parseInt(match[1], 10);
  return Number.isFinite(cores) ? cores : null;
}

/** 校验并补齐注入的 facts。 */
export function makeHostFacts(input: HostFactsInput): HostFacts {
  if (!Number.isFinite(input.ram_gb) || input.ram_gb <= 0) {
    throw new Error("ram_gb 必须是正数");
  }
  return {
    ram_gb: input.ram_gb,
    chip: input.chip && input.chip.length > 0 ? input.chip : UNKNOWN_CHIP,
    gpu_cores: input.gpu_cores ?? null,
    os: input.os && input.os.length > 0 ? input.os : "unknown",
    source: "injected",
  };
}

/** 真实探测（best-effort）。只导入 node:os，不 fork 任何进程。 */
export function inspectHost(): HostFacts {
  const cpu = cpus()[0];
  return {
    ram_gb: nominalRamGb(totalmem()),
    chip: parseChip(cpu?.model),
    gpu_cores: null,
    os: platform(),
    source: "probe",
  };
}

/** 把 system_profiler 文本并入探测结果（供 macOS 适配器调用）。 */
export function withSystemProfiler(base: HostFacts, systemProfilerOutput: string): HostFacts {
  return {
    ...base,
    gpu_cores: parseSystemProfilerGpuCores(systemProfilerOutput),
  };
}

