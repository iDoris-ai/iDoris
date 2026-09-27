import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

// 集成测试专用配置：默认 vitest.config.ts 把 tests/integration 排除在 `test` 之外，
// 这里反过来只跑它，避免「默认跑一次慢的、集成又跑不到」这种两头不落的情况。
export default defineConfig({
  resolve: {
    alias: {
      "@idoris/contracts": fileURLToPath(new URL("../contracts/src/index.ts", import.meta.url)),
    },
  },
  test: { include: ["tests/integration/**/*.test.ts"] },
});
