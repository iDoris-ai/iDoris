/**
 * T2.6.1 验收：同一批用量数据在 TZ=UTC / Asia/Bangkok / Pacific/Midway 下聚合，
 * `totals` 与 `range_utc` 必须完全一致。
 *
 * 为什么必须用子进程：`process.env.TZ` 在运行中赋值是否被 Node 的 Date/Intl 重读取决于
 * 版本与首次使用时机；只有「启动前带 TZ 的独立 node 进程」才真正切换进程时区。子进程还会
 * 回显它实际解析到的 `process_timezone`，父测试断言三者互不相同——否则「三个时区结果相同」
 * 可能只是因为三个进程根本没换时区。
 *
 * 变异说明（改坏后必须变红的那条）：把边界换算改成服务器本地时区后
 *   - TZ=UTC           → range_utc 变 2026-09-01T00:00:00Z..2026-10-01T00:00:00Z，totals 变 1102；
 *   - TZ=Pacific/Midway → range_utc 变 2026-09-01T11:00:00Z..2026-10-01T11:00:00Z，totals 变 1102；
 *   - TZ=Asia/Bangkok   → 该次恰好正确（服务器 tz == 租户 tz），但已与前两次不一致。
 * 所以「三次两两完全一致」+「等于曼谷边界」两条断言都会变红；只比较 totals 不够——
 * 两个都错的边界也可能凑出相同的总数，故 range_utc 必须一起钉死（见 FU-8 量纲教训）。
 */
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { beforeAll, describe, expect, it } from "vitest";
import {
  aggregateUsageRecords,
  BillingPeriodError,
  BillingTimezoneError,
  queryBalance,
  queryMonthlyUsage,
  resolveBillingPeriodRange,
  writeUsageRecord,
} from "../src/billing.js";
import { TenantScopeError, TenantStore } from "../src/store.js";
import {
  BILLING_TENANT,
  ENTRIES,
  EXPECTED_RANGE_UTC,
  EXPECTED_TOTALS,
  PERIOD,
} from "./billing-fixture.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const CHILD_RUNNER = join(HERE, "billing-tz-child.mjs");
const RESULT_MARKER = "__BILLING_RESULT__";
const PROCESS_TIMEZONES = ["UTC", "Asia/Bangkok", "Pacific/Midway"] as const;

interface ChildUsage {
  tenant_id: string;
  period: string;
  billing_timezone: string;
  range_utc: { from: string; to: string };
  totals: { cost_minor: number; tokens_in: number; tokens_out: number; calls: number };
}

interface ChildResult {
  process_timezone: string;
  usage: ChildUsage;
}

function runChild(timeZone: string): ChildResult {
  const result = spawnSync(process.execPath, ["--experimental-strip-types", CHILD_RUNNER], {
    env: { ...process.env, TZ: timeZone },
    encoding: "utf8",
  });
  if (result.error !== undefined) throw result.error;
  const stdout = result.stdout ?? "";
  const stderr = result.stderr ?? "";
  if (result.status !== 0) {
    throw new Error("billing child under TZ=" + timeZone + " failed: " + stderr);
  }
  const line = stdout.split("\n").find((candidate) => candidate.startsWith(RESULT_MARKER));
  if (line === undefined) {
    throw new Error("billing child under TZ=" + timeZone + " produced no result: " + stdout);
  }
  return JSON.parse(line.slice(RESULT_MARKER.length)) as ChildResult;
}

