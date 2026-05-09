//! Per-connection UI protocol driver.
//!
//! This module owns the semantic state for one AppUI connection. The
//! WebSocket adapter remains responsible for frame parsing and serialization;
//! the driver owns command dispatch and connection-scoped cleanup.

use super::*;
use octos_core::ui_protocol::{
    ApprovalRespondParams, ApprovalScopesListParams, DiffPreviewGetParams, TaskOutputReadParams,
};

pub(super) struct UiProtocolDriver {
    state: Arc<AppState>,
    active_turns: SharedActiveTurns,
    connection_turns: SharedConnectionTurns,
    live_forwarders: SharedLiveForwarders,
    contracts: Arc<UiProtocolContractStores>,
    ledger: Arc<UiProtocolLedger>,
    connection_profile_id: Option<String>,
    routed_profile_id: Option<String>,
    features: ConnectionUiFeatures,
}

impl UiProtocolDriver {
    pub(super) async fn new(
        state: Arc<AppState>,
        connection_profile_id: Option<String>,
        routed_profile_id: Option<String>,
        features: ConnectionUiFeatures,
    ) -> Self {
        let active_turns = active_turns_registry();
        let connection_turns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let live_forwarders = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let contracts = contract_stores();
        let ledger = event_ledger(&state).await;

        // Force lazy init of the diff-preview store on this connection so
        // its disk recovery + write-ahead path is wired up before any
        // approval flow can `upsert_file_mutation`. Subsequent calls reuse
        // the same `Arc`. Without `state.sessions` (headless smoke) this
        // installs the ephemeral RAM-only fallback.
        let _ = diff_preview_store(&state, contracts.as_ref()).await;

        Self {
            state,
            active_turns,
            connection_turns,
            live_forwarders,
            contracts,
            ledger,
            connection_profile_id,
            routed_profile_id,
            features,
        }
    }

    pub(super) async fn handle_command(&self, ws: &WsConnection, id: String, command: UiCommand) {
        match command {
            UiCommand::SessionOpen(params) => self.handle_session_open(ws, id, params).await,
            UiCommand::TurnStart(params) => self.handle_turn_start(ws, id, params).await,
            UiCommand::TurnInterrupt(params) => self.handle_turn_interrupt(ws, id, params).await,
            UiCommand::ApprovalRespond(params) => {
                self.handle_approval_respond(ws, id, params).await;
            }
            UiCommand::ApprovalScopesList(params) => {
                self.handle_approval_scopes_list(ws, id, params).await;
            }
            UiCommand::DiffPreviewGet(params) => {
                self.handle_diff_preview_get(ws, id, params).await;
            }
            UiCommand::TaskOutputRead(params) => {
                self.handle_task_output_read(ws, id, params).await;
            }
            UiCommand::TaskList(params) => self.handle_task_list(ws, id, params).await,
            UiCommand::TaskCancel(params) => self.handle_task_cancel(ws, id, params).await,
            UiCommand::TaskRestartFromNode(params) => {
                self.handle_task_restart_from_node(ws, id, params).await;
            }
            UiCommand::SessionHydrate(params) => self.handle_session_hydrate(ws, id, params).await,
            UiCommand::ThreadGraphGet(params) => self.handle_thread_graph_get(ws, id, params).await,
            UiCommand::TurnStateGet(params) => self.handle_turn_state_get(ws, id, params).await,
        }
    }

    pub(super) async fn shutdown(&self) {
        abort_connection_turns(
            &self.active_turns,
            &self.connection_turns,
            &self.contracts.scopes,
        )
        .await;
        abort_live_forwarders(&self.live_forwarders).await;
    }

    async fn handle_session_open(&self, ws: &WsConnection, id: String, params: SessionOpenParams) {
        handle_session_open(
            ws,
            &self.state,
            &self.ledger,
            &self.contracts.approvals,
            &self.live_forwarders,
            self.connection_profile_id.as_deref(),
            self.features,
            id,
            params,
        )
        .await;
    }

    async fn handle_turn_start(&self, ws: &WsConnection, id: String, params: TurnStartParams) {
        handle_turn_start(
            ws,
            &self.state,
            &self.ledger,
            &self.contracts,
            &self.active_turns,
            &self.connection_turns,
            self.connection_profile_id.as_deref(),
            self.routed_profile_id.as_deref(),
            self.features,
            id,
            params,
        )
        .await;
    }

    async fn handle_turn_interrupt(
        &self,
        ws: &WsConnection,
        id: String,
        params: TurnInterruptParams,
    ) {
        handle_turn_interrupt(
            ws,
            &self.ledger,
            &self.active_turns,
            &self.contracts,
            id,
            params,
        )
        .await;
    }

    async fn handle_approval_respond(
        &self,
        ws: &WsConnection,
        id: String,
        params: ApprovalRespondParams,
    ) {
        handle_approval_respond(
            ws,
            &self.state,
            &self.ledger,
            &self.contracts,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_approval_scopes_list(
        &self,
        ws: &WsConnection,
        id: String,
        params: ApprovalScopesListParams,
    ) {
        handle_approval_scopes_list(
            ws,
            &self.contracts.scopes,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_diff_preview_get(
        &self,
        ws: &WsConnection,
        id: String,
        params: DiffPreviewGetParams,
    ) {
        let store = diff_preview_store(&self.state, self.contracts.as_ref()).await;
        handle_diff_preview_get(
            ws,
            store.as_ref(),
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_task_output_read(
        &self,
        ws: &WsConnection,
        id: String,
        params: TaskOutputReadParams,
    ) {
        handle_task_output_read(
            ws,
            &self.state,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_task_list(&self, ws: &WsConnection, id: String, params: TaskListParams) {
        handle_task_list(
            ws,
            &self.state,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_task_cancel(&self, ws: &WsConnection, id: String, params: TaskCancelParams) {
        handle_task_cancel(
            ws,
            &self.state,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_task_restart_from_node(
        &self,
        ws: &WsConnection,
        id: String,
        params: TaskRestartFromNodeParams,
    ) {
        handle_task_restart_from_node(
            ws,
            &self.state,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_session_hydrate(
        &self,
        ws: &WsConnection,
        id: String,
        params: SessionHydrateParams,
    ) {
        handle_session_hydrate(
            ws,
            &self.state,
            &self.ledger,
            &self.contracts.approvals,
            &self.active_turns,
            self.connection_profile_id.as_deref(),
            self.features,
            id,
            params,
        )
        .await;
    }

    async fn handle_thread_graph_get(
        &self,
        ws: &WsConnection,
        id: String,
        params: ThreadGraphGetParams,
    ) {
        handle_thread_graph_get(
            ws,
            &self.state,
            &self.ledger,
            &self.active_turns,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }

    async fn handle_turn_state_get(
        &self,
        ws: &WsConnection,
        id: String,
        params: TurnStateGetParams,
    ) {
        handle_turn_state_get(
            ws,
            &self.state,
            &self.ledger,
            &self.active_turns,
            self.connection_profile_id.as_deref(),
            id,
            params,
        )
        .await;
    }
}
