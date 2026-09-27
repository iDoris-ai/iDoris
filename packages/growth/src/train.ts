import { spawnSync } from "node:child_process";
import { mkdirSync, readdirSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";
import { adapterManifestSchema, type AdapterManifest } from "@idoris/contracts";
import type { DataClass } from "./datalake.js";
import { digestDirectory, digestFiles, tokenizerFilesIn, weightFilesIn } from "./digest.js";

export interface CommandResult {
  status: number | null;
  stdout: string;
  stderr: string;
}
export type CommandRunner = (cmd: string, args: readonly string[]) => CommandResult;

export const spawnRunner: CommandRunner = (cmd, args) => {
  const r = spawnSync(cmd, [...args], { encoding: "utf8" });
  return { status: r.status, stdout: r.stdout ?? "", stderr: r.stderr ?? "" };
};

export type TrainErrorCode = "TRAIN_FAILED" | "NO_ADAPTER_OUTPUT" | "INVALID_MANIFEST" | "NO_WEIGHTS";

export class TrainError extends Error {
  constructor(
    readonly code: TrainErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "TrainError";
  }
}

export interface TrainLoraOptions {
  python?: string;
  /** 本地 MLX 权重目录。 */
  modelDir: string;
  /** 训练数据目录，需含 train.jsonl（valid.jsonl 可选）。 */
  dataDir: string;
  /** adapter 权重输出目录（mlx-lm 的 adapter_path）。 */
  outputDir: string;
  adapterId: string;
  dataClass: DataClass;
  rank?: number;
  iters?: number;
  seed?: number;
  runner?: CommandRunner;
  now?: number;
}

/**
 * 生成 mlx-lm 的 YAML 配置。**rank 只能走 config**：`mlx_lm.lora` 没有
 * `--lora-rank` 这个 flag（CLI 只有 --num-layers / --iters / ...），
 * LoRA rank 属 `lora_parameters`。字符串一律用 JSON.stringify 转义（JSON 字符串是合法 YAML）。
 */
export function mlxLoraConfigYaml(opts: TrainLoraOptions): string {
  const lines = [
    "model: " + JSON.stringify(opts.modelDir),
    "train: true",
    "data: " + JSON.stringify(opts.dataDir),
    "adapter_path: " + JSON.stringify(opts.outputDir),
    "iters: " + String(opts.iters ?? 20),
    "seed: " + String(opts.seed ?? 0),
    "lora_parameters:",
    "  rank: " + String(opts.rank ?? 16),
    "  scale: 20.0",
    "  dropout: 0.0",
    "",
  ];
  return lines.join("\n");
}

export interface TrainedAdapter {
  manifest: AdapterManifest;
  adapterDir: string;
  configPath: string;
}

/**
 * MLX-LoRA 本地训练（T3.1.2）。
 *
 * 训练前先算 base/tokenizer 指纹并写进 manifest —— 没有指纹就不训练，
 * 免得产出一批「不知道挂在哪个底座上」的 adapter（那正是 T3.2.1 要防的）。
 * runner 可注入，便于在没有 MLX 的环境里验证编排逻辑。
 */
export function trainLora(opts: TrainLoraOptions): TrainedAdapter {
  const run = opts.runner ?? spawnRunner;
  const python = opts.python ?? process.env.IDORIS_PYTHON ?? "python3";

  const tokenizerFiles = tokenizerFilesIn(opts.modelDir);
  if (tokenizerFiles.length === 0) {
    throw new TrainError("INVALID_MANIFEST", "no tokenizer files in " + opts.modelDir + "; refusing to invent a fingerprint");
  }
  const weights = weightFilesIn(opts.modelDir);
  if (weights.length === 0) {
    throw new TrainError("NO_WEIGHTS", "no .safetensors/.npz weights in " + opts.modelDir);
  }
  const rank = opts.rank ?? 16;
  mkdirSync(opts.outputDir, { recursive: true });
  const configPath = join(opts.outputDir, "lora-config.yaml");
  writeFileSync(configPath, mlxLoraConfigYaml(opts), "utf8");

  const res = run(python, ["-m", "mlx_lm.lora", "--config", configPath]);
  if (res.status !== 0) {
    throw new TrainError("TRAIN_FAILED", "mlx_lm.lora exited " + String(res.status) + ": " + res.stderr.slice(-400));
  }
  const produced = readdirSync(opts.outputDir);
  if (!produced.some((f) => f.endsWith(".safetensors"))) {
    throw new TrainError("NO_ADAPTER_OUTPUT", "training reported success but no .safetensors in " + opts.outputDir);
  }

  const manifest = {
    adapter_id: opts.adapterId,
    base_model_id: basename(opts.modelDir),
    base_digest: digestDirectory(opts.modelDir),
    tokenizer_digest: digestFiles(tokenizerFiles),
    rank,
    data_class: opts.dataClass,
    created_at: new Date(opts.now ?? Date.now()).toISOString(),
  };
  const parsed = adapterManifestSchema.safeParse(manifest);
  if (!parsed.success) {
    throw new TrainError(
      "INVALID_MANIFEST",
      "produced manifest is not schema-valid: " + parsed.error.issues.map((i) => i.path.join(".") + ": " + i.message).join("; "),
    );
  }
  writeFileSync(join(opts.outputDir, "adapter-manifest.json"), JSON.stringify(parsed.data, null, 2) + "\n", "utf8");
  return { manifest: parsed.data, adapterDir: opts.outputDir, configPath };
}
