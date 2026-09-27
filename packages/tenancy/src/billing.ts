/**
 * T2.6.1 — 按 tenant 的月度用量聚合 + 显式账期时区。
 *
 * 计费红线（contract-tenancy §6 / spec.md「用量聚合与账期时区」）：
 *  - `period=YYYY-MM` 的月份边界**一律用租户显式配置的 `billing_timezone` 解释**，
 *    既不取服务器本地时区，也不接受调用方在查询里另指定时区；
 *  - 记录里的 `ts_utc` 存 UTC epoch（毫秒），**只在聚合时**做时区换算；
 *  - 响应回显 `billing_timezone` 与解析出的 `range_utc {from,to}`，让调用方能断言；
 *  - 用量/预算查询与写入都经 `TenantStore`：缺 tenant 上下文即抛 `TenantScopeError`，
 *    绝不返回全量。
 *
 * 真实踩过的坑：月份边界用本地时区而时间戳存 UTC，同一笔「曼谷 10-01 06:00」的调用
 * 在 UTC 算 9 月、在曼谷算 10 月——换台机器账单就变且没有任何东西报错。回归测试必须
 * 真的切换进程时区跑同一批数据，见 tests/billing.test.ts + tests/billing-tz-child.mjs。
 */
import type { TenantContext } from "@idoris/contracts";
import type { TenantScopedRecord, TenantStore } from "./store.js";

/** 写入 `usage` 台账的一条记录：`ts_utc` 是 UTC epoch 毫秒，绝不存本地时间。 */
export interface UsageEntry {
  /** UTC epoch 毫秒。 */
  ts_utc: number;
  tokens_in?: number;
  tokens_out?: number;
  cost_minor?: number;
  request_id?: string;
}

export interface UsageTotals {
  cost_minor: number;
  tokens_in: number;
  tokens_out: number;
  calls: number;
}

export interface RangeUtc {
  from: string;
  to: string;
}

/** `GET /idoris/tenants/{tenant_id}/usage?period=YYYY-MM` 的响应体。 */
export interface MonthlyUsage {
  tenant_id: string;
  period: string;
  billing_timezone: string;
  range_utc: RangeUtc;
  totals: UsageTotals;
}

/** `GET /idoris/tenants/{tenant_id}/budget` 的响应体（余额）。 */
export interface Balance {
  tenant_id: string;
  billing_timezone: string;
  limit_minor: number;
  spent_minor: number;
  remaining_minor: number;
  scope: "paid_only" | "all";
}

/**
 * 聚合来源：默认 `usage` 台账。
 * 契约 §6 的 `audit` 记录带同样的计费字段（`ts_utc`/`tokens_in`/`tokens_out`/`cost_minor`），
 * 可作为计费依据显式传入 `{ source: "audit" }`。
 */
export type UsageSource = "usage" | "audit";

export interface MonthlyUsageOptions {
  source?: UsageSource;
}

export class BillingError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BillingError";
  }
}

export class BillingPeriodError extends BillingError {
  readonly period: string;

  constructor(period: string) {
    super('billing period must be "YYYY-MM" (got "' + period + '")');
    this.name = "BillingPeriodError";
    this.period = period;
  }
}

export class BillingTimezoneError extends BillingError {
  readonly timeZone: string;

  constructor(timeZone: string) {
    super('billing_timezone is not a supported IANA time zone: "' + timeZone + '"');
    this.name = "BillingTimezoneError";
    this.timeZone = timeZone;
  }
}

export class BillingRecordError extends BillingError {
  constructor(message: string) {
    super(message);
    this.name = "BillingRecordError";
  }
}

const PERIOD_PATTERN = /^(\d{4})-(0[1-9]|1[0-2])$/;

interface PeriodParts {
  year: number;
  /** 1–12。 */
  month: number;
}

