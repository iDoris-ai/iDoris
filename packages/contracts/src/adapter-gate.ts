import type { AdapterManifest } from "./adapter-manifest.js";
import { adapterManifestSchema } from "./adapter-manifest.js";

const DIGEST = /^sha256:[a-f0-9]{64}$/;

export type AdapterGateErrorCode =
  | "INVALID_MANIFEST"
  | "MISSING_BASE_IDENTITY"
  | "BASE_MODEL_MISMATCH"
  | "BASE_DIGEST_MISMATCH"
  | "TOKENIZER_DIGEST_MISMATCH"
  | "AGGREGATION_BASE_MISMATCH";

export class AdapterGateError extends Error {
  constructor(
    readonly code: AdapterGateErrorCode,
    readonly adapterId: string | undefined,
    message: string,
  ) {
    super(message);
    this.name = "AdapterGateError";
  }
}

/**
 * 当前真正在服役的 base 身份。**指纹是唯一权威，模型名不是**：同名 base 被静默升级
 * 之后权重换了、名字没换，只有 base_digest 会变 —— 此时老 adapter 必须被拒绝。
 */
export interface BaseIdentity {
  base_model_id: string;
  base_digest: string;
  tokenizer_digest: string;
}

/** 结构校验（T3.2.1）。指纹格式也由 schema 强制。 */
export function validateAdapterManifest(input: unknown): AdapterManifest {
  const parsed = adapterManifestSchema.safeParse(input);
  if (!parsed.success) {
    throw new AdapterGateError(
      "INVALID_MANIFEST",
      undefined,
      "adapter manifest schema validation failed: " +
        parsed.error.issues.map((i) => i.path.join(".") + ": " + i.message).join("; "),
    );
  }
  return parsed.data;
}

/** 目标身份必须有真指纹 —— 缺指纹一律拒绝，绝不因为「没有可比的指纹」就放行。 */
function requiredText(value: unknown, field: string, adapterId: string | undefined): string {
  if (typeof value !== "string" || value === "") {
    throw new AdapterGateError("MISSING_BASE_IDENTITY", adapterId, "base identity must declare a non-empty " + field);
  }
  return value;
}

function requiredDigest(value: unknown, field: string, adapterId: string | undefined): string {
  if (typeof value !== "string" || !DIGEST.test(value)) {
    throw new AdapterGateError(
      "MISSING_BASE_IDENTITY",
      adapterId,
      "base identity must declare a real " + field + " (sha256:<64 hex>); a missing fingerprint is refused, never skipped",
    );
  }
  return value;
}

/**
 * 挂载门禁（T3.2.1）：manifest 的 base/tokenizer 指纹必须与**当前加载**的 base 逐字段相同。
 *
 * 静默升级陷阱：模型名相同但 base_digest 不同时也必须拒绝，所以这里比的是指纹而不是名字。
 * 任何一项无法比对（目标缺指纹）同样拒绝 —— 「静默升级毁掉整批 LoRA」正是从
 * 「没有指纹就跳过校验」进入的。
 */
export function assertAdapterMountable(input: unknown, target: BaseIdentity): AdapterManifest {
  const m = validateAdapterManifest(input);
  const targetId = requiredText(target.base_model_id, "base_model_id", m.adapter_id);
  const targetBase = requiredDigest(target.base_digest, "base_digest", m.adapter_id);
  const targetTokenizer = requiredDigest(target.tokenizer_digest, "tokenizer_digest", m.adapter_id);
  if (m.base_model_id !== targetId) {
    throw new AdapterGateError(
      "BASE_MODEL_MISMATCH",
      m.adapter_id,
      "adapter " + m.adapter_id + " targets base " + m.base_model_id + " but the loaded base is " + targetId,
    );
  }
  if (m.base_digest !== targetBase) {
    throw new AdapterGateError(
      "BASE_DIGEST_MISMATCH",
      m.adapter_id,
      "adapter " + m.adapter_id + " was trained on base_digest " + m.base_digest + " but the loaded base is " + targetBase + " (same name is not the same weights)",
    );
  }
  if (m.tokenizer_digest !== targetTokenizer) {
    throw new AdapterGateError(
      "TOKENIZER_DIGEST_MISMATCH",
      m.adapter_id,
      "adapter " + m.adapter_id + " was trained with tokenizer_digest " + m.tokenizer_digest + " but the loaded tokenizer is " + targetTokenizer,
    );
  }
  return m;
}

/**
 * 聚合门禁（T3.2.1）：FedAvg 只能平均**同 base、同 tokenizer** 的 adapter。
 *
 * 任何一条不一致即**整体拒绝**，不做「跳过坏的那条」—— 静默跳过会让聚合结果看起来
 * 正常，而参与平均的权重已经不是同一个底座了。
 */
export function assertAdaptersAggregatable(inputs: readonly unknown[], target: BaseIdentity): AdapterManifest[] {
  const targetId = requiredText(target.base_model_id, "base_model_id", undefined);
  const targetBase = requiredDigest(target.base_digest, "base_digest", undefined);
  const targetTokenizer = requiredDigest(target.tokenizer_digest, "tokenizer_digest", undefined);
  const out: AdapterManifest[] = [];
  for (const input of inputs) {
    const m = validateAdapterManifest(input);
    if (m.base_model_id !== targetId || m.base_digest !== targetBase || m.tokenizer_digest !== targetTokenizer) {
      throw new AdapterGateError(
        "AGGREGATION_BASE_MISMATCH",
        m.adapter_id,
        "adapter " + m.adapter_id + " does not share the aggregation base: base_model_id/base_digest/tokenizer_digest must all match exactly",
      );
    }
    out.push(m);
  }
  return out;
}
