# Explicit completion-cap reservations

Resident HTTP chat candidates quote
`max(1024, request.max_tokens, request.max_completion_tokens)` completion units
before selection and leader reservation. Missing or explicit null fields are
absent for quotation; smaller valid caps retain the 1024 floor. With two valid
non-null fields, the larger cap sets the quote regardless of upstream precedence.

Both fields are validated globally after profile/authentication and before
correlation parsing, candidate selection, or backend-specific parameter checks.
Each non-null value must be an unsigned JSON integer no greater than 2^53.
Floats (including integral floats), numeric strings, negative numbers, booleans,
arrays, objects, and larger integers return 400 `invalid_completion_cap` before
any POST or charge. A valid or null sibling never masks an invalid non-null value.
This preflight also applies to free, no-ledger, and Supervisor routes: invalid
non-null caps take precedence over Supervisor stream/unsupported-field errors
and do not receive the selected backend locality header. Supervisor candidates
retain their fixed reservation quote; valid integer and null caps reach their
existing unsupported-field checks. Null quotation compatibility does not make
these options supported by Supervisor execution.

The request body, including null fields, is preserved. An unaffordable declared
cap returns the existing 402 `budget_exceeded` before any POST, debit, or retained
reservation. Zero-cost admission and leader-only reservation keep cached/replay
responses usable after payment. Pricing reuses existing floating-point currency
rounding and overflow checks. The 2^53 bound prevents loss when converting the
integer cap itself to f64; it does not guarantee exact arithmetic for arbitrary
rates, input/output addition, or currency conversion.

This covers explicit `max_tokens` and `max_completion_tokens` reservation sizing.
Prompt estimates and requests without a non-null output cap retain their legacy
assumptions. The output multiplier `n` is not included in the quote and remains
a tracked follow-up: quote cap times n or reject unsupported multiplicity for paid
resident requests. This is not a guarantee for arbitrary upstream billing behavior
or other fields.
