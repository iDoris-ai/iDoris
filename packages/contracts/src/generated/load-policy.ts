// GENERATED FROM packages/contracts/schema/load-policy.schema.json — do not edit by hand.
import { z } from "zod"

export const loadPolicySchema = z.object({ "mode": z.enum(["resident","on_demand","evict_to_load"]), "keepalive": z.any().superRefine((x, ctx) => {
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
  }), "admission": z.enum(["coexist","requires_eviction"]) }).strict()

