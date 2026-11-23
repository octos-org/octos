//! A peer's declared task inputs ([`octos_core::task_inputs`]).
//!
//! `peer_handoff` may name the files its peer needs to begin, and
//! `peer_send_input` the files the task in its message needs. Either
//! declaration is checked against the peer's working directory when it is
//! made, and travels with what it belongs to: beside the brief, or in the
//! queue with the message. The server then puts the files into the message
//! that starts the work — the first of the peer's conversation, or the one
//! that delivers the input — whichever client sends that turn. The reading,
//! the budget and the report are [`octos_agent::task_inputs`]'s, the resolver
//! a fleet task's worker uses.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use octos_agent::policy::FilesystemScope;
use octos_agent::task_inputs::{
    InputAuthority, InputContext, check_declared_inputs, input_budget_tokens,
    resolve_declared_inputs,
};
use octos_core::task_inputs::{
    DeclaredInput, InputOutcome, InputReport, OmitReason, parse_declared_inputs,
};
use octos_core::ui_protocol::RpcError;
use octos_core::{Message, SessionKey, SessionScope};
use octos_llm::LlmProvider;
use tracing::{info, warn};

use super::peer_io;

/// The declaration, as the handoff gave it. Written before `brief.md`.
const DECLARED_LEAF: &str = "inputs.json";
/// What each message that was due inputs carried of them: one JSON line per
/// turn, oldest first.
const REPORT_LEAF: &str = "inputs-report.jsonl";

/// Check `inputs` against the peer's working directory and record them in
/// `peer_dir`. Returns what the check found, or `None` when nothing is
/// declared.
///
/// A required input that is not there to deliver refuses the staging: the
/// handoff's caller can still fix the declaration, a started peer cannot. An
/// input outside the working directory is left to the peer session's own
/// reach, which only its first turn knows.
pub(super) fn record_at_staging(
    peer_dir: &Path,
    cwd: &Path,
    fenced: bool,
    inputs: &[DeclaredInput],
    session_scope: Option<&SessionScope>,
) -> Result<Option<InputReport>, RpcError> {
    if inputs.is_empty() {
        return Ok(None);
    }
    let report = check_in_working_dir(cwd, fenced, inputs, session_scope)
        .map_err(|why| RpcError::invalid_params(format!("peer not staged: {why}")))?;
    let body = serde_json::to_string(inputs)
        .map_err(|err| RpcError::internal_error(format!("failed to encode peer inputs: {err}")))?;
    peer_io::write_peer_file_atomic(peer_dir, DECLARED_LEAF, &body)
        .map_err(|err| RpcError::internal_error(format!("failed to record peer inputs: {err}")))?;
    Ok(Some(report))
}

/// Check the inputs a message to an existing peer declares against that
/// peer's working directory, and encode them for the queue that carries the
/// message. `Ok(None)` when it declares none.
///
/// As at staging, a required input that is not there refuses the send, while
/// the sender can still fix it. A peer with no recorded working directory
/// (staged before the binding was kept) is not checked here; the turn that
/// delivers the message decides.
pub(crate) fn check_for_message(
    peer_dir: &Path,
    inputs: &[DeclaredInput],
    session_scope: Option<&SessionScope>,
) -> Result<Option<String>, String> {
    if inputs.is_empty() {
        return Ok(None);
    }
    if let Some((cwd, fenced)) = working_dir(peer_dir) {
        check_in_working_dir(&cwd, fenced, inputs, session_scope)
            .map_err(|why| format!("input not sent: {why}"))?;
    }
    serde_json::to_string(inputs)
        .map(Some)
        .map_err(|err| format!("failed to encode the declared inputs: {err}"))
}

/// Where a staged peer works, and whether that is its own git worktree.
fn working_dir(peer_dir: &Path) -> Option<(PathBuf, bool)> {
    let read = |leaf| peer_io::read_peer_file(peer_dir, leaf, peer_io::PEER_FILE_READ_CAP_SMALL);
    if read("workspace-mode").as_deref().map(str::trim) == Some("fenced") {
        return Some((peer_dir.join("wt"), true));
    }
    let root = read("workspace-root")?;
    let root = root.trim();
    (!root.is_empty()).then(|| (PathBuf::from(root), false))
}

