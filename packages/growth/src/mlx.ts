import { spawnSync } from "node:child_process";

export interface MlxProbe {
  available: boolean;
  reason: string;
  python: string;
  version: string;
}

/** mlx-lm 需要 python >= 3.10（FU-2：本机曾实测 3.9.6）。 */
export const MLX_MIN_PYTHON_MINOR = 10;

/**
 * 探测本机是否具备 MLX-LoRA 训练条件（T3.1.2）。
 * 不做「假设能跑」——探不到就返回 available=false 让集成测试打印 SKIPPED。
 */
export function detectMlx(python = process.env.IDORIS_PYTHON ?? "python3"): MlxProbe {
  const versionProbe = spawnSync(python, ["-c", "import sys;print('%d.%d.%d'%sys.version_info[:3])"], { encoding: "utf8" });
  if (versionProbe.status !== 0) {
    return { available: false, reason: "python not runnable: " + python, python, version: "" };
  }
  const version = (versionProbe.stdout ?? "").trim();
  const parts = version.split(".");
  const major = Number(parts[0]);
  const minor = Number(parts[1]);
  if (!(major > 3 || (major === 3 && minor >= MLX_MIN_PYTHON_MINOR))) {
    return { available: false, reason: "python " + version + " < 3." + MLX_MIN_PYTHON_MINOR + " (mlx-lm 需 >= 3.10)", python, version };
  }
  const importProbe = spawnSync(python, ["-c", "import mlx_lm"], { encoding: "utf8" });
  if (importProbe.status !== 0) {
    return { available: false, reason: "mlx_lm not importable (pip install 'mlx-lm[train]')", python, version };
  }
  return { available: true, reason: "ok", python, version };
}
