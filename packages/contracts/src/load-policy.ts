// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型。
// 真源：packages/contracts/schema/load-policy.schema.json；产物：src/generated/load-policy.ts。
import type { z } from "zod";
import { loadPolicySchema } from "./generated/load-policy.js";

export { loadPolicySchema };
export type LoadPolicy = z.infer<typeof loadPolicySchema>;
