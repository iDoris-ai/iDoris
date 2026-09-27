import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { AdapterMountTable, detectMlx, trainLora } from "../../src/index.js";

const probe = detectMlx();
const modelDir = process.env.IDORIS_MLX_MODEL_DIR;
const canRun = probe.available && modelDir !== undefined;
const reason = !probe.available
  ? probe.reason
  : modelDir === undefined
    ? "IDORIS_MLX_MODEL_DIR not set（真训需要一份本地 MLX 权重）"
    : "";
if (!canRun) console.log("SKIPPED: T3.1.2 MLX-LoRA 集成 —— " + reason);

describe.skipIf(!canRun)("T3.1.2 MLX-LoRA 真实训练 + 热挂载", () => {
  it("训练出 rank=16 的 adapter 并经热挂载表可推理", async () => {
    const root = mkdtempSync(join(tmpdir(), "idoris-lora-"));
    const dataDir = join(root, "data");
    mkdirSync(dataDir, { recursive: true });
    writeFileSync(
      join(dataDir, "train.jsonl"),
      JSON.stringify({ messages: [{ role: "user", content: "1+1=?" }, { role: "assistant", content: "2" }] }) + "\n",
      "utf8",
    );
    const trained = trainLora({
      modelDir: modelDir as string,
      dataDir,
      outputDir: join(root, "adapters"),
      adapterId: "lora-int-1",
      dataClass: "synthetic",
      rank: 16,
      iters: 20,
    });
    expect(trained.manifest.rank).toBe(16);
    const identity = {
      base_model_id: trained.manifest.base_model_id,
      base_digest: trained.manifest.base_digest,
      tokenizer_digest: trained.manifest.tokenizer_digest,
    };
    const table = new AdapterMountTable();
    table.mount(trained.manifest, identity);
    const res = await table.infer(trained.manifest.adapter_id, identity, { model: "m", messages: [] }, async () => ({
      model: "m", content: "IDORIS_LORA_MOUNT_OK",
    }));
    expect(res.content).toBe("IDORIS_LORA_MOUNT_OK");
  }, 600_000);
});
