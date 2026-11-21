// GENERATED FROM packages/contracts/schema/admin-v0-upstream.schema.json — do not edit by hand.
import { z } from "zod"

export const adminV0UpstreamSchema = z.object({ "id": z.string().min(1), "engine_kind": z.string().regex(new RegExp("^[a-z][a-z0-9_]{0,31}$")), "state": z.enum(["ready","starting","unavailable","restarting","failed","stopped","absent"]), "reason": z.enum(["engine_not_found","port_in_use","auth_required","auth_failed","startup_timeout","crash_loop","fence_pending","budget_zero","base_unavailable","startup_latched"]).optional(), "managed": z.boolean(), "owned": z.boolean(), "port": z.number().int().gte(1).lte(65535).optional(), "pid": z.number().int().gte(1).optional(), "restart_required": z.boolean(), "failure_count": z.number().int().gte(0).optional(), "last_failure_at_ms": z.number().int().gte(0).optional() }).strict()