/** 解析 `YYYY-MM`；任何别的形状都拒绝，不猜测、不回落。 */
export function parseBillingPeriod(period: string): PeriodParts {
  const match = PERIOD_PATTERN.exec(period);
  if (match === null) throw new BillingPeriodError(period);
  return { year: Number(match[1] ?? ""), month: Number(match[2] ?? "") };
}

const formatterCache = new Map<string, Intl.DateTimeFormat>();

function formatterFor(timeZone: string): Intl.DateTimeFormat {
  const cached = formatterCache.get(timeZone);
  if (cached !== undefined) return cached;
  let formatter: Intl.DateTimeFormat;
  try {
    formatter = new Intl.DateTimeFormat("en-US", {
      timeZone,
      hourCycle: "h23",
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  } catch {
    throw new BillingTimezoneError(timeZone);
  }
  formatterCache.set(timeZone, formatter);
  return formatter;
}

/**
 * 该 UTC 瞬间在 `timeZone` 的本地墙钟相对 UTC 的偏移（毫秒，东八区为正）。
 * 用显式 `timeZone` 的 Intl 格式化，因此结果与进程 TZ 无关——这是「换台机器账单不变」的根。
 */
function timeZoneOffsetMs(timeZone: string, utcEpochMs: number): number {
  const parts = formatterFor(timeZone).formatToParts(new Date(utcEpochMs));
  let year = 0;
  let month = 1;
  let day = 1;
  let hour = 0;
  let minute = 0;
  let second = 0;
  for (const part of parts) {
    const value = Number(part.value);
    switch (part.type) {
      case "year":
        year = value;
        break;
      case "month":
        month = value;
        break;
      case "day":
        day = value;
        break;
      case "hour":
        hour = value;
        break;
      case "minute":
        minute = value;
        break;
      case "second":
        second = value;
        break;
      default:
        break;
    }
  }
  return Date.UTC(year, month - 1, day, hour, minute, second) - utcEpochMs;
}

/** 把 `timeZone` 里的某天 00:00:00 墙钟时间换算成 UTC epoch（毫秒）。 */
function zonedMidnightUtcMs(timeZone: string, year: number, month: number, day: number): number {
  const wallClockAsUtc = Date.UTC(year, month - 1, day, 0, 0, 0);
  const firstOffset = timeZoneOffsetMs(timeZone, wallClockAsUtc);
  const candidate = wallClockAsUtc - firstOffset;
  // DST 切换会让第一次估算落在另一个偏移上，用候选瞬间的偏移再校正一次。
  const secondOffset = timeZoneOffsetMs(timeZone, candidate);
  return secondOffset === firstOffset ? candidate : wallClockAsUtc - secondOffset;
}

/** 租户时区下 `period` 的 UTC 半开区间 `[from, to)`（毫秒）。 */
export function resolveBillingPeriodRange(
  period: string,
  billingTimezone: string,
): { from: number; to: number } {
  const { year, month } = parseBillingPeriod(period);
  const nextYear = month === 12 ? year + 1 : year;
  const nextMonth = month === 12 ? 1 : month + 1;
  return {
    from: zonedMidnightUtcMs(billingTimezone, year, month, 1),
    to: zonedMidnightUtcMs(billingTimezone, nextYear, nextMonth, 1),
  };
}

function optionalNumber(value: unknown, fallback: number, field: string): number {
  if (value === undefined || value === null) return fallback;
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new BillingRecordError("billing field " + field + " must be a finite number");
  }
  return value;
}

/** 只对落在 UTC 区间内的记录求和；`ts_utc` 缺失/非法即抛错（静默漏账比报错更危险）。 */
export function aggregateUsageRecords(
  records: readonly TenantScopedRecord[],
  range: { from: number; to: number },
): UsageTotals {
  let costMinor = 0;
  let tokensIn = 0;
  let tokensOut = 0;
  let calls = 0;
  for (const record of records) {
    const tsUtc = record.payload.ts_utc;
    if (typeof tsUtc !== "number" || !Number.isFinite(tsUtc)) {
      throw new BillingRecordError(
        'usage record "' + record.id + '" must store ts_utc as a finite UTC epoch',
      );
    }
    if (tsUtc < range.from || tsUtc >= range.to) continue;
    calls += 1;
    costMinor += optionalNumber(record.payload.cost_minor, 0, "cost_minor");
    tokensIn += optionalNumber(record.payload.tokens_in, 0, "tokens_in");
    tokensOut += optionalNumber(record.payload.tokens_out, 0, "tokens_out");
  }
  return { cost_minor: costMinor, tokens_in: tokensIn, tokens_out: tokensOut, calls };
}

function toIsoUtc(epochMs: number): string {
  const iso = new Date(epochMs).toISOString();
  // 契约示例形如 2026-08-31T17:00:00Z；整秒边界不显示 .000。
  return iso.endsWith(".000Z") ? iso.slice(0, -5) + "Z" : iso;
}

/** 写一条 `usage` 记录。`store.put` 是 tenant 作用域闸门：缺 ctx 直接抛 `TenantScopeError`。 */
export function writeUsageRecord(
  store: TenantStore,
  ctx: TenantContext | undefined,
  entry: UsageEntry,
): TenantScopedRecord {
  if (typeof entry.ts_utc !== "number" || !Number.isFinite(entry.ts_utc)) {
    throw new BillingRecordError("usage ts_utc must be a finite UTC epoch (milliseconds)");
  }
  const id = entry.request_id !== undefined ? String(entry.request_id) : String(entry.ts_utc);
  return store.put(ctx, "usage", id, { ...entry });
}

/**
 * 月度用量与成本查询。响应必填 `billing_timezone` 与 `range_utc`。
 * `store.list(ctx, source)` 先做 tenant 作用域闸门：ctx 缺失时抛 `TenantScopeError`。
 */
export function queryMonthlyUsage(
  store: TenantStore,
  ctx: TenantContext | undefined,
  period: string,
  options: MonthlyUsageOptions = {},
): MonthlyUsage {
  const source: UsageSource = options.source ?? "usage";
  const records = store.list(ctx, source);
  if (ctx === undefined) {
    // 上面的 store.list 已抛 TenantScopeError；这里仅为 TypeScript 收窄，正常不可达。
    throw new BillingError("tenant scope is required for billing queries");
  }
  const billingTimezone = ctx.billing_timezone;
  const range = resolveBillingPeriodRange(period, billingTimezone);
  return {
    tenant_id: ctx.tenant_id,
    period,
    billing_timezone: billingTimezone,
    range_utc: { from: toIsoUtc(range.from), to: toIsoUtc(range.to) },
    totals: aggregateUsageRecords(records, range),
  };
}

/**
 * 余额查询。预算状态取 `TenantStore` 中最新的 `budget` 快照，无快照时回落到
 * `TenantContext.budget`（spec.md：`spent_minor` 由 Router 维护）。
 * `store.list(ctx, "budget")` 是 tenant 作用域闸门。
 */
export function queryBalance(store: TenantStore, ctx: TenantContext | undefined): Balance {
  const snapshots = store.list(ctx, "budget");
  if (ctx === undefined) {
    // 上面的 store.list 已抛 TenantScopeError；这里仅为 TypeScript 收窄，正常不可达。
    throw new BillingError("tenant scope is required for billing queries");
  }
  const latest = snapshots[snapshots.length - 1];
  const payload = latest?.payload ?? {};
  const limitMinor = optionalNumber(payload.limit_minor, ctx.budget.limit_minor, "limit_minor");
  const spentMinor = optionalNumber(payload.spent_minor, ctx.budget.spent_minor, "spent_minor");
  const scope =
    payload.scope === "all" || payload.scope === "paid_only" ? payload.scope : ctx.budget.scope;
  return {
    tenant_id: ctx.tenant_id,
    billing_timezone: ctx.billing_timezone,
    limit_minor: limitMinor,
    spent_minor: spentMinor,
    remaining_minor: limitMinor - spentMinor,
    scope,
  };
}