/// What a declaration finds in the working directory of the peer it is for,
/// or why it cannot go: a required input is not there to deliver.
fn check_in_working_dir(
    cwd: &Path,
    fenced: bool,
    inputs: &[DeclaredInput],
    session_scope: Option<&SessionScope>,
) -> Result<InputReport, String> {
    let Some(scope) = session_scope else {
        return Ok(deferred_report(inputs));
    };
    let report = check_declared_inputs(inputs, InputAuthority::Session(scope));
    if report
        .undelivered_required()
        .any(|record| !left_to_the_peers_reach(&record.outcome))
    {
        let fence = if fenced {
            " The peer runs in its own git worktree, which holds only committed files."
        } else {
            ""
        };
        return Err(format!(
            "a required input cannot be delivered from the peer's working directory ({}).{fence} \
             Fix the declaration, or mark the input optional.\n{}",
            cwd.display(),
            staging_note(&report),
        ));
    }
    Ok(report)
}

/// No live session scope is available, so the declaration is carried to the
/// peer's first request without learning anything about the files here.
fn deferred_report(inputs: &[DeclaredInput]) -> InputReport {
    InputReport {
        inputs: inputs
            .iter()
            .map(|input| octos_core::task_inputs::InputRecord {
                path: input.path.clone(),
                required: input.required,
                outcome: InputOutcome::Omitted {
                    reason: OmitReason::Denied,
                    detail: "not checked here; the peer's first request resolves it with its own file access".to_owned(),
                },
            })
            .collect(),
        budget_tokens: 0,
        used_tokens: 0,
    }
}

fn left_to_the_peers_reach(outcome: &InputOutcome) -> bool {
    matches!(
        outcome,
        InputOutcome::Omitted {
            reason: OmitReason::Denied,
            ..
        }
    )
}

/// One line per declared input: what the staging check found.
pub(super) fn staging_note(report: &InputReport) -> String {
    let mut out = String::new();
    for record in &report.inputs {
        if !out.is_empty() {
            out.push('\n');
        }
        let required = if record.required {
            "required"
        } else {
            "optional"
        };
        match &record.outcome {
            InputOutcome::Delivered { form, bytes, .. } => out.push_str(&format!(
                "- `{}`: ready ({}, {bytes} bytes)",
                record.path,
                form.as_str()
            )),
            outcome @ InputOutcome::Omitted { detail, .. } if left_to_the_peers_reach(outcome) => {
                out.push_str(&format!(
                    "- `{}`: left for the peer's own file access ({required}); its first request \
                     resolves it: {detail}",
                    record.path
                ))
            }
            InputOutcome::Omitted { reason, detail } => out.push_str(&format!(
                "- `{}`: NOT ready ({required}, {}): {detail}",
                record.path,
                reason.as_str()
            )),
        }
    }
    out
}

/// The inputs `peer_dir`'s peer was staged with, for a fork that starts from
/// the same brief. A declaration that cannot be read is an error, never "none".
pub(super) fn declared(peer_dir: &Path) -> Result<Vec<DeclaredInput>, String> {
    match read_declared(peer_dir) {
        Declared::None => Ok(Vec::new()),
        Declared::Inputs(inputs) => Ok(inputs),
        Declared::Unreadable(err) => Err(err),
    }
}

enum Declared {
    None,
    Inputs(Vec<DeclaredInput>),
    Unreadable(String),
}

fn read_declared(peer_dir: &Path) -> Declared {
    let body = match peer_io::read_peer_control_file(
        peer_dir,
        DECLARED_LEAF,
        peer_io::PEER_FILE_READ_CAP_LARGE,
    ) {
        Ok(None) => return Declared::None,
        Ok(Some(body)) => body,
        Err(err) => return Declared::Unreadable(err.to_string()),
    };
    match decode(&body) {
        Ok(inputs) if inputs.is_empty() => Declared::None,
        Ok(inputs) => Declared::Inputs(inputs),
        Err(err) => Declared::Unreadable(err),
    }
}

/// A recorded or queued declaration, validated again as it is read back.
fn decode(encoded: &str) -> Result<Vec<DeclaredInput>, String> {
    serde_json::from_str::<serde_json::Value>(encoded)
        .map_err(|err| err.to_string())
        .and_then(|value| parse_declared_inputs(Some(&value)))
}

/// Whose reach decides what a peer may be given: its own session's, as its
/// file tools have it.
pub(crate) enum PeerReadAuthority {
    Session(Arc<SessionScope>),
    Filesystem {
        base_dir: PathBuf,
        scope: FilesystemScope,
    },
}

/// Which declarations a turn of a peer's session is due.
pub(crate) struct Due {
    /// The turn's message is the first of the conversation: the inputs the
    /// peer was staged with go with it.
    pub(crate) staged: bool,
    /// The declaration queued with the `peer_send_input` message this turn
    /// delivers ([`check_for_message`]); `None` when it declared none.
    pub(crate) with_message: Option<String>,
}

