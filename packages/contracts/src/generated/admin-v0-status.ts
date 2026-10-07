// GENERATED FROM packages/contracts/schema/admin-v0-status.schema.json — do not edit by hand.
import { z } from "zod"

export const adminV0StatusResponseSchema = z.object({ "status": z.literal("ok"), "service": z.literal("idoris"), "version": z.string().min(1), "contract_version": z.string().min(1), "instance_id": z.string().min(1), "components": z.number().int().gte(0), "runtimes": z.number().int().gte(0), "subscriptions": z.number().int().gte(0), "budget_configured": z.boolean(), "audit_configured": z.boolean(), "capacity": z.any().superRefine((x, ctx) => {
    const schemas = [z.object({ "state": z.literal("observed"), "entries": z.array(z.object({ "id": z.string(), "capability": z.string(), "resident": z.boolean(), "estimated_memory_gb": z.number().gte(0), "ctx_limit": z.number().int().gte(0), "queue_depth": z.number().int().gte(0), "admission_status": z.enum(["ready","requires_eviction","blocked"]) }).strict()) }).strict(), z.object({ "state": z.literal("unavailable") }).strict(), z.object({ "state": z.literal("error") }).strict()];
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
  }) }).strict()

