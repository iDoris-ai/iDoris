import { taskProfileSchema, type DeployMode, type TaskProfile } from "@idoris/contracts";

export type HeaderBag = Record<string, string | string[] | undefined>;

export interface ProfileParseResult {
  profile: TaskProfile;
  tenantId?: string;
}

/** 控制面/租户相关的可预期错误（映射为 HTTP 状态 + 错误码）。 */
export class ProfileError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ProfileError";
  }
}

export function currentDeployMode(): DeployMode {
  return process.env.IDORIS_DEPLOY_MODE === "tenant" ? "tenant" : "personal";
}

const header = (headers: HeaderBag, name: string): string | undefined => {
  const v = headers[name] ?? headers[name.toLowerCase()];
  const s = Array.isArray(v) ? v[0] : v;
  return s === undefined || s === "" ? undefined : s;
};

/**
 * 控制面 header → TaskProfile（T1.3.2）。走 header 不走 prompt。
 * - **缺省 privacy = local_only**（保守默认）；
 * - 非法值 → 400；
 * - `deploy_mode=tenant` 时缺 `X-iDoris-Tenant` → 400 tenant_missing，**不回落默认租户**。
 */
export function parseProfile(headers: HeaderBag, deployMode: DeployMode = currentDeployMode()): ProfileParseResult {
  const privacy = header(headers, "x-idoris-privacy") ?? "local_only";
  if (privacy !== "local_only" && privacy !== "any") {
    throw new ProfileError(400, "invalid_privacy", "X-iDoris-Privacy must be local_only|any");
  }
  const intent = header(headers, "x-idoris-intent") ?? "chat";
  const complexity = header(headers, "x-idoris-complexity") ?? "simple";
  const capsRaw = header(headers, "x-idoris-capabilities");
  const capabilities = capsRaw ? capsRaw.split(",").map((s) => s.trim()).filter(Boolean) : ["chat"];
  const fallback = header(headers, "x-idoris-fallback");

  const parsed = taskProfileSchema.safeParse({
    privacy,
    intent,
    complexity,
    capabilities,
    ...(fallback === undefined ? {} : { fallback }),
  });
  if (!parsed.success) {
    throw new ProfileError(400, "invalid_header", parsed.error.issues.map((i) => i.path.join(".") + ": " + i.message).join("; "));
  }

  if (deployMode === "tenant") {
    const tenantId = header(headers, "x-idoris-tenant");
    if (tenantId === undefined) {
      throw new ProfileError(400, "tenant_missing", "deploy_mode=tenant requires X-iDoris-Tenant; no default tenant fallback");
    }
    return { profile: parsed.data, tenantId };
  }
  return { profile: parsed.data };
}
