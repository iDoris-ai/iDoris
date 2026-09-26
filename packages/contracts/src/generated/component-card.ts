// GENERATED FROM packages/contracts/schema/component-card.schema.json — do not edit by hand.
import { z } from "zod"

export const componentCardSchema = z.object({ "provider": z.object({ "id": z.string().min(1), "family": z.enum(["idoris","claude","openai","local","other"]), "tier": z.enum(["local","remote","lora"]), "capabilities": z.array(z.enum(["chat","reasoning","vision","asr","tts","coding","embedding","rerank"])).min(1), "privacy_class": z.enum(["local_only","any"]), "cost": z.object({ "input_per_m": z.number().gte(0), "output_per_m": z.number().gte(0) }).strict(), "locality": z.enum(["loopback","lan","remote"]), "extensions": z.record(z.string(), z.any()).optional() }).strict(), "form": z.enum(["http_service","spawn_cli","bundled_binary","nostr_node","mitm_proxy","batch_job"]), "endpoint": z.string().min(1), "version_pin": z.string().min(1), "privacy_class": z.enum(["local_only","any"]), "allowed_egress": z.array(z.enum(["none","loopback","lan","internet"])).min(1), "fallback_policy": z.enum(["fail_closed","next_in_chain"]), "fail_closed": z.boolean(), "load_policy": z.object({ "mode": z.enum(["resident","on_demand","evict_to_load"]), "keepalive": z.any().superRefine((x, ctx) => {
    const schemas = [z.object({ "pinned": z.boolean() }).strict(), z.object({ "idle_ttl_s": z.number().int().gte(1) }).strict()];
    const { errors, failed } = schemas.reduce<{
      errors: z.core.$ZodIssue[];
      failed: number;
    }>(
      ({ errors, failed }, schema) =>
        ((result) =>
          result.error
            ? {
                errors: [...errors, ...result.error.issues],
                failed: failed + 1,
              }
            : { errors, failed })(
          schema.safeParse(x),
        ),
      { errors: [], failed: 0 },
    );
    const passed = schemas.length - failed;
    if (passed !== 1) {
      ctx.addIssue(errors.length ? {
        path: [],
        code: "invalid_union",
        errors: [errors],
        message: "Invalid input: Should pass single schema. Passed " + passed,
      } : {
        path: [],
        code: "custom",
        errors: [errors],
        message: "Invalid input: Should pass single schema. Passed " + passed,
      });
    }
  }), "admission": z.enum(["coexist","requires_eviction"]) }).strict().optional(), "extensions": z.record(z.string(), z.any()).optional() }).strict()

