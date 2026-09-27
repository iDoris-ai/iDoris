// 手写薄封装：JSON Schema 生成的 zod + 一处生成器不支持的约束。
// 真源：packages/contracts/schema/routing-policy.schema.json；产物：src/generated/routing-policy.ts。
// `minProperties: 1`（then/default 至少一个字段）json-schema-to-zod 不生成，故在此补等价 refine；
// Ajv 侧仍由 minProperties 强制，tests/contract/parity.test.ts 断言两侧判定一致。
import type { z } from "zod";
import { routingPolicySchema as generated } from "./generated/routing-policy.js";

export const routingPolicySchema = generated.superRefine((value, ctx) => {
  const rp = value.routing_policy;
  if (Object.keys(rp.default).length === 0) {
    ctx.addIssue({ code: "custom", path: ["routing_policy", "default"], message: "default must set at least one field" });
  }
  rp.rules.forEach((rule, i) => {
    if (Object.keys(rule.then).length === 0) {
      ctx.addIssue({ code: "custom", path: ["routing_policy", "rules", i, "then"], message: "then must set at least one field" });
    }
  });
});

export type RoutingPolicy = z.infer<typeof routingPolicySchema>;
