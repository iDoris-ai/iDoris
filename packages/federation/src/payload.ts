import type { BaseIdentity } from "@idoris/contracts";
import type { DataClass } from "@idoris/growth";

/** LoRA 权重张量：名字 → 扁平权重。这是**唯一**允许跨机传输的东西。 */
export type Tensors = Record<string, number[]>;

/** 一个客户端的一轮联邦更新。 */
export interface AdapterUpdate {
  client_id: string;
  base: BaseIdentity;
  rank: number;
  data_class: DataClass;
  /** 参与本轮的样本数（FedAvg 的权重）。 */
  sample_count: number;
  tensors: Tensors;
}

export type FederationErrorCode =
  | "RAW_SAMPLE_IN_PAYLOAD"
  | "REAL_DATA_NOT_ADMISSIBLE"
  | "EMPTY_TENSORS"
  | "BASE_MISMATCH"
  | "NO_CLIENTS";

export class FederationError extends Error {
  constructor(
    readonly code: FederationErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "FederationError";
  }
}

/** 只可能出现在「原始样本」里的字段名 —— 跨机载荷的黑名单。 */
export const RAW_SAMPLE_FIELDS = [
  "samples", "messages", "prompt", "prompts", "response", "input", "body", "content",
  "text", "document", "dataset", "train", "tokens",
] as const;

/** 载荷必须**只**含 adapter 权重：出现任何原始样本痕迹即拒绝。 */
export function assertWeightsOnly(update: AdapterUpdate): void {
  const keys = Object.keys(update);
  for (const field of RAW_SAMPLE_FIELDS) {
    if (keys.includes(field)) {
      throw new FederationError("RAW_SAMPLE_IN_PAYLOAD", "adapter update carries a raw-sample field: " + field);
    }
  }
  const names = Object.keys(update.tensors);
  if (names.length === 0) {
    throw new FederationError("EMPTY_TENSORS", "adapter update " + update.client_id + " has no tensors");
  }
  for (const name of names) {
    const lower = name.toLowerCase();
    for (const field of RAW_SAMPLE_FIELDS) {
      if (lower.includes(field)) {
        throw new FederationError("RAW_SAMPLE_IN_PAYLOAD", "tensor name looks like raw sample content: " + name);
      }
    }
  }
}

/**
 * 真实数据准入门禁（T3.4.2）：隐私层未启用时，标记为 real 的客户端**报错**，
 * 不是跳过 —— 跳过会让人以为它没参与，而实际上它已经把梯度算出来了。
 */
export function assertFederatedAdmissible(update: AdapterUpdate, privacyLayerEnabled: boolean): void {
  if (update.data_class === "real" && !privacyLayerEnabled) {
    throw new FederationError(
      "REAL_DATA_NOT_ADMISSIBLE",
      "client " + update.client_id + " is marked data_class=real; refusing to enter the federation pipeline before the F3.4 privacy layer is enabled",
    );
  }
}
