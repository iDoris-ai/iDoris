// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型。
// 真源：packages/contracts/schema/training-sample.schema.json；产物：src/generated/training-sample.ts。
import type { z } from "zod";
import { trainingSampleSchema } from "./generated/training-sample.js";

export { trainingSampleSchema };
export type TrainingSample = z.infer<typeof trainingSampleSchema>;