/// Put the declared inputs `due` to this turn of the peer behind `session`
/// into `message`, the message that starts the work.
///
/// A session that is not a staged peer's, or a turn that is due nothing,
/// leaves `message` as it is. Otherwise `message` gains either the inputs
/// (each whole, with a manifest of what was delivered and what was left out
/// and why) or, when a required one could not be delivered, a notice saying
/// that none were — a peer has no way to report back but to run. What was
/// decided is recorded beside the peer's brief.
pub(crate) async fn attach_to_message(
    peers_root: &Path,
    session: &SessionKey,
    turn_id: &str,
    due: Due,
    authority: PeerReadAuthority,
    llm: &Arc<dyn LlmProvider>,
    message: &mut Message,
) {
    let Some(slug) = session
        .topic()
        .and_then(|topic| topic.strip_prefix("peer-"))
    else {
        return;
    };
    let Some(peer_dir) = super::staged_peer_dir(peers_root, slug) else {
        return;
    };
    let mut inputs = Vec::new();
    let mut unreadable = None;
    if due.staged {
        match read_declared(&peer_dir) {
            Declared::None => {}
            Declared::Inputs(staged) => inputs.extend(staged),
            Declared::Unreadable(error) => unreadable = Some(error),
        }
    }
    if let Some(encoded) = &due.with_message {
        match decode(encoded) {
            Ok(declared) => inputs.extend(declared),
            Err(error) => unreadable = Some(error),
        }
    }
    if let Some(error) = unreadable {
        warn!(peer = slug, %error, "peer inputs declaration is unreadable; none delivered");
        message.content.push_str(&undeclared_notice(&error));
        return;
    }
    if inputs.is_empty() {
        return;
    }

    llm.ensure_ready().await;
    let budget_tokens = input_budget_tokens(llm.context_window());
    let accepts_images = llm.accepts_image_input();
    let resolved = tokio::task::spawn_blocking(move || {
        let authority = match &authority {
            PeerReadAuthority::Session(scope) => InputAuthority::Session(scope),
            PeerReadAuthority::Filesystem { base_dir, scope } => InputAuthority::Filesystem {
                base_dir,
                scope: *scope,
            },
        };
        resolve_declared_inputs(
            &inputs,
            &InputContext {
                authority,
                budget_tokens,
                accepts_images,
            },
        )
    })
    .await;
    let (delivered, report) = match resolved {
        Ok(Ok(resolved)) => {
            resolved.attach_to(message);
            (true, resolved.report)
        }
        Ok(Err(err)) => {
            message.content.push_str("\n\n");
            message.content.push_str(&err.notice());
            (false, err.report)
        }
        Err(error) => {
            warn!(peer = slug, %error, "reading peer inputs failed; none delivered");
            message
                .content
                .push_str(&undeclared_notice("they could not be read"));
            return;
        }
    };
    if delivered {
        info!(
            peer = slug,
            turn = turn_id,
            "peer task inputs:\n{}",
            report.summary()
        );
    } else {
        warn!(
            peer = slug,
            turn = turn_id,
            "peer task inputs withheld, a required one is undeliverable:\n{}",
            report.summary()
        );
    }
    let record = serde_json::json!({
        "turn_id": turn_id,
        "delivered": delivered,
        "report": report,
    });
    if let Err(error) = peer_io::append_peer_line(&peer_dir, REPORT_LEAF, &format!("{record}\n")) {
        warn!(peer = slug, %error, "failed to record the peer's input report");
    }
}

fn undeclared_notice(why: &str) -> String {
    format!(
        "\n\n## Task inputs — NOT delivered\n\n\
         This task declared input files, but the declaration could not be used ({why}), so \
         none are in this message. Do not guess what they say: read the files the brief \
         names yourself, and say in your result that the declared inputs were not delivered.\n"
    )
}

