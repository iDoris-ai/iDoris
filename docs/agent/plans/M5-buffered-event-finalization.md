# Buffered Event Log finalization

## Ordering

For a newly forwarded buffered request, run the synchronous success gate,
then the asynchronous completion hook, then publish the cache/replay response.
Cache hits and singleflight waiters do not repeat either hook.

## Failure and cancellation

A completion-hook failure returns its error and retains a generic 503 marker.
The same cache key cannot execute another POST within the idempotency window;
a changed payload conflicts. Cancellation while the hook is pending retains
the existing generic 502 cancellation marker. Calls without request IDs fail
before returning success but
have no cross-request deduplication guarantee.

## Follow-up

Protocol endpoints and free chat use the pre-publication completion hook.
Paid chat preserves its existing committed-settlement response rule until
its Event Log failure semantics receive an explicit review.

## Protocol attempt terminals

A protocol request whose `dispatched` append committed records `completed`
with failure even when connection establishment fails. This completes the
router attempt; it does not claim that upstream inference ran, and it does
not change `ExecutionDisposition` or charge budget. Breaker/preflight refusals
have no committed dispatch and therefore no completion. The same distinction
applies to a messages stream that returns a buffered transport error.

## Free chat

Free buffered chat uses the same pre-publication completion hook. Append failure
cannot become a cached success. Paid chat retains its existing post-settlement
response rule and is covered by the existing committed-settlement regression.
