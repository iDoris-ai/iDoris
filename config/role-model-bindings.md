# Backend model bindings for role requests

Proposed contract implemented on this local branch; canonical iDoris/Agent24
design review must accept this configuration boundary before release.

The Rust router accepts optional `extensions.idoris_role_models` on each component
card. It is a trusted operator configuration boundary, mapping role names to exact
model IDs understood by that card's backend. For example, with synthetic IDs:

```yaml
extensions:
  _degradation: "Removing these bindings disables role aliases on this backend."
  idoris_role_models:
    daily: installed-chat-model-id
    vision: installed-vision-model-id
    auto: installed-chat-model-id
```

Replace the example IDs with independently verified backend IDs. No binding is
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
existing extension registration rule.

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
and independently establish model availability. This change supplies dispatch
identity, not the Agent24 launcher, authentication bridge, or allocation budget
execution contract.
