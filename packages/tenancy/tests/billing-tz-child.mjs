/**
 * T2.6.1 跨时区子进程：由 tests/billing.test.ts 带 `TZ=<zone>` 启动。
 *
 * 它回显 `Intl` 实际解析到的进程时区，父测试据此证明「TZ 真的切换了」——
 * 而不是三个进程其实都在同一时区里自洽地全绿。
 *
 * 用 `node` 直接加载 TS 源（Node >= 22.18 默认启用类型剥离；更早的 22.x 由父测试传
 * `--experimental-strip-types`）。billing.ts / store.ts 对 `@idoris/contracts` 和
 * `./store.js` 都只用 `import type`，类型剥离后不产生运行时解析，因此可免打包直载。
 */
import { BILLING_TENANT, ENTRIES, PERIOD } from "./billing-fixture.ts";
import { queryMonthlyUsage, writeUsageRecord } from "../src/billing.ts";
import { TenantStore } from "../src/store.ts";

const store = new TenantStore();
for (const entry of ENTRIES) {
  writeUsageRecord(store, BILLING_TENANT, entry);
}

const usage = queryMonthlyUsage(store, BILLING_TENANT, PERIOD);

process.stdout.write(
  "\n__BILLING_RESULT__" +
    JSON.stringify({
      process_timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      usage,
    }) +
    "\n",
);
