// GENERATED FROM packages/contracts/schema/task-profile.schema.json — do not edit by hand.
import { z } from "zod"

export const taskProfileSchema = z.object({ "privacy": z.enum(["local_only","any"]).default("local_only"), "intent": z.string().min(1).default("chat"), "complexity": z.enum(["simple","complex"]).default("simple"), "capabilities": z.array(z.enum(["chat","reasoning","vision","asr","tts","coding","embedding","rerank"])).min(1).default(["chat"]), "fallback": z.enum(["fail_closed","next_in_chain"]).optional() }).strict()

