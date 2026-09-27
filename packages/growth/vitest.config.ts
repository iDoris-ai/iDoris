import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: {
    alias: {
      "@idoris/contracts": fileURLToPath(new URL("../contracts/src/index.ts", import.meta.url)),
    },
  },
  // 集成测试默认不跑（需要真 MLX / 真权重）：pnpm --filter @idoris/growth test:integration
  test: {
    exclude: ["**/node_modules/**", "**/dist/**", "tests/integration/**"],
  },
});
