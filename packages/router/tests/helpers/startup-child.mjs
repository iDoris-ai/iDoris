// 真实启动 Router，用进程退出码验证 T1.4.2 的 fail-closed 门禁。
//
// 从编译产物 dist/ 导入，不是 src/*.ts：以前这里用 --experimental-transform-types
// 直接跑 TS 源码，Node 22/24/26 三个版本对这个实验标志的支持不一致——Node 26
// 干脆不认这个选项，会直接报 bad option 崩掉。CI 的 `pnpm build` 已经在
// `pnpm test` 之前跑过，所以 dist/ 在测试时总是存在（本地手跑也要先 build）。
import { startRouter } from "../../dist/index.js";

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
