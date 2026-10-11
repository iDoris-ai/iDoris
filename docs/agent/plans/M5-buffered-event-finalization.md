# Buffered Event Log finalization

## Ordering

For a newly forwarded buffered request, run the synchronous success gate,
then the asynchronous completion hook, then publish the cache/replay response.
Cache hits and singleflight waiters do not repeat either hook.

## Failure and cancellation

A completion-hook failure returns its error and retains a generic 503 marker.
The same cache key cannot execute another POST within the idempotency window;
a changed payload conflicts. Cancellation while the hook is pending retains
that marker too. Calls without request IDs fail before returning success but
have no cross-request deduplication guarantee.

## Follow-up

This transport slice preserves existing endpoint behavior through no-op hooks.
Wire protocol endpoints next. Preserve the existing paid-chat settlement rule
until its Event Log failure semantics receive an explicit review.
