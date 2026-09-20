import { z } from "zod";
import { fallbackPolicySchema } from "./component-card.js";
import { capabilitySchema, privacyClassSchema } from "./provider.js";

/**
 * 控制面 → 内部表示（06 §10.5 / spec.md）。
 * 保守默认：未声明 privacy 按最严的 local_only 处理。
 */
export const taskProfileSchema = z
  .object({
    privacy: privacyClassSchema.default("local_only"),
    intent: z.string().min(1).default("chat"),
    complexity: z.enum(["simple", "complex"]).default("simple"),
    capabilities: z.array(capabilitySchema).min(1).default(["chat"]),
    fallback: fallbackPolicySchema.optional(),
  })
  .strict();

export type TaskProfile = z.infer<typeof taskProfileSchema>;
