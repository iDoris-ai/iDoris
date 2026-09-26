// GENERATED FROM packages/contracts/schema/training-sample.schema.json — do not edit by hand.
import { z } from "zod"

export const trainingSampleSchema = z.object({ "sample_id": z.string().min(1), "group_id": z.string().min(1).optional(), "data_class": z.enum(["synthetic","anonymized","real"]), "source": z.object({ "kind": z.enum(["synthetic_seed","refined_from_synthetic","human_reviewed"]), "generator": z.string().min(1).optional(), "intent": z.string().min(1).optional() }).strict(), "messages": z.array(z.object({ "role": z.enum(["system","user","assistant"]), "content": z.string().min(1) }).strict()).min(2) }).strict()

