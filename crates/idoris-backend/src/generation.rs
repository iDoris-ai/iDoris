//! Engine generations: which engine **process** a piece of work was admitted for (IDORIS-CORE S6).
//!
//! A managed engine is replaced (Admin `restart`, `stop` then `start`, an external engine the user
//! restarts) while the Supervisor that fronts it lives on. Credentials follow the engine: the engine
//! manager registers, per endpoint, the key of the process that is there *now* (and bumps the endpoint's
//! generation every time that changes). Work the Supervisor admitted for the old process must therefore
//! never pick up the new process's credentials: an old load that is in an OOM back-off, or an old chat that
//! has not yet written its request, would otherwise send its body to the **new** process, which knows
//! nothing about it (no fence, no ledger entry).
//!
//! The binding is made at **admission**, in the Supervisor's single-writer loop: it asks the adapter for
//! [`RuntimeAdapter::engine_generation`] and runs everything the admitted operation does against the
//! adapter inside [`scoped`]. An adapter that resolves credentials per request reads
//! [`pinned_generation`] at the one place it resolves them and **refuses** (before anything is sent) when
//! the generation it was admitted under is no longer the current one. An adapter that has no notion of
//! generations (`engine_generation() == None`, the default) is untouched.
//!
//! The channel is a task-local, not a parameter, because every `RuntimeAdapter` method would otherwise grow
//! one and every implementation (and test double) with it; the operation's whole adapter flow is awaited in
//! the one task the Supervisor spawned for it (`run_load_flow`, the chat task, the unload task), which is
//! exactly the extent a task-local covers.
//!
//! [`RuntimeAdapter::engine_generation`]: crate::RuntimeAdapter::engine_generation

use std::future::Future;

tokio::task_local! {
    static PINNED: u64;
}

/// The engine generation the current adapter call was admitted under, if the Supervisor bound it to one
/// (see the module docs). `None` outside such an operation, or for an adapter without generations.
pub fn pinned_generation() -> Option<u64> {
    PINNED.try_with(|generation| *generation).ok()
}

/// Runs `future` bound to `generation` (`None` = unbound: it runs as it is).
pub(crate) async fn scoped<F: Future>(generation: Option<u64>, future: F) -> F::Output {
    match generation {
        Some(generation) => PINNED.scope(generation, future).await,
        None => future.await,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn the_generation_is_visible_exactly_inside_the_scope_and_across_awaits() {
        assert_eq!(pinned_generation(), None);
        let seen = scoped(Some(7), async {
            let first = pinned_generation();
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            (first, pinned_generation())
        })
        .await;
        assert_eq!(seen, (Some(7), Some(7)));
        assert_eq!(
            pinned_generation(),
            None,
            "it does not leak out of the scope"
        );
        // `None` binds nothing: an unbound operation is not mistaken for generation 0.
        assert_eq!(scoped(None, async { pinned_generation() }).await, None);
    }

    #[tokio::test]
    async fn a_spawned_task_does_not_inherit_the_generation() {
        // The binding is per task: the Supervisor scopes the future it spawns, not the one it spawns from.
        let inner = scoped(Some(3), async {
            tokio::spawn(async { pinned_generation() }).await.unwrap()
        })
        .await;
        assert_eq!(inner, None);
    }
}
