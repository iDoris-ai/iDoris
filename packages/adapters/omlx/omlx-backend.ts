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

/** 只依赖最小的 fetch 形状，避免为库引入 DOM/@types/node 依赖。 */
export type FetchLike = (
  url: string,
  init?: { method?: string; headers?: Record<string, string>; body?: string },
) => Promise<{ ok: boolean; status: number; json(): Promise<unknown>; text(): Promise<string> }>;

export interface OmlxOptions {
  /** 默认 http://127.0.0.1:8088（本机自定义端口，见设计文档 D7）。 */
  baseUrl?: string;
  apiKey?: string;
  fetchImpl?: FetchLike;
}

const DEFAULT_URL = "http://127.0.0.1:8088";

/**
 * oMLX 适配器（T1.2.2）：把 LoadPolicy 抽象映射到 oMLX 实测端点（spike/u0/U0-LOG.md）。
 * - resident → model_settings.is_pinned=true；on_demand → unpinned
 * - 显式 POST /v1/models/{id}/load | /unload
 * - admission / status 读 GET /api/status（model_memory_max + loaded）
 *
 * ⚠️ 本适配器是本仓内唯一允许出现 "omlx" 字样的实现层；Router 核心不得引用它。
 */
export class OmlxBackend implements ModelBackend {
  private readonly baseUrl: string;
  private readonly apiKey: string | undefined;
  private readonly fetchImpl: FetchLike;

  constructor(opts: OmlxOptions = {}) {
    this.baseUrl = (opts.baseUrl ?? DEFAULT_URL).replace(/\/$/, "");
    this.apiKey = opts.apiKey;
    this.fetchImpl = opts.fetchImpl ?? ((globalThis as unknown as { fetch: FetchLike }).fetch);
  }

  async list(): Promise<ModelInfo[]> {
    const body = (await this.json("GET", "/v1/models")) as { data?: Array<{ id?: string }> };
    return (body.data ?? [])
      .filter((m): m is { id: string } => typeof m.id === "string")
      .map((m) => ({ id: m.id, memoryGb: 0 }));
  }

  async load(id: string, policy?: LoadPolicy): Promise<void> {
    await this.post(`/v1/models/${encodeURIComponent(id)}/load`);
    if (policy) {
      // resident = pinned（不驱逐）；on_demand/evict_to_load = unpinned
      await this.setPinned(id, policy.mode === "resident");
    }
  }

  async unload(id: string): Promise<void> {
    await this.post(`/v1/models/${encodeURIComponent(id)}/unload`);
  }

  async admission(id: string): Promise<Admission> {
    const st = await this.status();
    if (st.loaded.includes(id)) return "coexist";
    // 未知模型内存时保守判 requires_eviction（宁可先驱逐也不超 guard）
    return "requires_eviction";
  }

  async status(): Promise<BackendStatus> {
    const body = (await this.json("GET", "/api/status")) as Record<string, unknown>;
    const loaded = Array.isArray(body.loaded) ? body.loaded.filter((x): x is string => typeof x === "string") : [];
    const max = Number(body.model_memory_max ?? 0);
    const used = Number(body.model_memory_used ?? 0);
    const pressure = typeof body.pressure === "string" ? (body.pressure as Pressure) : "ok";
    return { pressure, usedGb: used, modelMemoryMaxGb: max, loaded };
  }

  async chat(req: ChatRequest): Promise<ChatResponse> {
    const body = (await this.json("POST", "/v1/chat/completions", { model: req.model, messages: req.messages })) as {
      choices?: Array<{ message?: { content?: string } }>;
    };
    const content = body.choices?.[0]?.message?.content ?? "";
    return { model: req.model, content };
  }

  private async setPinned(id: string, pinned: boolean): Promise<void> {
    await this.json("POST", "/admin/settings", { model_settings: { [id]: { is_pinned: pinned } } });
  }

  private headers(): Record<string, string> {
    const h: Record<string, string> = { "content-type": "application/json" };
    if (this.apiKey) h.authorization = `Bearer ${this.apiKey}`;
    return h;
  }

  private async post(path: string): Promise<void> {
    const res = await this.fetchImpl(this.baseUrl + path, { method: "POST", headers: this.headers() });
    if (!res.ok) throw new Error(`oMLX POST ${path} failed: HTTP ${res.status}`);
  }

  private async json(method: string, path: string, payload?: unknown): Promise<unknown> {
    const res = await this.fetchImpl(this.baseUrl + path, {
      method,
      headers: this.headers(),
      ...(payload === undefined ? {} : { body: JSON.stringify(payload) }),
    });
    if (!res.ok) throw new Error(`oMLX ${method} ${path} failed: HTTP ${res.status}`);
    return res.json();
  }
}
