/**
 * T2.2.3 — 路由决策审计日志（只存元数据，绝不记录内容）。
 *
 * 字段**穷举白名单**见 spec.md「审计记录（AuditRecord）」：
 *   request_id / tenant_id / component / intent / privacy / tier / provider_id /
 *   model_id / tokens_in / tokens_out / cost_minor / latency_ms / status / reason / ts_utc。
 *
 * 两道防线（spec.md / 06 §10.8；移植自 iDoris-website，Apache-2.0）：
 *  ① 写入前对**字段名**逐个比对黑名单 frozenset，命中即抛 `ContentLeakError`
 *     **拒绝写入**——不是静默丢弃（静默丢弃会让人以为内容被存下来了）；
 *  ② 单字段 500 字符上限——长文本出现在元数据里，本身就是「有人把内容塞进来了」的信号。
 *
 * `reason` 必须非空且能区分四类：privacy_enforced / budget / intent_match / degraded。
 * 写入走 `TenantStore` 的 tenant 作用域（T1.5.3）：缺 tenant 上下文即抛错，
 * 且记录里的 `tenant_id` 必须与上下文一致（防跨租户伪造）。
 */
import type { TenantContext } from "@idoris/contracts";
import { TenantScopeError, TenantStore, type TenantScopedRecord } from "@idoris/tenancy";
import { REASON_KINDS, type ReasonKind } from "./reason.js";

/** spec.md 审计记录字段穷举白名单（顺序即契约中的顺序）。 */
export const AUDIT_FIELDS = [
  "request_id",
  "tenant_id",
  "component",
  "intent",
  "privacy",
  "tier",
  "provider_id",
  "model_id",
  "tokens_in",
  "tokens_out",
  "cost_minor",
  "latency_ms",
  "status",
  "reason",
  "ts_utc",
] as const;
export type AuditField = (typeof AUDIT_FIELDS)[number];

const AUDIT_FIELD_SET: ReadonlySet<string> = new Set<string>(AUDIT_FIELDS);

/**
 * 内容字段名黑名单 frozenset。命中即**拒绝写入**（抛出），绝不清洗后落盘。
 * 比较前统一转小写，因此 `Prompt` / `CONTENT` 同样命中。
 */
export const CONTENT_FIELD_BLACKLIST: ReadonlySet<string> = Object.freeze(
  new Set<string>([
    "prompt",
    "prompts",
    "input",
    "inputs",
    "content",
    "contents",
    "text",
    "texts",
    "body",
    "messages",
    "message",
    "document",
    "documents",
    "file",
    "files",
    "payload",
    "completion",
    "completions",
    "response",
    "responses",
    "output",
    "outputs",
    "query",
    "answer",
    "raw",
    "attachment",
    "attachments",
    "image",
    "images",
    "audio",
    "transcript",
    "data",
  ]),
);

/** 单字段字符上限（spec.md 第②道防线）。 */
export const MAX_FIELD_CHARS = 500;

/** 审计写入失败基类。 */
export class AuditError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AuditError";
  }
}

/** 字段名命中内容黑名单：拒绝写入（不是静默丢弃）。 */
export class ContentLeakError extends AuditError {
  constructor(readonly field: string) {
    super('refusing to write audit record: field "' + field + '" is on the content blacklist');
    this.name = "ContentLeakError";
  }
}

/** 字段名不在白名单内。 */
export class UnknownAuditFieldError extends AuditError {
  constructor(readonly field: string) {
    super("unknown audit field: " + field);
    this.name = "UnknownAuditFieldError";
  }
}

/** 单字段超过 500 字符。 */
export class FieldTooLongError extends AuditError {
  constructor(
    readonly field: string,
    readonly length: number,
  ) {
    super("audit field " + field + " is " + length + " chars (> " + MAX_FIELD_CHARS + ")");
    this.name = "FieldTooLongError";
  }
}

/** 非标量值（对象/数组）：元数据只允许标量，防内容夹带。 */
export class NonScalarAuditValueError extends AuditError {
  constructor(readonly field: string) {
    super("audit field " + field + " must be a scalar; objects/arrays are not metadata");
    this.name = "NonScalarAuditValueError";
  }
}

