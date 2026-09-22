import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  resolve: {
    alias: {
      "@idoris/contracts": fileURLToPath(new URL("../contracts/src/index.ts", import.meta.url)),
      "@idoris/adapters": fileURLToPath(new URL("../adapters/src/index.ts", import.meta.url)),
      "@idoris/recommender": fileURLToPath(new URL("../recommender/src/index.ts", import.meta.url)),
      "@idoris/tenancy": fileURLToPath(new URL("../tenancy/src/index.ts", import.meta.url)),
    },
  },
});
