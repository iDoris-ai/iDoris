import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AdapterGateError, adapterManifestSchema } from "@idoris/contracts";
import { describe, expect, it } from "vitest";
import {
  AdapterMountTable,
  MountError,
  TrainError,
  mlxLoraConfigYaml,
  trainLora,
  type CommandRunner,
  type TrainLoraOptions,
} from "../src/index.js";

/** 造一个像样的本地 MLX 模型目录（tokenizer + 权重）。 */
const makeModelDir = (): string => {
  const dir = mkdtempSync(join(tmpdir(), "idoris-model-"));
  writeFileSync(join(dir, "config.json"), "{}", "utf8");
  writeFileSync(join(dir, "tokenizer.json"), "{\"v\":1}", "utf8");
  writeFileSync(join(dir, "model.safetensors"), "weights-v1", "utf8");
  return dir;
};

const makeDataDir = (): string => {
  const dir = mkdtempSync(join(tmpdir(), "idoris-data-"));
  writeFileSync(join(dir, "train.jsonl"), "{}", "utf8");
  return dir;
};

const okRunner = (outputDir: string): CommandRunner => () => {
  writeFileSync(join(outputDir, "adapters.safetensors"), "adapter-weights", "utf8");
  return { status: 0, stdout: "trained", stderr: "" };
};

const baseOpts = (over: Partial<TrainLoraOptions> = {}): TrainLoraOptions => {
  const outputDir = over.outputDir ?? mkdtempSync(join(tmpdir(), "idoris-adapters-"));
  const opts: TrainLoraOptions = {
    modelDir: over.modelDir ?? makeModelDir(),
    dataDir: over.dataDir ?? makeDataDir(),
    outputDir,
    adapterId: "lora-1",
    dataClass: "synthetic",
    rank: 16,
    runner: over.runner ?? okRunner(outputDir),
    now: 1_700_000_000_000,
  };
  return { ...opts, ...over, outputDir };
};

describe("T3.1.2 训练编排", () => {
  it("rank 走 YAML config（mlx_lm.lora 没有 --lora-rank），并写出合法 manifest", () => {
    const opts = baseOpts();
    const trained = trainLora(opts);
    const yaml = readFileSync(trained.configPath, "utf8");
    expect(yaml).toContain("rank: 16");
    expect(yaml).toContain("adapter_path:");
    expect(trained.manifest.rank).toBe(16);
    expect(adapterManifestSchema.safeParse(trained.manifest).success).toBe(true);
    expect(trained.manifest.data_class).toBe("synthetic");
    expect(readFileSync(join(opts.outputDir, "adapter-manifest.json"), "utf8")).toContain("adapter_id");
  });

  it("只调用 --config，不传不存在的 rank flag", () => {
    const outputDir = mkdtempSync(join(tmpdir(), "idoris-adapters-"));
    const seen: string[][] = [];
    const opts = baseOpts({
      outputDir,
      runner: (_cmd, args) => {
        seen.push([...args]);
        writeFileSync(join(outputDir, "adapters.safetensors"), "w", "utf8");
        return { status: 0, stdout: "ok", stderr: "" };
      },
    });
    trainLora(opts);
    expect(seen[0]?.[0]).toBe("-m");
    expect(seen[0]?.[1]).toBe("mlx_lm.lora");
    expect(seen[0]?.includes("--config")).toBe(true);
    expect(seen[0]?.some((a) => a.includes("rank"))).toBe(false);
  });

  it("训练失败 → TRAIN_FAILED", () => {
    expect(() => trainLora(baseOpts({ runner: () => ({ status: 1, stdout: "", stderr: "boom" }) }))).toThrow(TrainError);
  });

  it("声称成功但没有 .safetensors → NO_ADAPTER_OUTPUT", () => {
    const opts = baseOpts({ runner: () => ({ status: 0, stdout: "ok", stderr: "" }) });
    try {
      trainLora(opts);
      throw new Error("should have thrown");
    } catch (err) {
      expect((err as TrainError).code).toBe("NO_ADAPTER_OUTPUT");
    }
  });

  it("模型目录没有 tokenizer 文件 → 拒绝（不发明指纹）", () => {
    const bare = mkdtempSync(join(tmpdir(), "idoris-bare-"));
    try {
      trainLora(baseOpts({ modelDir: bare }));
      throw new Error("should have thrown");
    } catch (err) {
      expect((err as TrainError).code).toBe("INVALID_MANIFEST");
    }
  });

  it("模型目录没有权重文件 → NO_WEIGHTS", () => {
    const dir = mkdtempSync(join(tmpdir(), "idoris-nw-"));
    writeFileSync(join(dir, "tokenizer.json"), "{}", "utf8");
    try {
      trainLora(baseOpts({ modelDir: dir }));
      throw new Error("should have thrown");
    } catch (err) {
      expect((err as TrainError).code).toBe("NO_WEIGHTS");
    }
  });

  it("权重变了 → base_digest 变（指纹真的在跟踪内容）", () => {
    const dir = makeModelDir();
    const a = trainLora(baseOpts({ modelDir: dir })).manifest.base_digest;
    writeFileSync(join(dir, "model.safetensors"), "weights-v2", "utf8");
    const b = trainLora(baseOpts({ modelDir: dir })).manifest.base_digest;
    expect(a).not.toBe(b);
  });

  it("mlxLoraConfigYaml 对含空格/引号的路径做转义", () => {
    const yaml = mlxLoraConfigYaml({
      modelDir: "/tmp/a b/\"q\"", dataDir: "/tmp/d", outputDir: "/tmp/o",
      adapterId: "x", dataClass: "synthetic", rank: 16,
    });
    expect(yaml).toContain("rank: 16");
    expect(yaml.split("\n")[0]).toBe("model: " + JSON.stringify("/tmp/a b/\"q\""));
  });
});

