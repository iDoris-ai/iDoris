// GENERATED FROM packages/contracts/schema/provider.schema.json — do not edit by hand.
import { z } from "zod"

export const providerDescriptorSchema = z.object({ "id": z.string().min(1), "family": z.enum(["idoris","claude","openai","local","other"]), "tier": z.enum(["local","remote","lora"]), "capabilities": z.array(z.enum(["chat","reasoning","vision","asr","tts","coding","embedding","rerank"])).min(1), "privacy_class": z.enum(["local_only","any"]), "cost": z.object({ "input_per_m": z.number().gte(0), "output_per_m": z.number().gte(0) }).strict(), "locality": z.enum(["loopback","lan","remote"]), "extensions": z.record(z.string(), z.any()).optional() }).strict()

