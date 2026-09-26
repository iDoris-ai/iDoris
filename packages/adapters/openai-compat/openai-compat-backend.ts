import type { LoadPolicy } from "@idoris/contracts";
import type {
  Admission,
  BackendStatus,
  ChatRequest,
  ChatResponse,
  ModelBackend,
  ModelInfo,
} from "../src/backend.js";

/** 只声明真正被用到的成员，测试桩不必实现无关方法。 */
export interface FetchResponseLike {
  status: number;
  ok: boolean;
  json(): Promise<unknown>;
}
export type FetchLike = (
  url: string,
  init: { method?: string; headers?: Record<string, string>; body?: string; signal?: AbortSignal },
) => Promise<FetchResponseLike>;

export interface OpenAiCompatOptions {
  /** 上游 origin，例如 https://api.openai.com —— 不要带 /v1，本类自己拼 /v1/...。 */
  baseUrl: string;
  apiKey?: string;
  fetchImpl?: FetchLike;
}

/**
 * 通用 OpenAI-compat 上游槽位（T2.5.1）。只证明外部槽位可接，不做选型。
 *
 * - 外部 API 无常驻概念：load/unload 为 no-op，admission 恒为 coexist。
 * - status() 返回静态的「无本地压力」占位值，**不探测上游健康**；上游不可用时
 *   由 list()/chat() 抛错暴露（本仓状态机不依赖外部 provider 的 status）。
 * - list() 的 memoryGb 恒为 0：不占用本地内存，不参与驱逐。
 */
export class OpenAiCompatBackend implements ModelBackend {
  private readonly baseUrl: string;
  private readonly apiKey: string | undefined;
  private readonly fetchImpl: FetchLike;

  constructor(opts: OpenAiCompatOptions) {
    this.baseUrl = opts.baseUrl.replace(/\/$/, "");
    this.apiKey = opts.apiKey;
    const globalFetch = (globalThis as unknown as { fetch?: FetchLike }).fetch;
    const impl = opts.fetchImpl ?? globalFetch;
    if (impl === undefined) throw new Error("OpenAiCompatBackend: no fetch available; pass fetchImpl");
    this.fetchImpl = impl;
  }

  async list(): Promise<ModelInfo[]> {
    const body = (await this.json("GET", "/models")) as { data?: Array<{ id?: string }> };
    return (body.data ?? [])
      .filter((m): m is { id: string } => typeof m.id === "string")
      .map((m) => ({ id: m.id, memoryGb: 0 }));
  }

  async load(_id: string, _policy?: LoadPolicy): Promise<void> {
    /* 外部 API 无常驻概念 */
  }

  async unload(_id: string): Promise<void> {
    /* no-op */
  }

  async admission(_id: string): Promise<Admission> {
    return "coexist";
  }

  async status(): Promise<BackendStatus> {
    return { pressure: "ok", usedGb: 0, modelMemoryMaxGb: 0, loaded: [] };
  }

  async chat(req: ChatRequest): Promise<ChatResponse> {
    const body = (await this.json(
      "POST",
      "/chat/completions",
      { model: req.model, messages: req.messages },
      req.signal,
    )) as { choices?: Array<{ message?: { content?: string } }> };
    return { model: req.model, content: body.choices?.[0]?.message?.content ?? "" };
  }

  private async json(method: string, path: string, payload?: unknown, signal?: AbortSignal): Promise<unknown> {
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (this.apiKey !== undefined) headers.authorization = "Bearer " + this.apiKey;
    const init: { method: string; headers: Record<string, string>; body?: string; signal?: AbortSignal } = {
      method,
      headers,
    };
    if (payload !== undefined) init.body = JSON.stringify(payload);
    if (signal !== undefined) init.signal = signal;
    const res = await this.fetchImpl(this.baseUrl + "/v1" + path, init);
    if (!res.ok) throw new Error("openai-compat upstream " + method + " " + path + " failed: HTTP " + res.status);
    return res.json();
  }
}
