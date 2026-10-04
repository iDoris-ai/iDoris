# Recommender parity fixtures

These fixtures are reference outputs from the TypeScript recommender. They are data/test truth for the Rust port; they are not hand-authored Rust expectations.

## `ram.json`

Generated from the real `config/catalog.yaml` and TypeScript `recommend()` implementation for injected M4 host facts at 8/16/24/32/64/128GB RAM.

- 24GB stores the complete `Recommendation` because it is the main acceptance scenario.
- Other RAM tiers store an explicit projection: resident label and budgets, temp admissions/quant labels, blocked ids, and wired-limit recommendation.
- The fixture embeds the source commit and catalog SHA256. Update those fields only when intentionally regenerating the fixture from a changed TypeScript/catalog truth source.
- `packages/recommender/tests/parity-ram.test.ts` recomputes all six cases and contains single-field negative controls so a quant/budget drift cannot silently pass.

Generation source at this checkpoint:

- recommender source commit: `49a66e86e65e9ddf26f3bf3c9d68041df49d8981`
- catalog source commit: `49a66e86e65e9ddf26f3bf3c9d68041df49d8981`
- `config/catalog.yaml` SHA256: `5a49fe4e27dab1bca020479c956de7fc874cf6c856502617ca84a05304b4988a`

## `edges.json`

Generated from the same TypeScript recommender using deliberately minimal catalogs. It locks exact-tie order, empty candidates, RAM/quality/resident-budget equality boundaries and one-input negative contrasts, normal/unknown/oversized overrides, and the current `requires_eviction` result for a temp model that does not fit even when alone. Each scenario stores the full recommendation so warnings/tradeoff text is fixed without copying the production catalog.
