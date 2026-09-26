// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型。
// 真源：packages/contracts/schema/task-profile.schema.json；产物：src/generated/task-profile.ts。
import type { z } from "zod";
import { taskProfileSchema } from "./generated/task-profile.js";

export { taskProfileSchema };
export type TaskProfile = z.infer<typeof taskProfileSchema>;
