//! Panic firewalls for threads that run user-content work.
//!
//! A panic on **any** thread terminates the process; the editor's workers (image
//! fetch, search/replace, large-file open and index) chew through user content,
//! so a bug there must degrade to "this feature failed", not "the editor
//! vanished".
//!
//! Swallowing is not hiding: the panic hook has already written the full record
//! (thread, source location, payload) to `crash.log` before unwinding starts, and
//! [`crate::panic_log::record_recovered`] appends a `--- recovered` line naming
//! this scope. Grep the log for `recovered` to see what is being papered over.
//!
//! Only use this at thread boundaries: the worker's own state is not rolled
//! back, which is fine because these workers hand results over through
//! channels/atomics and own no UI state.

/// Run `f`, swallowing a panic. `scope` names the call site in `crash.log`.
pub(crate) fn worker(scope: &str, f: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        crate::panic_log::record_recovered(scope, &crate::panic_log::payload_str(&*payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The firewall must keep the caller alive and make the event countable — a
    /// silent `catch_unwind` would turn every worker bug into an invisible failure.
    #[test]
    fn worker_swallows_panics_and_counts_them() {
        let before = crate::panic_log::recovered_count();
        worker("test:noop", || {});
        assert_eq!(
            crate::panic_log::recovered_count(),
            before,
            "a normal run must not count as recovered"
        );
        worker("test:panic", || {
            panic!("intentional: the firewall must absorb this")
        });
        assert_eq!(
            crate::panic_log::recovered_count(),
            before + 1,
            "a swallowed panic must be counted"
        );
    }
}
