// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型。
// 真源：packages/contracts/schema/routing-policy.schema.json；产物：src/generated/routing-policy.ts。
import type { z } from "zod";
import { routingPolicySchema } from "./generated/routing-policy.js";

export { routingPolicySchema };
export type RoutingPolicy = z.infer<typeof routingPolicySchema>;
