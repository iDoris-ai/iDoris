# Backend model bindings for role requests

This is an iDoris-internal trusted configuration boundary for lifecycle-managed
and resident HTTP backends. It is not a cross-repository model-selection API.
Only the operator or trusted provisioner may write these bindings; request bodies
and caller headers cannot configure them.

The Rust router accepts optional `extensions.idoris_role_models` on HTTP component
cards, mapping role names to verified installed model IDs understood by that
card's backend. For example, with synthetic IDs:

```yaml
extensions:
  _degradation: "Removing these bindings disables role aliases on this backend."
  idoris_role_models:
    daily: installed-chat-model-id
    vision: installed-vision-model-id
    auto: installed-chat-model-id
```

Replace the example IDs with independently verified installed backend IDs. No binding is
shipped for oMLX: iDoris cannot infer which weights the user installed. Do not copy
recommender catalog IDs into these values without verifying the backend identity.
The static recommendation catalog describes families, roles, quantization, and
hardware eligibility; it has no provider/endpoint to installed-model-ID join.
This extension supplies that missing join without changing the portable card
schema or asserting new catalog facts. The operator attests role suitability;
existing component capability, privacy, cost, and admission gates still apply.

Keys are fast, daily, deep, vision, embed, rerank, decide, or auto. Values must be
nonblank strings, with no surrounding whitespace or control characters, and must
be concrete IDs rather than another `idoris/` alias. Malformed bindings fail
startup. Removing the extension leaves concrete requests available but makes role
requests unavailable on that card. Cards supplied programmatically also fail
closed for malformed or missing role bindings. `_degradation` is required by the
existing extension registration rule. Programmatic cards with invalid bindings
are excluded from selection, including concrete requests; a valid HTTP card with
no bindings still accepts explicit concrete overrides.

`idoris_role_models` is explicitly unsupported on the `subscription` provider,
including an empty mapping. Startup rejects it, and programmatic cards carrying
it fail closed. Subscription execution retains the authorized relay's fixed
identity; arbitrary mapping targets are never forwarded into its sandbox.

Only cards explicitly bound for the requested role enter selection. There is no
capability-only fallback to another role and no first-model selection. Existing
policy ordering chooses among eligible configured backends. `auto` also requires
an explicit target; it is a routing default, never a static catalog model role.
An explicit concrete request model remains unchanged. Readiness affinity and
lifecycle load/chat resolve the same exact ID; lifecycle catalog membership and
positive footprint checks run before load or eviction. A process-owned runtime
can supply its existing trusted footprint for its exact configured model.

Resident HTTP services receive the resolved concrete model in buffered and SSE
requests. Their residency/availability remains owned by the external service;
upstream failure is returned without retrying another role or backend. Binding
configuration does not prove the resident backend has weights installed, nor does
it add a new download, catalog refresh, or hardware admission authority.

Release integration must persist a verified user selection into these bindings
and independently establish model availability. Consuming the current read-only
models/capabilities observations and completing Agent24 integration remain a next
slice. This change supplies dispatch identity; it does not complete installation,
availability discovery, the Agent24 launcher/authentication bridge, or allocation
budget integration.
