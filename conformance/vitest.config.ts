import { defineConfig } from "vitest/config";

// 契约套件要起真实子进程 + 真实网络请求（重试退避、慢响应注入等），
// 默认 5s/测试对这些场景不够，统一放宽。
export default defineConfig({
  test: {
    testTimeout: 20_000,
    hookTimeout: 20_000,
    fileParallelism: false,
  },
});
