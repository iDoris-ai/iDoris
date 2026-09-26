// GENERATED FROM packages/contracts/schema/tenant.schema.json — do not edit by hand.
import { z } from "zod"

export const tenantContextSchema = z.object({ "tenant_id": z.string().regex(new RegExp("\\S")).min(1), "budget": z.object({ "limit_minor": z.number().int().gte(0), "spent_minor": z.number().int().gte(0), "scope": z.enum(["paid_only","all"]).default("paid_only") }).strict(), "billing_timezone": z.string().regex(new RegExp("\\S")).min(1), "quota": z.object({ "rpm": z.number().int().gte(1).optional(), "tpm": z.number().int().gte(1).optional() }).strict().optional() }).strict()

