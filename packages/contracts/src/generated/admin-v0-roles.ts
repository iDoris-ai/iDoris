// GENERATED FROM packages/contracts/schema/admin-v0-roles.schema.json — do not edit by hand.
import { z } from "zod"

export const adminV0RolesResponseSchema = z.array(z.object({ "role": z.string().min(1), "aliases": z.array(z.string().min(1)).min(1), "catalog_role": z.boolean() }).strict())

