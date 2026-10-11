# Explicit completion-cap reservations

Resident HTTP chat candidates quote max(1024, request.max_tokens) completion
units before selection and leader reservation. Larger unsigned caps increase
the quote; missing, invalid or smaller caps retain the existing 1024 floor.
Supervisor candidates retain their existing quote and unsupported-field checks.

The request body is preserved. An unaffordable declared cap returns the existing
402 budget_exceeded before any POST, debit or retained reservation. Zero-cost
admission and leader-only reservation keep cached/replay responses usable after
payment. The pricing changes reuse existing currency rounding/overflow checks.

This closes the explicit max_tokens under-reservation case. Prompt estimates
and requests without an explicit output cap retain their legacy assumptions;
it is not a guarantee for arbitrary upstream billing behavior or other fields.
