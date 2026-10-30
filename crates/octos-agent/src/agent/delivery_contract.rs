//! Root artifact-delivery gate, reusing workspace contracts and ValidatorRunner.
//! No model verdict, filename heuristic, or presentation policy lives here.
use std::path::{Path, PathBuf};

use crate::validators::ValidatorPhase;
use crate::workspace_contract::run_declared_validators_with_output;
use crate::workspace_policy::{Validator, ValidatorPhaseKind, read_workspace_policy};

use super::Agent;
use super::activity::LoopActivityState;

pub(super) struct DeliveryContract {
    root: PathBuf,
    validators: Vec<Validator>,
}

impl DeliveryContract {
    /// Pin the declared checks before the model acts. Removing the policy or
    /// editing its required flags during this turn cannot waive these checks.
    pub(super) fn load(root: &Path, spawn_depth: u8) -> eyre::Result<Option<Self>> {
        if spawn_depth > 0 {
            return Ok(None);
        }
        let Some(policy) = read_workspace_policy(root)? else {
            return Ok(None);
        };
        let validators: Vec<_> = policy
            .validation
            .validators
            .into_iter()
            .filter(|v| v.phase == ValidatorPhaseKind::Delivery)
            .collect();
        if validators.is_empty() {
            return Ok(None);
        }
        let mut ids = std::collections::HashSet::new();
        eyre::ensure!(
            validators
                .iter()
                .all(|v| !v.id.is_empty() && ids.insert(v.id.clone())),
            "Delivery contract has empty or duplicate validator IDs"
        );
        Ok(Some(Self {
            root: root.into(),
            validators,
        }))
    }

    pub(super) async fn check(
        &self,
        agent: &Agent,
        files: &[PathBuf],
        activity: &LoopActivityState,
    ) -> Result<(), String> {
        let result = run_declared_validators_with_output(
            agent.tools.as_ref(),
            &self.root,
            &self.validators,
            "root-delivery",
            ValidatorPhase::Delivery,
            None,
            None,
            Some(files.to_vec()),
            agent.tools.sandbox(),
        )
        .await;
        // A host check may legitimately outlast the loop's idle window. Its
        // completed verdict (including failure/timeout) is new evidence the
        // agent must be allowed to act on. Without this, the next iteration
        // reports "idle" immediately after receiving actionable feedback.
        // Do not manufacture heartbeat progress while the check is pending:
        // the validator's own timeout and cancellation remain authoritative.
        activity.mark_activity();
        result.map(|_| ())
    }
}

pub(super) fn feedback(reason: &str) -> octos_core::Message {
    octos_core::Message::user(format!(
        "[Workspace delivery contract: NOT ACCEPTED]\n{reason}\n\n\
         These are results of mandatory host-run checks, not a request to change the task. \
         Inspect the evidence and repair the actual artifacts. Recheck the candidate after relevant changes. \
         Do not remove checks, edit verification records, or report acceptance while a required check is unresolved. \
         If the checker is unavailable, report the infrastructure problem rather than changing correct content."
    ))
}
