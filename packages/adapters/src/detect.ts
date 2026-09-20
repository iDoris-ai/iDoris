/**
 * 跨平台后端探测（T1.2.3）。
 *
 * 目标：启动时按 OS/硬件选后端，**Router 核心不出现任何引擎名**。
 * 非 macOS 的实现可先返回 `implemented: false` 占位，但接口必须齐
 * （调用方据此知道「这个平台该用哪个后端、有没有实现」）。
 */

export type HostPlatform = "darwin" | "win32" | "linux" | "other";
export type HostArch = "arm64" | "x64" | "other";

export interface HostFacts {
  platform: HostPlatform;
  arch: HostArch;
  /** 是否探测到 NVIDIA GPU（决定 Win/Linux 上走 vLLM 还是 llama.cpp/Ollama）。 */
  hasNvidiaGpu: boolean;
}

export type BackendKind = "omlx" | "vllm" | "llama_cpp" | "ollama";

export interface BackendChoice {
  kind: BackendKind;
  /** 该平台/后端的适配器是否已实现；未实现的由上层决定是否拒绝启动。 */
  implemented: boolean;
  reason: string;
}

/**
 * 后端选择（06 §10.3 的跨平台表）：
 * - macOS Apple Silicon → oMLX（默认实现）
 * - Win/Linux + NVIDIA → vLLM 槽位
 * - 其余（含 Intel Mac / 无独显）→ llama.cpp(Ollama) 槽位
 */
export function detectBackend(facts: HostFacts): BackendChoice {
  if (facts.platform === "darwin" && facts.arch === "arm64") {
    return { kind: "omlx", implemented: true, reason: "macOS Apple Silicon: oMLX is the default local backend" };
  }
  if ((facts.platform === "win32" || facts.platform === "linux") && facts.hasNvidiaGpu) {
    return { kind: "vllm", implemented: false, reason: "Windows/Linux with NVIDIA: vLLM slot (adapter not implemented yet)" };
  }
  return {
    kind: "llama_cpp",
    implemented: false,
    reason: "no dedicated GPU / non-Apple-Silicon: llama.cpp (GGUF) slot (adapter not implemented yet)",
  };
}

/** 读当前主机事实（不依赖 @types/node：用 globalThis 探测）。 */
export function currentHostFacts(): HostFacts {
  const g = globalThis as { process?: { platform?: string; arch?: string } };
  const platform = g.process?.platform;
  const arch = g.process?.arch;
  return {
    platform: platform === "darwin" || platform === "win32" || platform === "linux" ? platform : "other",
    arch: arch === "arm64" || arch === "x64" ? arch : "other",
    hasNvidiaGpu: false,
  };
}