/** reason 缺失 / 空 / 不属于四类。 */
export class InvalidReasonError extends AuditError {
  constructor(message: string) {
    super(message);
    this.name = "InvalidReasonError";
  }
}

/**
 * 校验 reason 非空且属于四类。允许两种形态：`kind` 或 `kind: detail`（detail 非空）。
 * 「routed」这种无法区分四类的一句不合格。
 */
export function validateAuditReason(reason: unknown): ReasonKind {
  if (typeof reason !== "string" || reason.trim() === "") {
    throw new InvalidReasonError("audit reason must be a non-empty string");
  }
  const trimmed = reason.trim();
  const colon = trimmed.indexOf(":");
  const kind = (colon === -1 ? trimmed : trimmed.slice(0, colon)).trim();
  if (!(REASON_KINDS as readonly string[]).includes(kind)) {
    throw new InvalidReasonError(
      'audit reason must distinguish one of ' + REASON_KINDS.join(" | ") + ' (got "' + reason + '")',
    );
  }
  if (colon !== -1 && trimmed.slice(colon + 1).trim() === "") {
    throw new InvalidReasonError("audit reason detail must be non-empty when a kind prefix is used");
  }
  return kind as ReasonKind;
}

/** 输入记录（字段名即白名单，值只能是标量）。 */
export type AuditInput = Record<string, unknown>;

export type AuditScalar = string | number | boolean | null;
export type AuditRecord = Partial<Record<AuditField, AuditScalar>> & { reason: string };

/**
 * 审计写入器：白名单 + 黑名单 + 长度上限 + reason 校验 + tenant 作用域。
 * 任何一条不过即抛错，**不写任何东西**。
 */
export class AuditWriter {
  constructor(private readonly store: TenantStore) {}

  write(ctx: TenantContext | undefined, input: AuditInput): TenantScopedRecord {
    if (ctx === undefined || ctx.tenant_id.trim() === "") {
      throw new TenantScopeError("audit writes require a tenant context (tenant hard isolation)");
    }
    if (typeof input !== "object" || input === null || Array.isArray(input)) {
      throw new AuditError("audit record must be an object");
    }

    const record: Partial<Record<AuditField, AuditScalar>> = {};
    for (const [field, value] of Object.entries(input)) {
      // ① 黑名单先于白名单：命中内容字段名直接拒绝（不静默清洗）。
      if (CONTENT_FIELD_BLACKLIST.has(field.toLowerCase())) {
        throw new ContentLeakError(field);
      }
      if (!AUDIT_FIELD_SET.has(field)) {
        throw new UnknownAuditFieldError(field);
      }
      if (value === undefined) continue;
      if (value !== null && typeof value === "object") {
        throw new NonScalarAuditValueError(field);
      }
      // ② 单字段 500 字符上限。
      if (typeof value === "string" && value.length > MAX_FIELD_CHARS) {
        throw new FieldTooLongError(field, value.length);
      }
      record[field as AuditField] = value as AuditScalar;
    }

    // reason 必须非空且能区分四类。
    validateAuditReason(record.reason);

    // tenant 作用域：记录里的 tenant_id 必须与上下文一致，且最终以上下文为准。
    if (record.tenant_id !== undefined && record.tenant_id !== ctx.tenant_id) {
      throw new TenantScopeError("audit tenant_id must match the tenant context");
    }
    record.tenant_id = ctx.tenant_id;

    // ts_utc 存 UTC epoch；缺省补当前时间。
    if (record.ts_utc === undefined) {
      record.ts_utc = Date.now();
    } else if (typeof record.ts_utc !== "number" || !Number.isFinite(record.ts_utc)) {
      throw new AuditError("ts_utc must be a finite UTC epoch number");
    }

    const requestId = record.request_id;
    const id =
      typeof requestId === "string" || typeof requestId === "number"
        ? String(requestId)
        : String(record.ts_utc);

    return this.store.put(ctx, "audit", id, record);
  }
}
