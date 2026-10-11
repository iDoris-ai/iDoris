# Paid reservation lifecycle

1. Use zero-cost SpendGate admission for cache hits, replay and waiters.
   A paid candidate still requires a configured ledger.
2. Only the singleflight leader reserves its quoted charge in the pre-send
   observer. Append budget/dispatched facts after that reservation succeeds.
3. Arm retention immediately before the observer permits a send. Before-send
   cancellation releases funds; possible execution retains them until expiry.
4. Settle a successful response once before successful cache publication;
   preserve the existing paid completion-log failure rule and charge receipt.

The first slice builds and tests the coordinator under cfg(test). The next
slice wires it into the router. Explicit completion-cap estimates follow after
that, so a larger quote does not make an already-paid cache reply unaffordable.
