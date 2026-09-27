import { describe, expect, it } from "vitest";
import type { TenantContext } from "@idoris/contracts";
import { TenantScopeError, TenantStore } from "@idoris/tenancy";
import {
  AUDIT_FIELDS,
  AuditWriter,
  ContentLeakError,
  FieldTooLongError,
  InvalidReasonError,
  MAX_FIELD_CHARS,
  NonScalarAuditValueError,
  UnknownAuditFieldError,
} from "../src/audit.js";
import { REASON_KINDS } from "../src/reason.js";

const ctx = (tenant_id: string): TenantContext => ({
  tenant_id,
  budget: { limit_minor: 1_000_000, spent_minor: 0, scope: "paid_only" },
  billing_timezone: "Asia/Bangkok",
});

/** 合法基线：只含 spec.md 允许的元数据字段。 */
const base = () => ({
  request_id: "req-1",
  component: "router",
  intent: "chat",
  privacy: "local_only",
  tier: "local",
  provider_id: "mock",
  model_id: "mock-small",
  tokens_in: 10,
  tokens_out: 20,
  cost_minor: 0,
  latency_ms: 12,
  status: 200,
  reason: "intent_match",
  ts_utc: 1_700_000_000_000,
});

describe("AuditRecord - 字段穷举白名单（spec.md）", () => {
  it("白名单恰好是 spec 列举的 15 个字段", () => {
    expect([...AUDIT_FIELDS]).toEqual([
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
    ]);
  });

  it("接受完整元数据记录，落盘字段都在白名单内", () => {
    const store = new TenantStore();
    const row = new AuditWriter(store).write(ctx("A"), { ...base(), tenant_id: "A" });
    expect(row.kind).toBe("audit");
    expect(row.tenantId).toBe("A");
    expect(row.payload.reason).toBe("intent_match");
    for (const key of Object.keys(row.payload)) {
      expect(AUDIT_FIELDS).toContain(key);
    }
    expect(store.list(ctx("A"), "audit")).toHaveLength(1);
  });

  it("缺 ts_utc 时补当前 epoch，并用它作为行 id", () => {
    const store = new TenantStore();
    const { request_id: _request_id, ts_utc: _ts_utc, ...rest } = base();
    const row = new AuditWriter(store).write(ctx("A"), rest);
    expect(typeof row.payload.ts_utc).toBe("number");
    expect(row.id).toBe(String(row.payload.ts_utc));
  });

  it("拒绝白名单外的字段名（不静默丢弃）", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), extra_meta: 1 })).toThrow(
      UnknownAuditFieldError,
    );
    expect(store.list(ctx("A"), "audit")).toHaveLength(0);
  });
});

describe("AuditRecord - 内容黑名单（命中即抛错，绝不清洗）", () => {
  const SENTINEL = "SENTINEL_donotlog_9f3a";
  const blacklisted = [
    "prompt",
    "prompts",
    "input",
    "content",
    "text",
    "body",
    "messages",
    "document",
    "file",
    "payload",
  ];

  for (const field of blacklisted) {
    it("拒绝内容字段名 " + field + " 并抛 ContentLeakError", () => {
      const store = new TenantStore();
      const writer = new AuditWriter(store);
      let thrown: unknown;
      try {
        writer.write(ctx("A"), { ...base(), [field]: SENTINEL });
      } catch (err) {
        thrown = err;
      }
      expect(thrown).toBeInstanceOf(ContentLeakError);
      expect((thrown as ContentLeakError).field).toBe(field);
      // 拒绝写入：store 里没有任何一行，哨兵不出现在任何落盘文本里。
      expect(store.list(ctx("A"), "audit")).toHaveLength(0);
      expect(JSON.stringify(store.list(ctx("A")))).not.toContain(SENTINEL);
    });
  }

  it("黑名单比较大小写不敏感（Prompt 同样被拒）", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), Prompt: SENTINEL })).toThrow(
      ContentLeakError,
    );
    expect(store.list(ctx("A"), "audit")).toHaveLength(0);
  });
});

describe("AuditRecord - 单字段 500 字符上限", () => {
  const SENTINEL = "LONGTEXT_donotlog";

  it("恰好 500 字符可以通过", () => {
    const store = new TenantStore();
    const value = SENTINEL + "x".repeat(MAX_FIELD_CHARS - SENTINEL.length);
    expect(value.length).toBe(MAX_FIELD_CHARS);
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), component: value })).not.toThrow();
  });

  it("501 字符被拒绝（FieldTooLongError），且不落盘", () => {
    const store = new TenantStore();
    const value = SENTINEL + "x".repeat(MAX_FIELD_CHARS + 1 - SENTINEL.length);
    expect(value.length).toBe(MAX_FIELD_CHARS + 1);
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), component: value })).toThrow(
      FieldTooLongError,
    );
    expect(store.list(ctx("A"), "audit")).toHaveLength(0);
    expect(JSON.stringify(store.list(ctx("A")))).not.toContain(SENTINEL);
  });

  it("对象 / 数组值被拒绝（元数据只允许标量）", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), status: { note: 1 } })).toThrow(
      NonScalarAuditValueError,
    );
    expect(store.list(ctx("A"), "audit")).toHaveLength(0);
  });
});

describe("AuditRecord - reason 四类与非空", () => {
  for (const kind of REASON_KINDS) {
    it("接受 reason=" + kind, () => {
      const store = new TenantStore();
      const row = new AuditWriter(store).write(ctx("A"), { ...base(), reason: kind });
      expect(row.payload.reason).toBe(kind);
    });
  }

  it("接受 kind: detail 形式", () => {
    const store = new TenantStore();
    const row = new AuditWriter(store).write(ctx("A"), {
      ...base(),
      reason: "privacy_enforced: local_only kept on device",
    });
    expect(row.payload.reason).toContain("privacy_enforced");
  });

  it("拒绝空 reason", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), reason: "   " })).toThrow(
      InvalidReasonError,
    );
  });

  it("拒绝无法区分四类的 routed", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), reason: "routed" })).toThrow(
      InvalidReasonError,
    );
  });

  it("拒绝缺 reason 的记录", () => {
    const store = new TenantStore();
    const { reason: _reason, ...rest } = base();
    expect(() => new AuditWriter(store).write(ctx("A"), rest)).toThrow(InvalidReasonError);
  });

  it("拒绝 kind 后跟空 detail", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), reason: "budget:" })).toThrow(
      InvalidReasonError,
    );
  });
});

describe("AuditRecord - tenant 作用域", () => {
  it("A 的审计 B 一条都看不到", () => {
    const store = new TenantStore();
    const writer = new AuditWriter(store);
    writer.write(ctx("A"), base());
    writer.write(ctx("B"), { ...base(), request_id: "req-2" });
    expect(store.list(ctx("A"), "audit").map((r) => r.id)).toEqual(["req-1"]);
    expect(store.list(ctx("B"), "audit").map((r) => r.id)).toEqual(["req-2"]);
  });

  it("缺 tenant 上下文即抛错（不回落默认租户）", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(undefined, base())).toThrow(TenantScopeError);
  });

  it("记录里的 tenant_id 与上下文不一致即抛错（防跨租户伪造）", () => {
    const store = new TenantStore();
    expect(() => new AuditWriter(store).write(ctx("A"), { ...base(), tenant_id: "B" })).toThrow(
      TenantScopeError,
    );
    expect(store.list(ctx("A"), "audit")).toHaveLength(0);
  });
});
