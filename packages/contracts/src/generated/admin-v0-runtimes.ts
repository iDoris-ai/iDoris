// GENERATED FROM packages/contracts/schema/admin-v0-runtimes.schema.json — do not edit by hand.
import { z } from "zod"

export const adminV0RuntimesResponseSchema = z.object({ "runtimes": z.array(z.any().superRefine((x, ctx) => {
    const schemas = [z.object({ "provider_id": z.string().min(1), "state": z.literal("observed"), "pressure": z.enum(["ok","soft","hard","ceiling","unknown"]), "used_gb": z.number().gte(0), "model_memory_max_gb": z.number().gte(0), "loaded": z.array(z.string().min(1)) }).strict(), z.object({ "provider_id": z.string().min(1), "state": z.literal("error") }).strict()];
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
  })) }).strict()

