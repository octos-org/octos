//! Test-only tracing initialization shared by crates that capture logs with
//! thread-default subscribers. Enabled only through the `test-tracing` feature
//! on dev-dependencies; production builds do not install a subscriber.

use std::sync::Once;

static INSTALL_GLOBAL_SINK: Once = Once::new();

/// Keep every callsite interested even when a parallel test fires it on a
/// thread with no local subscriber.
///
/// `tracing` caches callsite interest across dispatchers. A subscriber-less
/// sibling can otherwise cache `never` after a capture test's last rebuild,
/// so rebuilding/replaying remains racy. The process-global TRACE subscriber
/// is a sink for otherwise-unowned events; thread-default capture subscribers
/// still receive their own events and retain their own filters and writers.
/// No test is serialized and no production tracing is changed.
pub fn ensure_test_tracing() {
    INSTALL_GLOBAL_SINK.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .with_writer(std::io::sink)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("test tracing global sink must be installed only once");
    });
    // The first capture may happen after a callsite was already poisoned.
    // With the permanent TRACE dispatch now registered, this rebuild cannot
    // be followed by a subscriber-less sibling storing `never` again.
    tracing::callsite::rebuild_interest_cache();
}
