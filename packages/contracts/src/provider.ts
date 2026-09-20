// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型。
// 真源：packages/contracts/schema/provider.schema.json；产物：src/generated/provider.ts。
import type { z } from "zod";
import { providerDescriptorSchema } from "./generated/provider.js";

export { providerDescriptorSchema };
export type ProviderDescriptor = z.infer<typeof providerDescriptorSchema>;
