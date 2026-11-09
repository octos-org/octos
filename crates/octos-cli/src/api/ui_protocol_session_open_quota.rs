// R-05 (robust audit): `session/open` live-forwarder admission quota.
// Extracted from `ui_protocol_transport.rs` to keep the transport under
// the quality ratchet; the items below live in the same module via
// `include!`.

/// R-05 (robust audit): every `session/open` used to install a forwarder
/// task + map entry + durable ledger append with NO per-connection or
/// process-wide quota — the ledger's own 1024-session LRU never evicts a
/// forwarder whose receiver is still alive, so a client opening distinct
/// sessions grew tasks, memory and disk without bound. These caps bound
/// the fan-out a single connection (and the whole process) can hold.
/// 128 distinct open sessions per connection is far past any real client
/// (the dashboard opens a handful); 4096 process-wide bounds the
/// multi-tenant sum.
const MAX_LIVE_FORWARDERS_PER_CONNECTION: usize = 128;
const MAX_LIVE_FORWARDERS_GLOBAL: usize = 4096;
static LIVE_FORWARDERS_GLOBAL: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// R-05: RAII guard for one process-wide forwarder slot. Held by the
/// forwarder task; released when the task ends (including abort). The
/// counter reference is `'static` so tests can substitute their own
/// (leaked) counter instead of the process-global one.
struct GlobalLiveForwarderPermit(&'static std::sync::atomic::AtomicUsize);

impl GlobalLiveForwarderPermit {
    fn try_acquire(counter: &'static std::sync::atomic::AtomicUsize) -> Option<Self> {
        let mut current = counter.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            if current >= MAX_LIVE_FORWARDERS_GLOBAL {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self(counter)),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for GlobalLiveForwarderPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// R-05 admission decision for one `session/open`.
enum ForwarderAdmission {
    /// A forwarder may be installed; the permit holds the global slot
    /// until the forwarder task ends.
    Admitted(GlobalLiveForwarderPermit),
    /// Refused: the per-connection or process-wide cap is reached.
    Refused(&'static str),
}

/// R-05: reap finished forwarders (their entries otherwise linger until a
/// reopen or connection cleanup) and decide whether this open may install
/// a forwarder. `reopening` is `true` when the caller already retired the
/// previous forwarder for this same session above (abort + await, which
/// also released its global slot). The counter is injectable so tests do
/// not share the process-global one.
async fn admit_live_forwarder(
    forwarders: &SharedLiveForwarders,
    session_id: &SessionKey,
    reopening: bool,
) -> ForwarderAdmission {
    admit_live_forwarder_with_counter(
        forwarders,
        session_id,
        reopening,
        &LIVE_FORWARDERS_GLOBAL,
    )
    .await
}

async fn admit_live_forwarder_with_counter(
    forwarders: &SharedLiveForwarders,
    session_id: &SessionKey,
    reopening: bool,
    counter: &'static std::sync::atomic::AtomicUsize,
) -> ForwarderAdmission {
    let (raced, at_cap) = {
        let mut map = forwarders.lock().await;
        map.retain(|_, handle| !handle.is_finished());
        // Race guard: a concurrent open could have reinstalled this
        // session's forwarder between the caller's retire and this
        // admission; remove it here and retire it the same way below.
        let raced = map.remove(session_id);
        (raced, map.len() >= MAX_LIVE_FORWARDERS_PER_CONNECTION)
    };
    let replacing = reopening || raced.is_some();
    if let Some(existing) = raced {
        existing.abort();
        let _ = existing.await;
    }
    if !replacing && at_cap {
        return ForwarderAdmission::Refused("connection");
    }
    // A reopen replaces a forwarder whose global slot was already released
    // when the retired task ended, so taking a fresh permit cannot
    // double-count — while SKIPPING the acquire on reopens would leak the
    // counter downward (the replacement would run holding no slot, and
    // reopen churn would silently disable the process-wide cap). At the
    // exact cap boundary a reopen can therefore be refused transiently.
    match GlobalLiveForwarderPermit::try_acquire(counter) {
        Some(permit) => ForwarderAdmission::Admitted(permit),
        None => ForwarderAdmission::Refused("global"),
    }
}
