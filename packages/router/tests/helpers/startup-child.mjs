// 真实启动 Router，用进程退出码验证 T1.4.2 的 fail-closed 门禁。
import { startRouter } from "../../src/server.ts";

const componentsDir = process.env.IDORIS_TEST_COMPONENTS_DIR;
try {
  const router = await startRouter({ componentsDir });
  console.log("STARTED components=" + router.registered.length);
  await router.close();
  process.exit(0);
} catch (err) {
  console.error("STARTUP_REFUSED: " + (err instanceof Error ? err.message : String(err)));
  process.exit(1);
}
