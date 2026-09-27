import type { TenantContext } from "@idoris/contracts";

export type RecordKind = "usage" | "budget" | "audit";

export interface TenantScopedRecord {
  tenantId: string;
  kind: RecordKind;
  id: string;
  payload: Record<string, unknown>;
}

/** 缺 tenant 上下文的查询直接抛错，而不是返回全量。 */
export class TenantScopeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TenantScopeError";
  }
}

/**
 * 租户硬隔离的数据访问层（T1.5.3）。
 * 用量/预算/审计三类数据的每次读写都必须带 tenant 作用域；缺上下文即抛错。
 * 这是「A 租户查不到 B 租户任何一条」的唯一可靠保证（不是靠调用方记得加 where）。
 */
export class TenantStore {
  private readonly rows: TenantScopedRecord[] = [];

  private assertScope(tenantId: string | undefined): asserts tenantId is string {
    if (tenantId === undefined || tenantId.trim() === "") {
      throw new TenantScopeError("tenant scope is required for every usage/budget/audit access");
    }
  }

  put(ctx: TenantContext | undefined, kind: RecordKind, id: string, payload: Record<string, unknown>): TenantScopedRecord {
    const tenantId = ctx?.tenant_id;
    this.assertScope(tenantId);
    const row: TenantScopedRecord = { tenantId, kind, id, payload };
    this.rows.push(row);
    return row;
  }

  list(ctx: TenantContext | undefined, kind?: RecordKind): TenantScopedRecord[] {
    const tenantId = ctx?.tenant_id;
    this.assertScope(tenantId);
    return this.rows.filter((r) => r.tenantId === tenantId && (kind === undefined || r.kind === kind));
  }

  get(ctx: TenantContext | undefined, kind: RecordKind, id: string): TenantScopedRecord | undefined {
    const tenantId = ctx?.tenant_id;
    this.assertScope(tenantId);
    return this.rows.find((r) => r.tenantId === tenantId && r.kind === kind && r.id === id);
  }
}
