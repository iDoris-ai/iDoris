# OpenAI-compatible streaming support

`POST /v1/chat/completions` has two execution paths:

| Path | `stream` behavior |
| --- | --- |
| Resident `http_service` card forwarded through the proxy | `true` is forwarded as SSE; the upstream stream must end with `[DONE]`. Missing or `false` uses buffered forwarding. |
| on_demand/Supervisor card backed by `RuntimeAdapter` | Missing or `false` returns a buffered completion. `true` is rejected with HTTP 400 and `error.type = unsupported_field`, `reason_code = unsupported_stream`. |

The Supervisor's `RuntimeAdapter::chat` contract returns one complete
`ChatResponse`; it has no incremental event or response-body lifecycle. The
router therefore rejects streaming before reserving budget or asking the
Supervisor to inspect, load, or execute a model. Supporting SSE there needs a
streaming adapter contract that carries cancellation and holds admission
permits through terminal delivery, so it is outside this bounded compatibility
fix. Resident proxy streaming continues to use the existing SSE termination
and cancellation behavior.

The `stream` field is accepted only as a JSON boolean on the Supervisor path:

| Value | Result |
| --- | --- |
| omitted | buffered completion |
| `false` | buffered completion |
| `true` | HTTP 400; retry with `stream: false` |
| `null`, string, number, array, or object | HTTP 400 explaining that `stream` must be a boolean |

This keeps the unsupported request visible to callers. The rejection includes
`X-iDoris-Served-Locality` after a local card has been selected, matching the
route's response contract.
