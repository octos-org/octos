//! Session state machine and configuration for agent loop control.
//!
//! Provides formal lifecycle states (A4) and configurable per-session limits (A5).
//!
//! TODO: Wire `SessionStateHandle` and `SessionUsage` into the agent loop
//! (`agent.rs`) to enforce `max_turns` and per-tool limits at runtime.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::watch;

/// Session lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Waiting for user input.
    Idle,
    /// Agent loop is actively running.
    Processing,
    /// Agent is waiting for external input (e.g., human confirmation).
    AwaitingInput,
    /// Session has been closed.
    Closed,
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Processing => write!(f, "processing"),
            Self::AwaitingInput => write!(f, "awaiting_input"),
            Self::Closed => write!(f, "closed"),
        }
    }
}

/// Observable session state with watch channel for subscribers.
#[derive(Clone)]
pub struct SessionStateHandle {
    tx: Arc<watch::Sender<SessionState>>,
    rx: watch::Receiver<SessionState>,
}

impl SessionStateHandle {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(SessionState::Idle);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// Get the current state.
    pub fn get(&self) -> SessionState {
        *self.rx.borrow()
    }

    /// Transition to a new state.
    pub fn set(&self, state: SessionState) {
        let _ = self.tx.send(state);
    }

    /// Subscribe to state changes.
    pub fn subscribe(&self) -> watch::Receiver<SessionState> {
        self.tx.subscribe()
    }
}

impl Default for SessionStateHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-session configuration limits (A5).
#[derive(Debug, Clone, Default)]
pub struct SessionLimits {
    /// Maximum LLM call turns per session. None = use AgentConfig.max_iterations.
    pub max_turns: Option<u32>,
    /// Maximum parallel tool execution rounds. None = unlimited.
    pub max_tool_rounds: Option<u32>,
    /// Per-tool execution limits (tool_name → max_calls). Empty = unlimited.
    pub per_tool_limits: HashMap<String, u32>,
}

/// Tracks per-session usage against SessionLimits.
#[derive(Debug, Default)]
pub struct SessionUsage {
    pub tool_rounds: u32,
    pub tool_calls: HashMap<String, u32>,
}

impl SessionUsage {
    /// Check if a tool call is allowed by the session limits.
    pub fn check_tool_allowed(&self, tool_name: &str, limits: &SessionLimits) -> bool {
        if let Some(max_rounds) = limits.max_tool_rounds {
            if self.tool_rounds >= max_rounds {
                return false;
            }
        }
        if let Some(&max_calls) = limits.per_tool_limits.get(tool_name) {
            let used = self.tool_calls.get(tool_name).copied().unwrap_or(0);
            if used >= max_calls {
                return false;
            }
        }
        true
    }

    /// Record a tool call.
    pub fn record_tool_call(&mut self, tool_name: &str) {
        *self.tool_calls.entry(tool_name.to_string()).or_insert(0) += 1;
    }

    /// Record a tool round (one batch of parallel tool calls).
    pub fn record_tool_round(&mut self) {
        self.tool_rounds += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_start_in_idle() {
        let handle = SessionStateHandle::new();
        assert_eq!(handle.get(), SessionState::Idle);
    }

    #[test]
    fn should_transition_states() {
        let handle = SessionStateHandle::new();
        handle.set(SessionState::Processing);
        assert_eq!(handle.get(), SessionState::Processing);
        handle.set(SessionState::AwaitingInput);
        assert_eq!(handle.get(), SessionState::AwaitingInput);
        handle.set(SessionState::Closed);
        assert_eq!(handle.get(), SessionState::Closed);
    }

    #[tokio::test]
    async fn should_notify_subscribers() {
        let handle = SessionStateHandle::new();
        let mut rx = handle.subscribe();
        handle.set(SessionState::Processing);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), SessionState::Processing);
    }

    #[test]
    fn should_display_state() {
        assert_eq!(SessionState::Idle.to_string(), "idle");
        assert_eq!(SessionState::Processing.to_string(), "processing");
        assert_eq!(SessionState::AwaitingInput.to_string(), "awaiting_input");
        assert_eq!(SessionState::Closed.to_string(), "closed");
    }

    #[test]
    fn should_allow_unlimited_tool_calls() {
        let usage = SessionUsage::default();
        let limits = SessionLimits::default();
        assert!(usage.check_tool_allowed("shell", &limits));
    }

    #[test]
    fn should_enforce_per_tool_limits() {
        let mut usage = SessionUsage::default();
        let limits = SessionLimits {
            per_tool_limits: [("shell".into(), 2)].into(),
            ..Default::default()
        };
        assert!(usage.check_tool_allowed("shell", &limits));
        usage.record_tool_call("shell");
        assert!(usage.check_tool_allowed("shell", &limits));
        usage.record_tool_call("shell");
        assert!(!usage.check_tool_allowed("shell", &limits));
        // Other tools unaffected
        assert!(usage.check_tool_allowed("read_file", &limits));
    }

    #[test]
    fn should_enforce_max_tool_rounds() {
        let mut usage = SessionUsage::default();
        let limits = SessionLimits {
            max_tool_rounds: Some(3),
            ..Default::default()
        };
        for _ in 0..3 {
            assert!(usage.check_tool_allowed("shell", &limits));
            usage.record_tool_round();
        }
        assert!(!usage.check_tool_allowed("shell", &limits));
    }
}
