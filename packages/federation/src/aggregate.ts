import { assertAdapterMountable } from "@idoris/contracts";
import { FederationError, assertWeightsOnly, type AdapterUpdate, type Tensors } from "./payload.js";

export interface AggregationResult {
  tensors: Tensors;
  totalSamples: number;
  clients: string[];
}

/**
 * FedAvg（T3.3.1）：按样本数加权平均。
 *
 * 每条更新先过 T3.2.1 的 base 一致性门禁（复用同一套指纹规则，而不是在这里再写一遍），
 * 再要求张量形状一致 —— 否则平均出来的东西没有任何意义。
 */
export function fedAvg(updates: readonly AdapterUpdate[], target: AdapterUpdate["base"]): AggregationResult {
  if (updates.length === 0) throw new FederationError("NO_CLIENTS", "no updates to aggregate");
  const tensors: Record<string, number[]> = {};
  let total = 0;
  const clients: string[] = [];
  for (const u of updates) {
    assertWeightsOnly(u);
    assertAdapterMountable(
      {
        adapter_id: u.client_id,
        base_model_id: u.base.base_model_id,
        base_digest: u.base.base_digest,
        tokenizer_digest: u.base.tokenizer_digest,
        rank: u.rank,
        data_class: u.data_class,
      },
      target,
    );
    total += u.sample_count;
    clients.push(u.client_id);
    for (const [name, values] of Object.entries(u.tensors)) {
      const acc = tensors[name] ?? new Array<number>(values.length).fill(0);
      if (acc.length !== values.length) {
        throw new FederationError("BASE_MISMATCH", "tensor " + name + " has inconsistent length across clients");
      }
      for (let i = 0; i < values.length; i += 1) acc[i] = (acc[i] ?? 0) + (values[i] ?? 0) * u.sample_count;
      tensors[name] = acc;
    }
  }
  for (const acc of Object.values(tensors)) {
    for (let i = 0; i < acc.length; i += 1) acc[i] = (acc[i] ?? 0) / total;
  }
  return { tensors, totalSamples: total, clients };
}
