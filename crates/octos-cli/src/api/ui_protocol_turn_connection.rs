//! What an admitted turn delivers through (T-0637, `docs/IM_REMOTE_CONTROL.md`
//! §14): `run_standalone_turn` and the send helpers it calls take a
//! [`TurnConnection`], not a socket. A [`WsConnection`] delivers each event to
//! its client directly and appends it to the session ledger, as before. A
//! [`LedgerConnection`] has no client: the turn's durable events reach their
//! readers through the ledger alone, so an ingress without a socket
//! (`ExternalTurns`, for the IM bridge) can run a turn and read its result
//! from the ledger.

use super::*;

/// A send that may wait for room on the transport.
pub(crate) type PendingSend<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SendError>> + Send + 'a>>;

/// Whoever a turn's events are delivered to: who it is, what it may do, and
/// how a frame reaches it.
pub(crate) trait TurnConnection: Send + Sync {
    /// Stamped on the ledger rows sent to this connection directly, so its
    /// own live forwarder does not deliver them twice.
    fn connection_id(&self) -> ConnectionId;

    /// An external client of `octos serve --host-managed` (UPCR-2026-036):
    /// its turns keep only the external tool allowlist.
    fn is_external(&self) -> bool;

    /// Whether a turn outlives this connection (see
    /// [`WsConnection::server_owns_turns`]).
    fn server_owns_turns(&self) -> bool;

    fn metrics(&self) -> &ConnectionMetrics;

    /// The features direct sends are filtered by.
    fn snapshot_live_features(&self) -> ConnectionUiFeatures;

    /// Whether an untagged ledger append for `session_id` reaches this
    /// connection through a live forwarder.
    fn has_live_forwarder(&self, session_id: &SessionKey) -> bool;

    fn is_failed(&self) -> bool;

    #[cfg(any(test, feature = "transport-test-fixtures"))]
    fn mark_failed(&self);

    fn try_enqueue(&self, frame: WsMessage) -> Result<(), SendError>;

    fn send_lifecycle(&self, frame: WsMessage) -> Result<(), SendError>;

    fn send_durable(&self, frame: WsMessage, method: &str) -> Result<(), SendError>;

    fn send_ephemeral(&self, frame: WsMessage, method: &str) -> Result<(), SendError>;

    fn send_durable_offloaded<'a>(&'a self, frame: WsMessage, method: &'a str) -> PendingSend<'a>;

    fn send_lifecycle_offloaded(&self, frame: WsMessage) -> PendingSend<'_>;
}

impl TurnConnection for WsConnection {
    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn is_external(&self) -> bool {
        WsConnection::is_external(self)
    }

    fn server_owns_turns(&self) -> bool {
        WsConnection::server_owns_turns(self)
    }

    fn metrics(&self) -> &ConnectionMetrics {
        &self.metrics
    }

    fn snapshot_live_features(&self) -> ConnectionUiFeatures {
        WsConnection::snapshot_live_features(self)
    }

    fn has_live_forwarder(&self, session_id: &SessionKey) -> bool {
        WsConnection::has_live_forwarder(self, session_id)
    }

    fn is_failed(&self) -> bool {
        WsConnection::is_failed(self)
    }

    #[cfg(any(test, feature = "transport-test-fixtures"))]
    fn mark_failed(&self) {
        WsConnection::mark_failed(self)
    }

    fn try_enqueue(&self, frame: WsMessage) -> Result<(), SendError> {
        WsConnection::try_enqueue(self, frame)
    }

    fn send_lifecycle(&self, frame: WsMessage) -> Result<(), SendError> {
        WsConnection::send_lifecycle(self, frame)
    }

    fn send_durable(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        WsConnection::send_durable(self, frame, method)
    }

    fn send_ephemeral(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        WsConnection::send_ephemeral(self, frame, method)
    }

    fn send_durable_offloaded<'a>(&'a self, frame: WsMessage, method: &'a str) -> PendingSend<'a> {
        Box::pin(WsConnection::send_durable_offloaded(self, frame, method))
    }

