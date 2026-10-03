# OpenAI-compatible model identity

The iDoris × Agent24 interface treats `idoris/<role>` as the stable request
contract. Concrete model IDs are informational and may change with routing.
For Supervisor-backed responses, the OpenAI `model` field therefore comes
from the backend's `ChatResponse.model`, which identifies the model that
actually answered.

When a caller sends a concrete model ID on this path, iDoris accepts it only
when it matches the selected card's `provider.id`. A mismatch returns HTTP
400 with `error.type=unsupported_field`, `reason_code=unsupported_model`,
and guidance to use a role alias or the selected provider ID. Selection runs
first, so this error includes `X-iDoris-Served-Locality`. A present `model`
that is empty, null, or not a string also returns 400 on the Supervisor path.
This check does not
change Resident `http_service` proxy behavior: the upstream still interprets
its model field, including fixture IDs used by conformance tests.
