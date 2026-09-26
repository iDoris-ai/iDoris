// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型。
// 真源：packages/contracts/schema/adapter-manifest.schema.json；产物：src/generated/adapter-manifest.ts。
import type { z } from "zod";
import { adapterManifestSchema } from "./generated/adapter-manifest.js";

export { adapterManifestSchema };
export type AdapterManifest = z.infer<typeof adapterManifestSchema>;
