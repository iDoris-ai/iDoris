# M5 tenant virtual-key boundary

Tenant authority comes from a server-stored virtual-key binding. `owner`,
`budget_ref`, request metadata and an unauthenticated tenant header confer no
tenant authority. This is a safety repair, independent of Admin v1 writes.

## Storage and issuance

Migration 0002 adds nullable `virtual_keys.tenant_id` transactionally. Existing
keys stay unbound; never infer a binding from an owner or request. Missing
`tenant_id` in older serialized scopes deserializes as `None`. Personal mode
continues accepting unbound keys. Tenant mode requires an explicitly bound key.

Use local issuance with the tenant administrator's chosen identifier:

```sh
printf '%s\n' '{"owner":"agent24","tenant_id":"acme","allowed_privacy":["local_only"],"allowed_roles":["fast"]}' | idoris key issue --spec-stdin
```

The plaintext key is returned once; SQLite persists only its verifier. Tenant
identifiers must be nonempty, unpadded, control-free and at most 128 UTF-16 units.

## HTTP enforcement (next slice)

The shipped binary installs the authenticator on all data-plane endpoints.
Except `/health`, tenant requests need a valid bound bearer. Tenant-scoped
handlers require exactly one header equal to the stored binding. Absent headers
retain existing body/profile error precedence and confer no tenant audit scope.
Unbound/mismatched keys receive generic
403 `VIRTUAL_KEY_TENANT_FORBIDDEN`; invalid/missing bearers receive generic 401.
DEV_NO_KEY does not bypass tenant authentication. Existing tenant URL/header
checks still prevent a bound tenant from querying a different tenant's path.

Authenticate and enforce scope before `request.received`. Audit middleware
uses the authenticated tenant marker, so denied tenant requests create no
victim-tenant rows. Tenant library mode also refuses an absent authenticator. Authorized routing
fixtures use fresh bound keys through a cfg(test)-only helper; authentication
negative tests call build_app directly. Personal library fixtures retain legacy
behavior. Production startup always installs an authenticator.

Acceptance: legacy DB migration/reopen, bound-key issuance, correct-tenant
execution, cross-tenant/unbound/anonymous/duplicate-header rejection, zero
upstream calls and zero victim-tenant Event Log/legacy audit rows.

Explicit binding also limits a key after a deployment-mode change: personal
mode accepts unbound keys or an explicit personal binding, never a foreign
tenant binding. An explicit personal key rejects foreign/duplicate headers.
