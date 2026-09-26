// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型。
// 真源：packages/contracts/schema/component-card.schema.json；产物：src/generated/component-card.ts。
import type { z } from "zod";
import { componentCardSchema } from "./generated/component-card.js";

export { componentCardSchema };
export type ComponentCard = z.infer<typeof componentCardSchema>;
