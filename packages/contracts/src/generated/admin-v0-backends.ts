// GENERATED FROM packages/contracts/schema/admin-v0-backends.schema.json — do not edit by hand.
import { z } from "zod"

export const adminV0BackendsResponseSchema = z.array(z.object({ "provider_id": z.string().min(1), "locality": z.enum(["loopback","lan","remote"]), "form": z.enum(["http_service","spawn_cli","bundled_binary","nostr_node","mitm_proxy","batch_job"]), "lifecycle_runtime_bound": z.boolean() }).strict())

