import { z } from "zod";
import { loadModeSchema } from "./load-policy.js";
import { capabilitySchema, privacyClassSchema, tierSchema } from "./provider.js";

const conditionSchema = z
  .object({
    privacy: privacyClassSchema.optional(),
    intent: z.string().min(1).optional(),
    complexity: z.enum(["simple", "complex"]).optional(),
    capabilities: z.array(capabilitySchema).optional(),
  })
  .strict();

const thenSchema = z
  .object({
    tiers: z.array(tierSchema).optional(),
    fail_closed: z.boolean().optional(),
    capability: capabilitySchema.optional(),
    load: loadModeSchema.optional(),
  })
  .strict();

export const routingRuleSchema = z.object({ if: conditionSchema, then: thenSchema }).strict();

/** RoutingPolicy：声明式、版本化；规则按序匹配、首条命中即用；default 必填（spec.md）。 */
export const routingPolicySchema = z
  .object({
    routing_policy: z
      .object({
        version: z.number().int().positive(),
        rules: z.array(routingRuleSchema),
        default: thenSchema,
      })
      .strict(),
  })
  .strict();

export type RoutingPolicy = z.infer<typeof routingPolicySchema>;