/// What each message of `slug`'s conversation that was due declared inputs
/// carried of them, oldest first: `(delivered, report)`.
#[cfg(test)]
pub(crate) fn delivery_records(peers_root: &Path, slug: &str) -> Vec<(bool, InputReport)> {
    let body = super::staged_peer_dir(peers_root, slug).and_then(|dir| {
        peer_io::read_peer_file(&dir, REPORT_LEAF, peer_io::PEER_FILE_READ_CAP_LARGE)
    });
    body.iter()
        .flat_map(|body| body.lines())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            (
                value["delivered"].as_bool().unwrap(),
                serde_json::from_value(value["report"].clone()).unwrap(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::task_inputs::InputKind;
    use sha2::{Digest, Sha256};

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let peer_dir = tmp.path().join("peers/edison");
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&peer_dir).unwrap();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        (tmp, peer_dir, cwd)
    }

    fn solo_scope(cwd: &Path) -> SessionScope {
        SessionScope::solo(cwd.to_path_buf(), Vec::new()).unwrap()
    }

    #[test]
    fn should_record_the_declaration_as_given_when_every_required_input_is_there() {
        let (_tmp, peer_dir, cwd) = dirs();
        std::fs::write(cwd.join("src/lib.rs"), "pub fn add() {}\n").unwrap();
        let inputs = vec![
            DeclaredInput::required("src/lib.rs"),
            DeclaredInput {
                kind: InputKind::Image,
                sha256: Some("a".repeat(64)),
                ..DeclaredInput::optional("renders/page-03.png")
            },
        ];

        let scope = solo_scope(&cwd);
        let report = record_at_staging(&peer_dir, &cwd, false, &inputs, Some(&scope))
            .unwrap()
            .unwrap();

        assert_eq!(declared(&peer_dir).unwrap(), inputs);
        let note = staging_note(&report);
        assert!(
            note.contains("- `src/lib.rs`: ready (text, 16 bytes)"),
            "{note}"
        );
        assert!(
            note.contains("- `renders/page-03.png`: NOT ready (optional, missing)"),
            "{note}"
        );
    }

    #[test]
    fn should_tell_a_fenced_peers_master_why_an_uncommitted_input_is_not_there() {
        let (_tmp, peer_dir, cwd) = dirs();

        let scope = solo_scope(&cwd);
        let err = record_at_staging(
            &peer_dir,
            &cwd,
            true,
            &[DeclaredInput::required("renders/page-03.png")],
            Some(&scope),
        )
        .unwrap_err();

        assert!(
            err.message
                .contains("`renders/page-03.png`: NOT ready (required, missing)"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("holds only committed files"),
            "{}",
            err.message
        );
        assert!(declared(&peer_dir).unwrap().is_empty(), "nothing recorded");
    }

    #[test]
    fn should_hide_staging_input_facts_from_a_denied_chat_path() {
        let (_tmp, peer_dir, cwd) = dirs();
        let hidden = cwd.join("chats/chat-1");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("notes.md"), "hidden chat secret").unwrap();
        let scope = SessionScope::solo(cwd.clone(), Vec::new())
            .unwrap()
            .with_hidden_folders(vec![cwd.join("chats")])
            .unwrap();
        let input = DeclaredInput {
            sha256: Some("0".repeat(64)),
            ..DeclaredInput::required("chats/chat-1/notes.md")
        };

        let report = record_at_staging(&peer_dir, &cwd, false, &[input], Some(&scope))
            .unwrap()
            .unwrap();
        let note = staging_note(&report);

        assert!(
            note.contains("left for the peer's own file access"),
            "{note}"
        );
        assert!(!note.contains("bytes"), "{note}");
        assert!(!note.contains("sha256"), "{note}");
        assert!(
            !note.contains(&format!("{:x}", Sha256::digest(b"hidden chat secret"))),
            "{note}"
        );
    }

    #[test]
    fn should_hide_send_input_facts_from_a_denied_chat_path() {
        let (_tmp, peer_dir, cwd) = dirs();
        std::fs::write(peer_dir.join("workspace-root"), cwd.display().to_string()).unwrap();
        let hidden = cwd.join("chats/chat-1");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("notes.md"), "hidden chat secret").unwrap();
        let scope = SessionScope::solo(cwd.clone(), Vec::new())
            .unwrap()
            .with_hidden_folders(vec![cwd.join("chats")])
            .unwrap();
        let input = DeclaredInput {
            sha256: Some("0".repeat(64)),
            ..DeclaredInput::required("chats/chat-1/notes.md")
        };

        let encoded = check_for_message(&peer_dir, &[input], Some(&scope))
            .unwrap()
            .unwrap();

        assert!(!encoded.contains("hidden chat secret"), "{encoded}");
        assert!(
            !encoded.contains(&format!("{:x}", Sha256::digest(b"hidden chat secret"))),
            "{encoded}"
        );
    }

    #[test]
    fn should_report_a_corrupt_declaration_instead_of_reading_it_as_none() {
        let (_tmp, peer_dir, _cwd) = dirs();
        std::fs::write(peer_dir.join(DECLARED_LEAF), "[{\"path\": 3}]").unwrap();

        assert!(declared(&peer_dir).is_err());
    }
}