describe("T2.6.1 月度用量聚合 + 显式账期时区", () => {
  let results: ChildResult[] = [];

  beforeAll(() => {
    results = PROCESS_TIMEZONES.map((timeZone) => runChild(timeZone));
  }, 30_000);

  it("三个子进程真的跑在不同的进程时区（否则下面的「一致」不证明任何事）", () => {
    expect(new Set(results.map((result) => result.process_timezone)).size).toBe(
      PROCESS_TIMEZONES.length,
    );
  });

  it("同一批数据在不同进程时区下 totals 与 range_utc 完全一致，且等于租户时区边界", () => {
    const first = results[0];
    expect(first).toBeDefined();
    if (first === undefined) return;
    for (const result of results) {
      expect(result.usage.totals).toEqual(first.usage.totals);
      expect(result.usage.range_utc).toEqual(first.usage.range_utc);
      // 响应的必填回显字段。
      expect(result.usage.tenant_id).toBe(BILLING_TENANT.tenant_id);
      expect(result.usage.period).toBe(PERIOD);
      expect(result.usage.billing_timezone).toBe(BILLING_TENANT.billing_timezone);
    }
    // 只断言「彼此一致」还不够：两个都错的边界也可能一致。必须钉死曼谷解析出的 UTC 边界。
    expect(first.usage.totals).toEqual(EXPECTED_TOTALS);
    expect(first.usage.range_utc).toEqual(EXPECTED_RANGE_UTC);
  });

  it("变异守卫：服务器时区边界会得到不同的 range_utc 与 totals", () => {
    const store = new TenantStore();
    for (const entry of ENTRIES) {
      store.put(BILLING_TENANT, "usage", entry.request_id, { ...entry });
    }
    const rows = store.list(BILLING_TENANT, "usage");
    const tenantRange = resolveBillingPeriodRange(PERIOD, BILLING_TENANT.billing_timezone);
    // 变异实现：边界按服务器时区（此处以 UTC 为例）解析。
    const serverTimeZoneRange = { from: Date.UTC(2026, 8, 1), to: Date.UTC(2026, 9, 1) };
    expect(tenantRange).not.toEqual(serverTimeZoneRange);
    expect(aggregateUsageRecords(rows, serverTimeZoneRange)).not.toEqual(
      aggregateUsageRecords(rows, tenantRange),
    );
    expect(aggregateUsageRecords(rows, tenantRange)).toEqual(EXPECTED_TOTALS);
  });

  it("租户硬隔离：A 只聚合到 A 的记录", () => {
    const store = new TenantStore();
    writeUsageRecord(store, BILLING_TENANT, {
      request_id: "a",
      ts_utc: Date.UTC(2026, 8, 10, 0, 0, 0),
      cost_minor: 100,
      tokens_in: 1000,
      tokens_out: 500,
    });
    const other: typeof BILLING_TENANT = { ...BILLING_TENANT, tenant_id: "other-co" };
    writeUsageRecord(store, other, {
      request_id: "b",
      ts_utc: Date.UTC(2026, 8, 10, 0, 0, 0),
      cost_minor: 9999,
      tokens_in: 9999,
      tokens_out: 9999,
    });
    expect(queryMonthlyUsage(store, BILLING_TENANT, PERIOD).totals.cost_minor).toBe(100);
    expect(queryMonthlyUsage(store, other, PERIOD).totals.cost_minor).toBe(9999);
  });

  it("缺 tenant 作用域直接抛 TenantScopeError（不是返回全量）", () => {
    const store = new TenantStore();
    writeUsageRecord(store, BILLING_TENANT, {
      request_id: "a",
      ts_utc: Date.UTC(2026, 8, 10, 0, 0, 0),
    });
    expect(() => queryMonthlyUsage(store, undefined, PERIOD)).toThrow(TenantScopeError);
    expect(() => queryBalance(store, undefined)).toThrow(TenantScopeError);
    expect(() => writeUsageRecord(store, undefined, { request_id: "x", ts_utc: 0 })).toThrow(
      TenantScopeError,
    );
  });

  it("余额查询回显 tenant/billing_timezone，store 预算快照优先于 TenantContext", () => {
    const store = new TenantStore();
    const fromContext = queryBalance(store, BILLING_TENANT);
    expect(fromContext.tenant_id).toBe(BILLING_TENANT.tenant_id);
    expect(fromContext.billing_timezone).toBe(BILLING_TENANT.billing_timezone);
    expect(fromContext.limit_minor).toBe(5_000_000);
    expect(fromContext.spent_minor).toBe(1_234_567);
    expect(fromContext.remaining_minor).toBe(5_000_000 - 1_234_567);

    store.put(BILLING_TENANT, "budget", "snap", {
      limit_minor: 100,
      spent_minor: 40,
      scope: "all",
    });
    expect(queryBalance(store, BILLING_TENANT)).toEqual({
      tenant_id: "acme-co",
      billing_timezone: "Asia/Bangkok",
      limit_minor: 100,
      spent_minor: 40,
      remaining_minor: 60,
      scope: "all",
    });
  });

  it("非法 period / 时区直接拒绝，绝不静默回落到服务器时区", () => {
    expect(() => resolveBillingPeriodRange("2026-13", "Asia/Bangkok")).toThrow(BillingPeriodError);
    expect(() => resolveBillingPeriodRange("2026-9", "Asia/Bangkok")).toThrow(BillingPeriodError);
    expect(() => resolveBillingPeriodRange(PERIOD, "Not/AZone")).toThrow(BillingTimezoneError);
  });

  it("可显式聚合 audit 台账（契约 §6 的计费依据）", () => {
    const store = new TenantStore();
    for (const entry of ENTRIES) {
      store.put(BILLING_TENANT, "audit", entry.request_id, { ...entry, reason: "intent_match" });
    }
    const usage = queryMonthlyUsage(store, BILLING_TENANT, PERIOD, { source: "audit" });
    expect(usage.totals).toEqual(EXPECTED_TOTALS);
    expect(usage.range_utc).toEqual(EXPECTED_RANGE_UTC);
  });
});