const base = (m: { base_model_id: string; base_digest: string; tokenizer_digest: string }) => ({
  base_model_id: m.base_model_id, base_digest: m.base_digest, tokenizer_digest: m.tokenizer_digest,
});

describe("T3.1.2 热挂载表", () => {
  const trained = () => trainLora(baseOpts());

  it("挂载 → has/list；卸载 → 没了", () => {
    const t = new AdapterMountTable();
    const { manifest } = trained();
    t.mount(manifest, base(manifest));
    expect(t.has(manifest.adapter_id)).toBe(true);
    expect(t.list().map((m) => m.adapter_id)).toEqual([manifest.adapter_id]);
    expect(t.unmount(manifest.adapter_id)).toBe(true);
    expect(t.has(manifest.adapter_id)).toBe(false);
  });

  it("同名底座但指纹变了 → 挂载被 T3.2.1 门禁拒绝", () => {
    const { manifest } = trained();
    const t = new AdapterMountTable();
    expect(() => t.mount(manifest, { ...base(manifest), base_digest: "sha256:" + "c".repeat(64) })).toThrow(AdapterGateError);
  });

  it("重复挂载 → ADAPTER_ALREADY_MOUNTED", () => {
    const { manifest } = trained();
    const t = new AdapterMountTable();
    t.mount(manifest, base(manifest));
    try {
      t.mount(manifest, base(manifest));
      throw new Error("should have thrown");
    } catch (err) {
      expect(err).toBeInstanceOf(MountError);
      expect((err as MountError).code).toBe("ADAPTER_ALREADY_MOUNTED");
    }
  });

  it("热挂载后可推理", async () => {
    const { manifest } = trained();
    const t = new AdapterMountTable();
    t.mount(manifest, base(manifest));
    const res = await t.infer(manifest.adapter_id, base(manifest), { model: "m", messages: [] }, async (req) => ({
      model: "adapter:" + req.model, content: "ok",
    }));
    expect(res.model).toBe("adapter:m");
  });

  it("未挂载就推理 → ADAPTER_NOT_MOUNTED", async () => {
    const { manifest } = trained();
    const t = new AdapterMountTable();
    await expect(t.infer(manifest.adapter_id, base(manifest), { model: "m", messages: [] }, async () => ({ model: "m", content: "x" }))).rejects.toThrow(MountError);
  });

  it("挂载后底座被换掉 → 推理时再校验一次并拒绝", async () => {
    const { manifest } = trained();
    const t = new AdapterMountTable();
    t.mount(manifest, base(manifest));
    await expect(
      t.infer(manifest.adapter_id, { ...base(manifest), base_digest: "sha256:" + "d".repeat(64) }, { model: "m", messages: [] }, async () => ({ model: "m", content: "x" })),
    ).rejects.toThrow(AdapterGateError);
  });
});
