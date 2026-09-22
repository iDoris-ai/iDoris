import type { BaseIdentity } from "@idoris/contracts";
import { fedAvg, type AggregationResult } from "./aggregate.js";
import { FederationError, assertFederatedAdmissible, assertWeightsOnly, type AdapterUpdate } from "./payload.js";
import { hashString } from "./prng.js";

/** 本地样本：**永远不出本机**，只在 LocalClient.train() 里被消费。 */
export interface LocalSample {
  prompt: string;
  response: string;
}

export interface LocalClientOptions {
  clientId: string;
  base: BaseIdentity;
  rank?: number;
  dataClass?: "synthetic" | "anonymized" | "real";
  /** 权重维度（模拟，不依赖真模型）。 */
  dim?: number;
}

/**
 * 一个本地联邦客户端（T3.3.1）。
 *
 * train() 把本地样本压成固定维度的 pseudo-LoRA 权重：
 * 样本内容**影响数值**，但数值里不含样本原文 —— 这是「只传权重」能被验证的前提。
 */
export class LocalClient {
  readonly clientId: string;
  readonly base: BaseIdentity;
  readonly rank: number;
  private readonly dataClass: "synthetic" | "anonymized" | "real";
  private readonly dim: number;
  private readonly samples: LocalSample[] = [];

  constructor(opts: LocalClientOptions) {
    this.clientId = opts.clientId;
    this.base = opts.base;
    this.rank = opts.rank ?? 16;
    this.dataClass = opts.dataClass ?? "synthetic";
    this.dim = opts.dim ?? 4;
  }

  addSamples(samples: readonly LocalSample[]): void {
    this.samples.push(...samples);
  }

  sampleCount(): number {
    return this.samples.length;
  }

  train(): AdapterUpdate {
    if (this.samples.length === 0) throw new FederationError("NO_CLIENTS", "client " + this.clientId + " has no local samples");
    const acc = new Array<number>(this.dim).fill(0);
    for (const s of this.samples) {
      const h = hashString(s.prompt + "\u0000" + s.response);
      for (let i = 0; i < this.dim; i += 1) {
        const byte = (h >>> ((i % 4) * 8)) & 0xff;
        acc[i] = (acc[i] ?? 0) + byte / 255 / this.samples.length;
      }
    }
    return {
      client_id: this.clientId,
      base: { ...this.base },
      rank: this.rank,
      data_class: this.dataClass,
      sample_count: this.samples.length,
      tensors: { "layers.0.q_proj.lora_a": acc },
    };
  }
}

export interface RoundResult {
  transmitted: AdapterUpdate[];
  aggregate: AggregationResult;
}

/**
 * 跑一轮联邦：每个客户端本地训练 → 门禁（真实数据准入 + 只传权重）→ transport → FedAvg。
 * transport 是**唯一**的跨机路径，测试把传出去的东西全抓下来检查。
 */
export function runRound(
  clients: readonly LocalClient[],
  transport: (update: AdapterUpdate) => void,
  privacyLayerEnabled = false,
): RoundResult {
  const first = clients[0];
  if (first === undefined) throw new FederationError("NO_CLIENTS", "a round needs at least one client");
  const transmitted: AdapterUpdate[] = [];
  for (const client of clients) {
    const update = client.train();
    assertFederatedAdmissible(update, privacyLayerEnabled);
    assertWeightsOnly(update);
    transport(update);
    transmitted.push(update);
  }
  return { transmitted, aggregate: fedAvg(transmitted, first.base) };
}
