import { describe, expect, it } from "vitest";
import { currentHostFacts, detectBackend } from "../src/detect.js";

describe("detectBackend", () => {
  it("picks oMLX on macOS Apple Silicon (implemented)", () => {
    expect(detectBackend({ platform: "darwin", arch: "arm64", hasNvidiaGpu: false })).toMatchObject({ kind: "omlx", implemented: true });
  });
  it("picks the vLLM slot on Win/Linux with NVIDIA (not implemented yet)", () => {
    for (const platform of ["win32", "linux"] as const) {
      expect(detectBackend({ platform, arch: "x64", hasNvidiaGpu: true })).toMatchObject({ kind: "vllm", implemented: false });
    }
  });
  it("falls back to the llama.cpp slot elsewhere", () => {
    expect(detectBackend({ platform: "darwin", arch: "x64", hasNvidiaGpu: false })).toMatchObject({ kind: "llama_cpp", implemented: false });
    expect(detectBackend({ platform: "linux", arch: "x64", hasNvidiaGpu: false })).toMatchObject({ kind: "llama_cpp", implemented: false });
    expect(detectBackend({ platform: "other", arch: "other", hasNvidiaGpu: false })).toMatchObject({ kind: "llama_cpp", implemented: false });
  });
  it("currentHostFacts returns a well-formed shape", () => {
    const f = currentHostFacts();
    expect(["darwin", "win32", "linux", "other"]).toContain(f.platform);
    expect(["arm64", "x64", "other"]).toContain(f.arch);
    expect(typeof f.hasNvidiaGpu).toBe("boolean");
  });
});
