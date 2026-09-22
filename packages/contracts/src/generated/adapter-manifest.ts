// GENERATED FROM packages/contracts/schema/adapter-manifest.schema.json — do not edit by hand.
import { z } from "zod"

export const adapterManifestSchema = z.object({ "adapter_id": z.string().min(1), "base_model_id": z.string().min(1), "base_digest": z.string().regex(new RegExp("^sha256:[a-f0-9]{64}$")), "tokenizer_digest": z.string().regex(new RegExp("^sha256:[a-f0-9]{64}$")), "rank": z.number().int().gte(1).lte(256), "data_class": z.enum(["synthetic","anonymized","real"]), "created_at": z.string().min(1).optional(), "target_modules": z.array(z.string().min(1)).min(1).optional(), "metrics": z.record(z.string(), z.any()).optional() }).strict()

