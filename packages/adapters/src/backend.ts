import type { LoadPolicy } from "@idoris/contracts";

/** 后端可服务的模型条目（容量是接口的一部分，06 §10.8）。 */
export interface ModelInfo {
  id: string;
  memoryGb: number;
}

/**
 * 压力分级，与 oMLX 的 ok/soft/hard/ceiling 同形（06 §10.3）。
 *
 * `"unknown"`：后端没有报告压力状态（字段缺失/后端不支持），**不等于 `"ok"`**——
 * 一个不知道自己压力状态的后端不能被当成"压力正常"。任何消费方（router 的
 * admission/驱逐决策等）遇到 `"unknown"` 必须按**保守方向**处理，即视同至少
 * `"soft"`（宁可多驱逐/少接纳，不能当 `"ok"` 用）。截至本次修复，仓库内还没有
 * 任何消费方真正读 `.pressure` 做决策（`packages/router/src/capabilities.ts`
 * 只读 `.loaded.length`），所以这里先把类型和语义钉死，留给将来第一个消费方实现。
 */
export type Pressure = "ok" | "soft" | "hard" | "ceiling" | "unknown";

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
  /** 可选取消信号：spawn 型后端据此杀掉整个进程组。 */
  signal?: AbortSignal;
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