    fn send_lifecycle_offloaded(&self, frame: WsMessage) -> PendingSend<'_> {
        Box::pin(WsConnection::send_lifecycle_offloaded(self, frame))
    }
}

/// A shared connection delivers like the one it shares, so a turn can hold
/// its connection as an `Arc<dyn TurnConnection>` and pass `&connection`.
impl<T: TurnConnection + ?Sized> TurnConnection for Arc<T> {
    fn connection_id(&self) -> ConnectionId {
        (**self).connection_id()
    }

    fn is_external(&self) -> bool {
        (**self).is_external()
    }

    fn server_owns_turns(&self) -> bool {
        (**self).server_owns_turns()
    }

    fn metrics(&self) -> &ConnectionMetrics {
        (**self).metrics()
    }

    fn snapshot_live_features(&self) -> ConnectionUiFeatures {
        (**self).snapshot_live_features()
    }

    fn has_live_forwarder(&self, session_id: &SessionKey) -> bool {
        (**self).has_live_forwarder(session_id)
    }

    fn is_failed(&self) -> bool {
        (**self).is_failed()
    }

    #[cfg(any(test, feature = "transport-test-fixtures"))]
    fn mark_failed(&self) {
        (**self).mark_failed()
    }

    fn try_enqueue(&self, frame: WsMessage) -> Result<(), SendError> {
        (**self).try_enqueue(frame)
    }

    fn send_lifecycle(&self, frame: WsMessage) -> Result<(), SendError> {
        (**self).send_lifecycle(frame)
    }

    fn send_durable(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        (**self).send_durable(frame, method)
    }

    fn send_ephemeral(&self, frame: WsMessage, method: &str) -> Result<(), SendError> {
        (**self).send_ephemeral(frame, method)
    }

    fn send_durable_offloaded<'a>(&'a self, frame: WsMessage, method: &'a str) -> PendingSend<'a> {
        (**self).send_durable_offloaded(frame, method)
    }

    fn send_lifecycle_offloaded(&self, frame: WsMessage) -> PendingSend<'_> {
        (**self).send_lifecycle_offloaded(frame)
    }
}

/// A connection with no client. Nothing is sent to it directly; every frame
/// a turn would send is dropped, and the turn's durable events reach readers
/// through the session ledger, which the turn appends to either way. Its
/// turns are external (UPCR-2026-036): they keep only the external tool
/// allowlist, and the connection can interrupt and answer only for them.
// Only `ExternalTurns` makes one, and nothing in `octos serve` calls it yet.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) struct LedgerConnection {
    connection_id: ConnectionId,
    metrics: ConnectionMetrics,
}

impl LedgerConnection {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new() -> Self {
        Self {
            connection_id: ConnectionId::next(),
            metrics: ConnectionMetrics::default(),
        }
    }
}

impl TurnConnection for LedgerConnection {
    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn is_external(&self) -> bool {
        true
    }

    fn server_owns_turns(&self) -> bool {
        false
    }

    fn metrics(&self) -> &ConnectionMetrics {
        &self.metrics
    }

    fn snapshot_live_features(&self) -> ConnectionUiFeatures {
        ConnectionUiFeatures::default()
    }

    fn has_live_forwarder(&self, _session_id: &SessionKey) -> bool {
        false
    }

    fn is_failed(&self) -> bool {
        false
    }

    #[cfg(any(test, feature = "transport-test-fixtures"))]
    fn mark_failed(&self) {}

    fn try_enqueue(&self, _frame: WsMessage) -> Result<(), SendError> {
        Ok(())
    }

    fn send_lifecycle(&self, _frame: WsMessage) -> Result<(), SendError> {
        Ok(())
    }

    fn send_durable(&self, _frame: WsMessage, _method: &str) -> Result<(), SendError> {
        Ok(())
    }

    fn send_ephemeral(&self, _frame: WsMessage, _method: &str) -> Result<(), SendError> {
        Ok(())
    }

    fn send_durable_offloaded<'a>(
        &'a self,
        _frame: WsMessage,
        _method: &'a str,
    ) -> PendingSend<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn send_lifecycle_offloaded(&self, _frame: WsMessage) -> PendingSend<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
}
