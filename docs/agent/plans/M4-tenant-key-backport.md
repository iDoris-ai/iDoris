# M4 tenant-key security backport

The M4 candidate still has tenant mode without a virtual-key tenant binding.
Land this repair through small, independently reviewed slices so the security
fix can be considered with M4 acceptance. Source: #463/#465/#467/#468, already
validated on the M5 stack; this backport needs its own final-head validation.

1. Persist optional `tenant_id` with migration 0002 and accept an explicit
   binding in the existing local key-issuance CLI. Legacy rows stay unbound;
   never infer authority from `owner` or `budget_ref`.
2. Adapt conformance seeding and tenant routing fixtures to use real bound
   keys. Preserve negative tests using the actual app without fixture injection.
3. Require the authenticated binding to match the unique tenant header before
   tenant routing/events. Tenant mode without an authenticator fails closed.
   Audit attribution must use authenticated tenant authority.
4. Revalidate cross-tenant, anonymous, unbound, malformed-header and revoked
   cached-replay requests, with zero unauthorized egress/tenant writes. Run all
   repository gates and release Rust conformance on the final combined head.

The first slice supplies storage/issuance only; runtime authority enforcement
is not complete until the later slices land. Existing tenant operators must
reissue keys with an explicit binding before adopting enforcement. M4 is still
pending independent approvals, main acceptance and the release procedure.
