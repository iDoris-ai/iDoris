// 手写薄封装：JSON Schema 生成的 zod + 生成器表达不了的约束。
// 真源：packages/contracts/schema/tenant.schema.json；产物：src/generated/tenant.ts。
// ICU/时区白名单与 tenant_id 非空白是运行时/环境相关的约束，JSON Schema 只能标注 format/pattern。
import { z } from "zod";
import { deployModeSchema } from "./generated/deploy-mode.js";
import { tenantContextSchema as generated } from "./generated/tenant.js";

export { deployModeSchema };
export type DeployMode = z.infer<typeof deployModeSchema>;

/** 预算作用域（contract-tenancy §4）。 */
export const budgetScopeSchema = z.enum(["paid_only", "all"]);

/** tzdata 白名单：canonical 时区 + UTC。legacy 名（EST5EDT/GMT0）与数字偏移不在其中。 */
const IANA_TIME_ZONES: ReadonlySet<string> = new Set<string>([
  ...Intl.supportedValuesOf("timeZone"),
  "UTC",
]);

/**
 * 显式 IANA 时区名校验。
 * - 拒绝任何以 + / - / U+2212 开头的数字偏移；
 * - 只接受 tzdata canonical 名单（+UTC）；拒 legacy 无斜杠别名（EST5EDT、GMT0）。
 * 不依赖 ICU 的宽松解析，因此不随宿主机 ICU 版本变化。
 */
export function isIanaTimeZone(value: string): boolean {
  if (value.length === 0) return false;
  const first = value[0];
  if (first === "+" || first === "-" || first === "−") return false;
  return IANA_TIME_ZONES.has(value);
}

export const tenantContextSchema = generated.superRefine((value, ctx) => {
  if (!isIanaTimeZone(value.billing_timezone)) {
    ctx.addIssue({ code: "custom", path: ["billing_timezone"], message: "billing_timezone must be an explicit IANA time zone name" });
  }
  if (value.tenant_id.trim().length === 0) {
    ctx.addIssue({ code: "custom", path: ["tenant_id"], message: "tenant_id must not be blank" });
  }
});

export type TenantContext = z.infer<typeof tenantContextSchema>;
