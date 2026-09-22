import type {
  Admission,
  BackendStatus,
  ChatRequest,
  ChatResponse,
  ModelBackend,
  ModelInfo,
} from "../src/backend.js";

export interface FetchResponseLike {
  status: number;
  ok: boolean;
  text(): Promise<string>;
  json(): Promise<unknown>;
}
export type FetchLike = (
  url: string,
  init: { method?: string; headers?: Record<string, string>; body?: string; signal?: AbortSignal },
) => Promise<FetchResponseLike>;

export interface OpenAiCompatOptions {
  baseUrl: string;
  apiKey?: string;
  fetchImpl?: FetchLike;
}

/**
 * 通用 OpenAI-compat 上游槽位（T2.5.1）。只证明外部槽位可接，不做选型。
 * 外部 API 没有显式 load/unload：load/unload 为 no-op；status 反映远端可用。
 */
export class OpenAiCompatBackend implements ModelBackend {
  private readonly baseUrl: string;
  private readonly apiKey: string | undefined;
  private readonly fetchImpl: FetchLike;

  constructor(opts: OpenAiCompatOptions) {
    this.baseUrl = opts.baseUrl.replace(/\/$/, "");
    this.apiKey = opts.apiKey;
    this.fetchImpl = opts.fetchImpl ?? ((globalThis as unknown as { fetch: FetchLike }).fetch);
  }

  async list(): Promise<ModelInfo[]> {
    const body = (await this.json("GET", "/models")) as { data?: Array<{ id?: string }> };
    return (body.data ?? [])
      .filter((m): m is { id: string } => typeof m.id === "string")
      .map((m) => ({ id: m.id, memoryGb: 0 }));
  }

  async load(): Promise<void> {
    /* 外部 API 无常驻概念 */
  }

  async unload(): Promise<void> {
    /* no-op */
  }

  async admission(): Promise<Admission> {
    return "coexist";
  }

  async status(): Promise<BackendStatus> {
    return { pressure: "ok", usedGb: 0, modelMemoryMaxGb: 0, loaded: [] };
  }

  async chat(req: ChatRequest): Promise<ChatResponse> {
    const body = (await this.json("POST", "/chat/completions", { model: req.model, messages: req.messages })) as {
      choices?: Array<{ message?: { content?: string } }>;
    };
    return { model: req.model, content: body.choices?.[0]?.message?.content ?? "" };
  }

  private async json(method: string, path: string, payload?: unknown): Promise<unknown> {
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (this.apiKey !== undefined) headers.authorization = "Bearer " + this.apiKey;
    const init: { method: string; headers: Record<string, string>; body?: string } = { method, headers };
    if (payload !== undefined) init.body = JSON.stringify(payload);
    const res = await this.fetchImpl(this.baseUrl + "/v1" + path, init);
    if (!res.ok) throw new Error("openai-compat upstream " + method + " " + path + " failed: HTTP " + res.status);
    return res.json();
  }
}
