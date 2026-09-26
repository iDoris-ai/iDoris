import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: {
    alias: {
      "@idoris/contracts": fileURLToPath(new URL("../contracts/src/index.ts", import.meta.url)),
      "@idoris/growth": fileURLToPath(new URL("../growth/src/index.ts", import.meta.url)),
    },
  },
  test: { include: ["tests/integration/**/*.test.ts"] },
});
