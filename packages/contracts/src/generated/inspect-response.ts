// GENERATED FROM packages/contracts/schema/inspect-response.schema.json — do not edit by hand.
import { z } from "zod";

export const inspectResponseSchema = z.object({ "sensitivity": z.enum(["S0","S1","S2","S3"]), "findings": z.array(z.object({ "kind": z.enum(["cn_resident_id","cn_mobile","bank_card","cn_unified_social_credit_code","cn_license_plate","cn_passport","hk_identity_card","hk_macau_travel_permit","email_address","api_key","private_key","high_entropy_secret","credential_path","input_limit_exceeded"]), "span": z.object({ "start": z.number().int().gte(0), "length": z.number().int().gte(1) }).strict().optional() }).strict()), "action": z.enum(["BLOCK","REDACT","ALLOW"]) }).strict()
  .refine((value) => value.findings.every((finding) =>
    finding.kind !== "input_limit_exceeded" || finding.span === undefined),
    "unscanned input must not carry a span")
  .refine((value) => value.action === "BLOCK" ||
    value.findings.every((finding) => finding.kind !== "input_limit_exceeded"),
    "unscanned input requires BLOCK");

