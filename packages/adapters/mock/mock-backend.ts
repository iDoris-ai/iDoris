import type { LoadPolicy } from "@idoris/contracts";
import type {
  Admission,
  BackendStatus,
  ChatRequest,
  ChatResponse,
  ModelBackend,
  ModelInfo,
  Pressure,
} from "../src/backend.js";

interface Loaded {
  policy: LoadPolicy;
  memoryGb: number;
  lastUsed: number;
}

export interface MockBackendOptions {
  memoryMaxGb: number;
  models: ModelInfo[];
  /** 软/硬压力阈值，比例（默认 0.85 / 0.95）。 */
  softThreshold?: number;
  hardThreshold?: number;
}

const DEFAULT_POLICY: LoadPolicy = {
  mode: "on_demand",
  keepalive: { idle_ttl_s: 300 },
  admission: "requires_eviction",
};

/**
 * 内存态 mock 后端：模拟 ok/soft/hard/ceiling 压力分级与 LRU 驱逐。
 * - `resident`（pinned）**在 ceiling 压力下也不被驱逐**；
 * - 其余按 LRU 驱逐，直到回到上限内；
 * - 只剩 pinned 模型时不再驱逐（used 可暂时超过上限）。
 */
export class MockBackend implements ModelBackend {
  private readonly models: Map<string, ModelInfo>;
  private readonly loaded = new Map<string, Loaded>();
  private readonly memoryMaxGb: number;
  private readonly softThreshold: number;
  private readonly hardThreshold: number;
  private clock = 0;

  constructor(opts: MockBackendOptions) {
    this.models = new Map(opts.models.map((m) => [m.id, m]));
    this.memoryMaxGb = opts.memoryMaxGb;
    this.softThreshold = opts.softThreshold ?? 0.85;
    this.hardThreshold = opts.hardThreshold ?? 0.95;
  }

  async list(): Promise<ModelInfo[]> {
    return [...this.models.values()];
  }

  async load(id: string, policy: LoadPolicy = DEFAULT_POLICY): Promise<void> {
    const model = this.models.get(id);
    if (!model) throw new Error("unknown model: " + id);
    this.clock += 1;
    this.loaded.set(id, { policy, memoryGb: model.memoryGb, lastUsed: this.clock });
    this.enforce();
  }

  async unload(id: string): Promise<void> {
    this.loaded.delete(id);
  }

  async admission(id: string): Promise<Admission> {
    if (this.loaded.has(id)) return "coexist";
    const model = this.models.get(id);
    if (!model) throw new Error("unknown model: " + id);
    return this.usedGb() + model.memoryGb <= this.memoryMaxGb ? "coexist" : "requires_eviction";
  }

  async status(): Promise<BackendStatus> {
    return {
      pressure: this.pressure(),
      usedGb: this.usedGb(),
      modelMemoryMaxGb: this.memoryMaxGb,
      loaded: [...this.loaded.keys()],
    };
  }

  async chat(req: ChatRequest): Promise<ChatResponse> {
    const entry = this.loaded.get(req.model);
    if (entry) {
      this.clock += 1;
      entry.lastUsed = this.clock;
    } else {
      await this.load(req.model);
    }
    return { model: req.model, content: "mock:" + req.model };
  }

  /** 压力驱逐，返回被驱逐的 id。 */
  private enforce(): string[] {
    const evicted: string[] = [];
    while (this.usedGb() > this.memoryMaxGb) {
      const victim = [...this.loaded.entries()]
        .filter(([, e]) => e.policy.mode !== "resident")
        .sort((a, b) => a[1].lastUsed - b[1].lastUsed)[0];
      if (!victim) break; // 只剩 pinned
      this.loaded.delete(victim[0]);
      evicted.push(victim[0]);
    }
    return evicted;
  }

  private usedGb(): number {
    let sum = 0;
    for (const e of this.loaded.values()) sum += e.memoryGb;
    return sum;
  }

  private pressure(): Pressure {
    const ratio = this.memoryMaxGb === 0 ? 1 : this.usedGb() / this.memoryMaxGb;
    if (ratio >= 1) return "ceiling";
    if (ratio >= this.hardThreshold) return "hard";
    if (ratio >= this.softThreshold) return "soft";
    return "ok";
  }
}
