/**
 * T2.2.1 — `GET /capabilities`：把容量变成接口（06 §10.8）。
 *
 * 条目来源：
 *  - **recommender**（硬件 facts + catalog 动态推荐）给出 resident / temp / blocked 三类模型；
 *  - **backend.status()** 给出 `queue_depth`（当前已加载模型数，作为队列深度的容量代理）。
 *
 * `admission_status` 收敛为 `ready | requires_eviction | blocked`（06 §10.8 枚举）；
 * recommender 内部用大写 `BLOCKED`，这里统一转小写。
 */
import { fileURLToPath } from "node:url";
import {
  inspectHost,
  loadCatalog,
  recommend,
  type Catalog,
  type CatalogModel,
  type HostFacts,
  type RecommenderPolicy,
} from "@idoris/recommender";
import type { Registered } from "./registry.js";

/** 06 §10.8 的三个取值；服务端一律输出小写。 */
export type AdmissionStatus = "ready" | "requires_eviction" | "blocked";

export interface CapabilityEntry {
  /** 提供该能力的模型 id（recommender 目录 id）。 */
  id: string;
  /** 能力名（TaskProfile 能力枚举之一）。 */
  capability: string;
  /** 是否常驻（pinned）。 */
  resident: boolean;
  /** 估算内存 GB（含权重 + KV + 运行开销）。 */
  estimated_memory_gb: number;
  /** 上下文上限（token）。 */
  ctx_limit: number;
  /** 队列深度代理：所有后端当前已加载模型数之和。 */
  queue_depth: number;
  admission_status: AdmissionStatus;
}

export interface CapabilitiesProvider {
  snapshot(): Promise<CapabilityEntry[]>;
}

export interface CapabilitiesOptions {
  registered: Registered[];
  /** catalog 路径；默认 repo 根 config/catalog.yaml。 */
  catalogPath?: string;
  /** 已解析目录（测试可注入，避免读盘）。 */
  catalog?: Catalog;
  /** 硬件 facts；默认 inspectHost()。 */
  hardware?: HostFacts;
  /** recommender 策略（context_target / needed_capabilities 等）。 */
  policy?: Partial<RecommenderPolicy>;
}

/** 默认目录位置：repo 根的 config/catalog.yaml（src 与 dist 同深度）。 */
export function defaultCatalogPath(): string {
  return fileURLToPath(new URL("../../../config/catalog.yaml", import.meta.url));
}

/** catalog capability 的枚举内候选，按降序优先级。 */
const CAPABILITY_PRIORITY = ["reasoning", "coding", "vision", "asr", "tts", "embedding", "rerank", "chat"] as const;

/** 取模型在能力枚举内得分最高的能力；无则回落 chat。 */
function topCapability(model: CatalogModel | undefined): string {
  const caps = model?.capability;
  if (caps === undefined) return "chat";
  let best = "";
  let bestScore = Number.NEGATIVE_INFINITY;
  for (const name of CAPABILITY_PRIORITY) {
    const score = caps[name];
    if (typeof score === "number" && Number.isFinite(score) && score > bestScore) {
      best = name;
      bestScore = score;
    }
  }
  return best === "" ? "chat" : best;
}

function roundGb(value: number): number {
  return Math.round(value * 100) / 100;
}

/**
 * 默认容量提供者：每次 snapshot 重算推荐并重新读后端状态（容量是动态的）。
 * 目录与硬件 facts 在构造时固定，避免每次请求读盘 / 探测。
 */
export class DefaultCapabilitiesProvider implements CapabilitiesProvider {
  private readonly registered: Registered[];
  private readonly catalog: Catalog;
  private readonly hardware: HostFacts;
  private readonly policy: Partial<RecommenderPolicy> | undefined;

  constructor(opts: CapabilitiesOptions) {
    this.registered = opts.registered;
    this.catalog = opts.catalog ?? loadCatalog(opts.catalogPath ?? defaultCatalogPath());
    this.hardware = opts.hardware ?? inspectHost();
    this.policy = opts.policy;
  }

  async snapshot(): Promise<CapabilityEntry[]> {
    const rec = recommend({
      hardware: this.hardware,
      catalog: this.catalog,
      ...(this.policy === undefined ? {} : { policy: this.policy }),
    });
    const queueDepth = await this.queueDepth();
    const models = new Map(this.catalog.catalog.map((m) => [m.id, m]));
    const entries: CapabilityEntry[] = [];

    if (rec.resident !== null) {
      entries.push({
        id: rec.resident.id,
        capability: "reasoning",
        resident: true,
        estimated_memory_gb: roundGb(rec.resident.footprint_gb),
        ctx_limit: rec.resident.ctx,
        queue_depth: queueDepth,
        admission_status: "ready",
      });
    }

    for (const t of rec.temp) {
      entries.push({
        id: t.id,
        capability: t.capability,
        resident: false,
        estimated_memory_gb: t.quant === null ? 0 : roundGb(t.quant.footprint_gb),
        ctx_limit: rec.policy.context_target,
        queue_depth: queueDepth,
        admission_status: t.status === "ready" ? "ready" : "requires_eviction",
      });
    }

    for (const b of rec.blocked) {
      entries.push({
        id: b.id,
        capability: topCapability(models.get(b.id)),
        resident: false,
        estimated_memory_gb: roundGb(b.estimated_memory_gb),
        ctx_limit: rec.policy.context_target,
        queue_depth: queueDepth,
        admission_status: "blocked",
      });
    }

    return entries;
  }

  /** backend.status() → 队列深度。单个后端不可达不阻塞整个容量接口。 */
  private async queueDepth(): Promise<number> {
    let depth = 0;
    for (const { backend } of this.registered) {
      try {
        const status = await backend.status();
        depth += status.loaded.length;
      } catch {
        // 忽略：不可达后端不贡献队列深度，容量接口仍可用。
      }
    }
    return depth;
  }
}
