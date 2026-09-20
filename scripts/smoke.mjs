// T1.3.1 smoke：起 Router（loopback），断言 /v1/models 非空。前置于 pnpm build。
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { startRouter } from "../packages/router/dist/index.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const router = await startRouter({ componentsDir: join(root, "config", "components"), port: 0 });
try {
  const res = await fetch("http://127.0.0.1:" + router.port + "/v1/models");
  const body = await res.json();
  if (!Array.isArray(body.data) || body.data.length === 0) {
    console.error("smoke FAILED: /v1/models returned no data");
    process.exit(1);
  }
  console.log("smoke OK: " + body.data.length + " model(s) at http://127.0.0.1:" + router.port + " (bind=" + router.host + ")");
} finally {
  await router.close();
}
