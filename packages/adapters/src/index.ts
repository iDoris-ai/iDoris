export type {
  Admission,
  BackendStatus,
  ChatMessage,
  ChatRequest,
  ChatResponse,
  ModelBackend,
  ModelInfo,
  Pressure,
} from "./backend.js";
export { detectBackend, currentHostFacts } from "./detect.js";
export type { BackendChoice, BackendKind, HostArch, HostFacts, HostPlatform } from "./detect.js";
export { createBackend } from "./factory.js";
export { MockBackend } from "../mock/mock-backend.js";
export { OmlxBackend } from "../omlx/omlx-backend.js";
// T1.4.1 订阅中转（能力①，fail-closed）
export * from "../subscription/index.js";
