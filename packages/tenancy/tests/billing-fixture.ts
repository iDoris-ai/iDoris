/**
 * T2.6.1 跨时区回归测试共享的固定数据。
 *
 * 所有 `ts_utc` 都用 `Date.UTC` 构造——它们在任意机器时区下都是同一个瞬间。
 * 绝不能用本地 Date 构造器造时间戳：那会随进程 TZ 漂移，正是这个测试要抓的 bug
 * （旧测试「两边一起漂，在任何时区都自洽地全绿」）。
 *
 * 租户固定 Asia/Bangkok（UTC+7），2026-09 的 UTC 边界是
 * `[2026-08-31T17:00:00Z, 2026-09-30T17:00:00Z)`。
 * 5 条记录刻意压在边界两侧，使「服务器时区边界」版本必然得到不同的 totals/range。
 */
import type { TenantContext } from "@idoris/contracts";

export const BILLING_TENANT: TenantContext = {
  tenant_id: "acme-co",
  billing_timezone: "Asia/Bangkok",
  budget: { limit_minor: 5_000_000, spent_minor: 1_234_567, scope: "paid_only" },
};

export const PERIOD = "2026-09";

export type BillingFixtureEntry = {
  request_id: string;
  ts_utc: number;
  cost_minor: number;
  tokens_in: number;
  tokens_out: number;
};

export const ENTRIES: readonly BillingFixtureEntry[] = [
  // 曼谷 2026-09-15 07:00 —— 月内
  {
    request_id: "in-month",
    ts_utc: Date.UTC(2026, 8, 15, 0, 0, 0),
    cost_minor: 100,
    tokens_in: 1000,
    tokens_out: 500,
  },
  // 曼谷 2026-09-01 00:30 → UTC 08-31T17:30：UTC 看是 8 月，曼谷应计 9 月
  {
    request_id: "boundary-start",
    ts_utc: Date.UTC(2026, 7, 31, 17, 30, 0),
    cost_minor: 1,
    tokens_in: 10,
    tokens_out: 1,
  },
  // 曼谷 2026-09-30 23:30 → UTC 09-30T16:30：曼谷月末，应计 9 月
  {
    request_id: "boundary-end",
    ts_utc: Date.UTC(2026, 8, 30, 16, 30, 0),
    cost_minor: 2,
    tokens_in: 20,
    tokens_out: 2,
  },
  // 曼谷 2026-10-01 00:30 → UTC 09-30T17:30：曼谷已进 10 月，应排除
  {
    request_id: "just-after-oct",
    ts_utc: Date.UTC(2026, 8, 30, 17, 30, 0),
    cost_minor: 1000,
    tokens_in: 9999,
    tokens_out: 9999,
  },
  // 曼谷 2026-08-31 23:30 → UTC 08-31T16:30：曼谷仍是 8 月，应排除
  {
    request_id: "just-before-sep",
    ts_utc: Date.UTC(2026, 7, 31, 16, 30, 0),
    cost_minor: 1000,
    tokens_in: 9999,
    tokens_out: 9999,
  },
];

export const EXPECTED_TOTALS = { cost_minor: 103, tokens_in: 1030, tokens_out: 503, calls: 3 };

export const EXPECTED_RANGE_UTC = {
  from: "2026-08-31T17:00:00Z",
  to: "2026-09-30T17:00:00Z",
};
