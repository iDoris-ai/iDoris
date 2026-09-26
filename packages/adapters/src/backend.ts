import type { LoadPolicy } from "@idoris/contracts";

/** 后端可服务的模型条目（容量是接口的一部分，06 §10.8）。 */
export interface ModelInfo {
  id: string;
  memoryGb: number;
}

/** 压力分级，与 oMLX 的 ok/soft/hard/ceiling 同形（06 §10.3）。 */
export type Pressure = "ok" | "soft" | "hard" | "ceiling";

/** 能否在不驱逐的情况下加载。 */
export type Admission = "coexist" | "requires_eviction";

export interface BackendStatus {
  pressure: Pressure;
  usedGb: number;
  modelMemoryMaxGb: number;
  loaded: string[];
}

export interface ChatMessage {
  role: string;
  content: string;
}

export interface ChatRequest {
  model: string;
  messages: ChatMessage[];
}

export interface ChatResponse {
  model: string;
  content: string;
}

/**
 * 引擎无关的本地模型后端（T1.2.1）。
 *
 * LoadPolicy 的抽象语义（resident / on_demand / evict_to_load、pinned/idle_ttl、
 * admission）由实现负责映射到具体引擎；Router 只认这个接口，**不出现任何引擎名**。
 */
export interface ModelBackend {
  list(): Promise<ModelInfo[]>;
  load(id: string, policy?: LoadPolicy): Promise<void>;
  unload(id: string): Promise<void>;
  admission(id: string): Promise<Admission>;
  status(): Promise<BackendStatus>;
  chat(req: ChatRequest): Promise<ChatResponse>;
}
