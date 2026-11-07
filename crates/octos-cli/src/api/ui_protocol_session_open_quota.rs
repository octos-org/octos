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

#[cfg(test)]
mod quota_tests {
    use super::*;

#[test]
    fn global_live_forwarder_permit_enforces_process_cap() {
        // R-05: the process-wide slot counter refuses past the cap and the RAII
        // drop hands the slot back.
        let counter: &'static std::sync::atomic::AtomicUsize = Box::leak(Box::new(
            std::sync::atomic::AtomicUsize::new(MAX_LIVE_FORWARDERS_GLOBAL),
        ));
        assert!(
            GlobalLiveForwarderPermit::try_acquire(counter).is_none(),
            "at-cap counter must refuse"
        );
        counter.store(
            MAX_LIVE_FORWARDERS_GLOBAL - 1,
            std::sync::atomic::Ordering::SeqCst,
        );
        let permit = GlobalLiveForwarderPermit::try_acquire(counter).expect("one slot left");
        assert!(GlobalLiveForwarderPermit::try_acquire(counter).is_none());
        drop(permit);
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            MAX_LIVE_FORWARDERS_GLOBAL - 1,
            "dropping the permit releases the slot"
        );
    }
    
    #[tokio::test]
    async fn session_open_admission_caps_live_forwarders_per_connection() {
        // R-05: pre-fix, each distinct session/open installed a forwarder with
        // no bound (audit repro: 1025 opens -> 1025 forwarders while the ledger
        // LRU capped at 1024). The admission gate must refuse past the cap,
        // allow reopens of already-open sessions, and reap finished tasks.
        let forwarders: SharedLiveForwarders =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        for i in 0..MAX_LIVE_FORWARDERS_PER_CONNECTION {
            let task = tokio::spawn(std::future::pending::<()>());
            forwarders
                .lock()
                .await
                .insert(SessionKey(format!("sess-{i}")), task);
        }
        match admit_live_forwarder(&forwarders, &SessionKey("sess-new".into()), false).await {
            ForwarderAdmission::Refused("connection") => {}
            _ => panic!("a new session past the per-connection cap must be refused"),
        }
        // Reopen of an already-open session bypasses the per-connection
        // cap (the retired entry no longer counts) but still takes a fresh
        // global permit — the retired task's slot was released at abort.
        assert!(matches!(
            admit_live_forwarder(&forwarders, &SessionKey("sess-0".into()), true).await,
            ForwarderAdmission::Admitted(_)
        ));
        // A finished forwarder is reaped at admission and frees its slot.
        if let Some(handle) = forwarders.lock().await.get(&SessionKey("sess-0".into())) {
            handle.abort();
        }
        for _ in 0..100 {
            let finished = forwarders
                .lock()
                .await
                .get(&SessionKey("sess-0".into()))
                .map(|h| h.is_finished())
                .unwrap_or(false);
            if finished {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        match admit_live_forwarder(&forwarders, &SessionKey("sess-new".into()), false).await {
            ForwarderAdmission::Admitted(permit) => drop(permit),
            _ => panic!("a reaped slot must admit a new session"),
        }
    }
    #[tokio::test]
    async fn session_open_reopens_keep_the_global_count_consistent() {
        // alan0x review: before the fix, every reopen ran the replacement
        // forwarder with NO global slot (the retired task's permit was
        // released by abort+await, and the Reopen path acquired none), so
        // reopen churn deflated the counter and silently disabled the 4096
        // process cap. After N reopens the count must still equal the number
        // of live forwarders.
        let counter: &'static std::sync::atomic::AtomicUsize =
            Box::leak(Box::new(std::sync::atomic::AtomicUsize::new(0)));
        let forwarders: SharedLiveForwarders =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let key = SessionKey("reopen-me".into());
        for round in 0..8 {
            let permit = match admit_live_forwarder_with_counter(
                &forwarders,
                &key,
                /* reopening = */ round > 0,
                counter,
            )
            .await
            {
                ForwarderAdmission::Admitted(permit) => permit,
                ForwarderAdmission::Refused(scope) => panic!("reopen {round} refused: {scope}"),
            };
            let task = tokio::spawn(async move {
                let _permit = permit;
                std::future::pending::<()>().await
            });
            // Mirror handle_session_open: retire the previous forwarder BEFORE
            // installing the replacement.
            let previous = forwarders.lock().await.insert(key.clone(), task);
            if let Some(previous) = previous {
                previous.abort();
                let _ = previous.await;
            }
            assert_eq!(
                counter.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "round {round}: the live forwarder holds exactly one global slot"
            );
        }
        if let Some(task) = forwarders.lock().await.remove(&key) {
            task.abort();
            let _ = task.await;
        }
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "retiring the last forwarder releases its slot"
        );
    }
}
