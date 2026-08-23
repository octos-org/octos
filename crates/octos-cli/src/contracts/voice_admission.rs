//! Short-lived proof that a voice preflight accepted an uploaded utterance.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use octos_core::{SessionKey, ui_protocol::TurnId};

// Longer than the bridge's 30s RPC timeout so one reconnect + idempotent retry
// cannot lose a still-valid proof, while remaining short-lived.
const VOICE_ADMISSION_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceAdmissionIssued {
    pub(crate) admission_id: String,
    pub(crate) transcript: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceAdmissionClaim {
    Start(String),
    AlreadyCommitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceAdmissionClaimError {
    MissingOrExpired,
    Mismatch,
    AlreadyInProgress,
    ConsumedByAnotherTurn,
}

impl VoiceAdmissionClaimError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::MissingOrExpired => "voice admission is missing or expired",
            Self::Mismatch => "voice admission does not match this session, turn, or audio",
            Self::AlreadyInProgress => "voice admission commit is already in progress",
            Self::ConsumedByAnotherTurn => "voice admission was consumed by another turn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VoiceAdmissionState {
    Pending,
    Claimed(TurnId),
    Committed(TurnId),
}

#[derive(Debug, Clone)]
struct VoiceAdmissionRecord {
    request_id: String,
    session_id: SessionKey,
    turn_id: TurnId,
    audio_paths: Vec<String>,
    transcript: String,
    expires_at: Instant,
    state: VoiceAdmissionState,
}

#[derive(Debug, Default)]
pub(crate) struct VoiceAdmissionStore {
    records: Mutex<HashMap<String, VoiceAdmissionRecord>>,
}

impl VoiceAdmissionStore {
    pub(crate) fn issue(
        &self,
        request_id: String,
        session_id: SessionKey,
        turn_id: TurnId,
        audio_paths: Vec<String>,
        transcript: String,
    ) -> VoiceAdmissionIssued {
        let now = Instant::now();
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        records.retain(|_, record| record.expires_at > now);
        if let Some((admission_id, record)) = records.iter().find(|(_, record)| {
            record.request_id == request_id
                && record.session_id == session_id
                && record.turn_id == turn_id
                && record.audio_paths == audio_paths
        }) {
            return VoiceAdmissionIssued {
                admission_id: admission_id.clone(),
                transcript: record.transcript.clone(),
            };
        }

        let admission_id = format!("voice-admission-{}", uuid::Uuid::now_v7());
        let issued = VoiceAdmissionIssued {
            admission_id: admission_id.clone(),
            transcript: transcript.clone(),
        };
        records.insert(
            admission_id.clone(),
            VoiceAdmissionRecord {
                request_id,
                session_id,
                turn_id,
                audio_paths,
                transcript,
                expires_at: now + VOICE_ADMISSION_TTL,
                state: VoiceAdmissionState::Pending,
            },
        );
        issued
    }

    pub(crate) fn claim(
        &self,
        admission_id: &str,
        session_id: &SessionKey,
        turn_id: &TurnId,
        audio_paths: &[String],
    ) -> Result<VoiceAdmissionClaim, VoiceAdmissionClaimError> {
        let now = Instant::now();
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        records.retain(|_, record| record.expires_at > now);
        let record = records
            .get_mut(admission_id)
            .ok_or(VoiceAdmissionClaimError::MissingOrExpired)?;
        if record.session_id != *session_id
            || record.turn_id != *turn_id
            || record.audio_paths != audio_paths
        {
            return Err(VoiceAdmissionClaimError::Mismatch);
        }
        match &record.state {
            VoiceAdmissionState::Pending => {
                record.state = VoiceAdmissionState::Claimed(turn_id.clone());
                Ok(VoiceAdmissionClaim::Start(record.transcript.clone()))
            }
            VoiceAdmissionState::Claimed(owner) if owner == turn_id => {
                Err(VoiceAdmissionClaimError::AlreadyInProgress)
            }
            VoiceAdmissionState::Committed(owner) if owner == turn_id => {
                Ok(VoiceAdmissionClaim::AlreadyCommitted)
            }
            VoiceAdmissionState::Claimed(_) | VoiceAdmissionState::Committed(_) => {
                Err(VoiceAdmissionClaimError::ConsumedByAnotherTurn)
            }
        }
    }

    pub(crate) fn release(&self, admission_id: &str, turn_id: &TurnId) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = records.get_mut(admission_id) {
            if matches!(&record.state, VoiceAdmissionState::Claimed(owner) if owner == turn_id) {
                record.state = VoiceAdmissionState::Pending;
            }
        }
    }

    pub(crate) fn finalize(&self, admission_id: &str, turn_id: &TurnId) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = records.get_mut(admission_id) {
            if matches!(&record.state, VoiceAdmissionState::Claimed(owner) if owner == turn_id) {
                record.state = VoiceAdmissionState::Committed(turn_id.clone());
            }
        }
    }
}
