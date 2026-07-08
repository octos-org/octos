//! Model output schema + machine-enforced authority gates.
//!
//! `parse_model_output` handles SHAPE (strict JSON / schema — the only
//! failure class that earns the single corrective re-ask). `validate`
//! handles SEMANTICS: id resolution, authority, freezing, INIT 0-loss, the
//! entry-loss guard and consumed/dropped completeness. Every gate trusts
//! only host-written metadata (note frontmatter, extraction
//! `evidence_kind`); nothing the model claims is ever sufficient on its own.
//! Any semantic violation rejects the WHOLE merge — no partial apply.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDate;
use serde::Deserialize;

use super::entry::{Entry, UpdatedStamp};
use super::staging::{EvidenceKind, ExtractionItem, NoteFile, NoteKind, NoteOrigin};

/// One raw op proposed by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Add {
        section: Option<String>,
        text: String,
        sources: Vec<String>,
    },
    Update {
        id: String,
        new_text: String,
        sources: Vec<String>,
    },
    Supersede {
        id: String,
        replacement: Option<String>,
        reason: String,
        sources: Vec<String>,
    },
    Archive {
        id: String,
        reason: String,
        sources: Vec<String>,
    },
    HardDelete {
        id: String,
        authorized_by: String,
    },
}

/// A staging item the model dropped, with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub id: String,
    pub reason: String,
}

/// Model-suggested extra candidates for a pending note (may only ADD
/// candidates that also pass the Rust-side binding check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSuggestion {
    pub note_id: String,
    pub entry_ids: Vec<String>,
}

/// Parsed model reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOutput {
    pub ops: Vec<Op>,
    pub consumed_ids: Vec<String>,
    pub dropped: Vec<Dropped>,
    pub pending: Vec<PendingSuggestion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    ops: Vec<serde_json::Value>,
    #[serde(default)]
    consumed_ids: Vec<String>,
    #[serde(default)]
    dropped: Vec<RawDropped>,
    #[serde(default)]
    pending: Vec<RawPending>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDropped {
    id: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPending {
    note_id: String,
    #[serde(default)]
    entry_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdd {
    #[serde(default)]
    section: Option<String>,
    text: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    id: String,
    new_text: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSupersede {
    id: String,
    #[serde(default)]
    replacement: Option<String>,
    reason: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArchive {
    id: String,
    reason: String,
    #[serde(default)]
    sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHardDelete {
    id: String,
    authorized_by: String,
}

/// Strip one layer of markdown code fences if the reply is fenced.
fn strip_code_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let rest = rest.trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").map(str::trim_end).unwrap_or(rest)
}

/// Parse the model reply into a [`ModelOutput`]. Errors are `String`s meant
/// to be echoed back in the single corrective re-ask.
pub fn parse_model_output(raw: &str) -> Result<ModelOutput, String> {
    let cleaned = strip_code_fences(raw);
    let parsed: RawOutput = serde_json::from_str(cleaned)
        .map_err(|e| format!("reply is not the required JSON object: {e}"))?;

    let mut ops = Vec::with_capacity(parsed.ops.len());
    for (i, value) in parsed.ops.into_iter().enumerate() {
        let Some(obj) = value.as_object() else {
            return Err(format!("ops[{i}] must be a JSON object"));
        };
        let mut obj = obj.clone();
        let tag = match obj.remove("op") {
            Some(serde_json::Value::String(s)) => s,
            _ => return Err(format!("ops[{i}] is missing the string 'op' tag")),
        };
        let rest = serde_json::Value::Object(obj);
        let op = match tag.as_str() {
            "add" => serde_json::from_value::<RawAdd>(rest)
                .map(|r| Op::Add {
                    section: r.section,
                    text: r.text,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (add): {e}"))?,
            "update" => serde_json::from_value::<RawUpdate>(rest)
                .map(|r| Op::Update {
                    id: r.id,
                    new_text: r.new_text,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (update): {e}"))?,
            "supersede" => serde_json::from_value::<RawSupersede>(rest)
                .map(|r| Op::Supersede {
                    id: r.id,
                    replacement: r.replacement,
                    reason: r.reason,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (supersede): {e}"))?,
            "archive" => serde_json::from_value::<RawArchive>(rest)
                .map(|r| Op::Archive {
                    id: r.id,
                    reason: r.reason,
                    sources: r.sources,
                })
                .map_err(|e| format!("ops[{i}] (archive): {e}"))?,
            "hard_delete" => serde_json::from_value::<RawHardDelete>(rest)
                .map(|r| Op::HardDelete {
                    id: r.id,
                    authorized_by: r.authorized_by,
                })
                .map_err(|e| format!("ops[{i}] (hard_delete): {e}"))?,
            other => return Err(format!("ops[{i}] has unknown op kind '{other}'")),
        };
        ops.push(op);
    }

    Ok(ModelOutput {
        ops,
        consumed_ids: parsed.consumed_ids,
        dropped: parsed
            .dropped
            .into_iter()
            .map(|d| Dropped {
                id: d.id,
                reason: d.reason,
            })
            .collect(),
        pending: parsed
            .pending
            .into_iter()
            .map(|p| PendingSuggestion {
                note_id: p.note_id,
                entry_ids: p.entry_ids,
            })
            .collect(),
    })
}

/// Everything `validate` needs to judge a [`ModelOutput`].
pub struct ValidationCtx<'a> {
    /// Current MEMORY.md entries (post-INIT if this run performed INIT).
    pub entries: &'a [Entry],
    /// Interim-archived pending candidates: entry id → content hash. Valid
    /// hard-delete targets even though they are not in `entries`.
    pub interim: &'a HashMap<String, String>,
    /// Entry ids frozen by unresolved pending notes.
    pub frozen: &'a HashSet<String>,
    /// Batch notes by id (pending notes are NOT here — not consumable).
    pub notes: &'a HashMap<String, &'a NoteFile>,
    /// Extraction items by item id (`<stem>#<idx>`).
    pub items: &'a HashMap<String, &'a ExtractionItem>,
    /// This run performed INIT — only `add` ops are allowed (0-loss).
    pub init_mode: bool,
    pub today: NaiveDate,
    pub unused_days: u32,
}

/// A validated op with sanitized text, ready to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckedOp {
    Add {
        text: String,
        unverified: bool,
    },
    Update {
        id: String,
        new_text: String,
    },
    Supersede {
        id: String,
        replacement: Option<String>,
        reason: String,
    },
    Archive {
        id: String,
        reason: String,
    },
    HardDelete {
        id: String,
        authorized_by: String,
    },
}

/// The validated plan (ops in model order, semantics enforced).
#[derive(Debug, Clone, Default)]
pub struct ValidatedOps {
    pub ops: Vec<CheckedOp>,
    pub dropped: Vec<Dropped>,
    pub pending_suggestions: Vec<PendingSuggestion>,
}

impl<'a> ValidationCtx<'a> {
    fn entry(&self, id: &str) -> Option<&'a Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// A source qualifies as validated authority when it is a host note or
    /// an extraction item whose host-computed evidence is `user_said` /
    /// `tool_showed`. Model notes and `assistant_claimed` items never do —
    /// and neither does a free-text host forget note: it is parked as
    /// pending-confirm and carries no authority until the user confirms.
    fn source_qualifies(&self, source: &str) -> bool {
        if let Some(note) = self.notes.get(source) {
            // Forget notes carry hard-delete-only authority over their
            // NAMED ids (checked at the hard_delete gate); they must never
            // qualify as edit authority for update/supersede/archive — a
            // bad reply could otherwise cite a delete confirmation to
            // rewrite unrelated entries.
            return note.origin == NoteOrigin::Host && note.kind != NoteKind::Forget;
        }
        if let Some(item) = self.items.get(source) {
            return matches!(
                item.evidence_kind,
                EvidenceKind::UserSaid | EvidenceKind::ToolShowed
            );
        }
        false
    }

    fn source_known(&self, source: &str) -> bool {
        self.notes.contains_key(source) || self.items.contains_key(source)
    }

    /// Age-based auto-archive eligibility: REAL `(updated:)` stamp strictly
    /// older than `unused_days`; `unknown` NEVER auto-archives.
    fn age_qualifies(&self, entry: &Entry) -> bool {
        match entry.updated_stamp() {
            Some(UpdatedStamp::Real(date)) => {
                (self.today - date).num_days() > i64::from(self.unused_days)
            }
            Some(UpdatedStamp::Unknown) | None => false,
        }
    }
}

/// Sanitize model-provided entry text: normalize line ends, drop blank lines
/// (an entry is ONE block — a blank line would corrupt the file structure).
/// Errors on effectively-empty text.
fn sanitize_text(kind: &str, text: &str) -> Result<String, String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return Err(format!("{kind} text is empty"));
    }
    Ok(lines.join("\n"))
}

/// Strip a trailing echo of the target entry's own id token (models often
/// copy the id back since they see it in the file). Only the entry's OWN id
/// is stripped; the canonical token is re-appended at render time.
fn strip_id_echo(text: &str, id: &str) -> String {
    let trimmed = text.trim_end();
    if let Some(stripped) = trimmed.strip_suffix(id) {
        stripped.trim_end().to_string()
    } else {
        trimmed.to_string()
    }
}

/// Append engine-enforced markers and the id token to sanitized entry text:
/// `(unverified)` when required, `(updated: <today>)` when the model did not
/// stamp one itself, and the immutable id token last.
pub fn finalize_entry_text(text: &str, unverified: bool, today: NaiveDate, id: &str) -> String {
    let mut out = text.trim_end().to_string();
    if unverified && !out.contains("(unverified)") {
        out.push_str(" (unverified)");
    }
    if !out.contains("(updated:") {
        out.push_str(&format!(" (updated: {})", today.format("%Y-%m-%d")));
    }
    out.push_str(&format!(" {id}"));
    out
}

/// Validate a parsed model output against the authority gates. Any failure
/// rejects the whole merge — the error string is surfaced, never re-asked.
pub fn validate(output: &ModelOutput, ctx: &ValidationCtx) -> Result<ValidatedOps, String> {
    let mut checked: Vec<CheckedOp> = Vec::with_capacity(output.ops.len());
    let mut targeted: HashSet<&str> = HashSet::new();
    let mut removed_from_memory = 0usize;

    for (i, op) in output.ops.iter().enumerate() {
        // INIT is 0-loss: nothing but adds may touch a restructured file.
        if ctx.init_mode && !matches!(op, Op::Add { .. }) {
            return Err(format!(
                "ops[{i}]: INIT runs are 0-loss — only 'add' ops are allowed"
            ));
        }

        // Common source checks.
        let sources: &[String] = match op {
            Op::Add { sources, .. }
            | Op::Update { sources, .. }
            | Op::Supersede { sources, .. }
            | Op::Archive { sources, .. } => sources,
            Op::HardDelete { .. } => &[],
        };
        for src in sources {
            if !ctx.source_known(src) {
                return Err(format!(
                    "ops[{i}]: unknown source id '{src}' (not a batch note or extraction item)"
                ));
            }
        }

        match op {
            Op::Add { text, sources, .. } => {
                let text = sanitize_text("add", text)?;
                // A forget note may NEVER feed an add: a bad reply could
                // otherwise write the very content the user asked to forget
                // back into MEMORY.md while the request stays pending.
                for src in sources {
                    if ctx
                        .notes
                        .get(src)
                        .is_some_and(|n| n.kind == NoteKind::Forget)
                    {
                        return Err(format!(
                            "ops[{i}]: add cites forget note '{src}' — forget requests \
                             cannot source new memory"
                        ));
                    }
                }
                // Adds are otherwise always allowed, but weak sourcing
                // (model notes / assistant_claimed only, or no sources at
                // all) must be visible in the rendered entry.
                let unverified = !sources.iter().any(|s| ctx.source_qualifies(s));
                checked.push(CheckedOp::Add { text, unverified });
            }
            Op::Update {
                id,
                new_text,
                sources,
            } => {
                let entry = ctx
                    .entry(id)
                    .ok_or_else(|| format!("ops[{i}]: unknown entry id '{id}'"))?;
                if !targeted.insert(&entry.id) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                if ctx.frozen.contains(id) {
                    return Err(format!(
                        "ops[{i}]: entry '{id}' is frozen by an unresolved pending forget note"
                    ));
                }
                if !sources.iter().any(|s| ctx.source_qualifies(s)) {
                    return Err(format!(
                        "ops[{i}]: update of '{id}' lacks validated authority (needs a host \
                         note or user_said/tool_showed evidence)"
                    ));
                }
                let text = sanitize_text("update", &strip_id_echo(new_text, id))?;
                checked.push(CheckedOp::Update {
                    id: id.clone(),
                    new_text: text,
                });
            }
            Op::Supersede {
                id,
                replacement,
                reason,
                sources,
            } => {
                let entry = ctx
                    .entry(id)
                    .ok_or_else(|| format!("ops[{i}]: unknown entry id '{id}'"))?;
                if !targeted.insert(&entry.id) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                if ctx.frozen.contains(id) {
                    return Err(format!(
                        "ops[{i}]: entry '{id}' is frozen by an unresolved pending forget note"
                    ));
                }
                if !sources.iter().any(|s| ctx.source_qualifies(s)) {
                    return Err(format!(
                        "ops[{i}]: supersede of '{id}' lacks validated authority (needs a host \
                         note or user_said/tool_showed evidence)"
                    ));
                }
                let replacement = replacement
                    .as_deref()
                    .map(|r| sanitize_text("supersede replacement", &strip_id_echo(r, id)))
                    .transpose()?;
                if replacement.is_none() {
                    removed_from_memory += 1;
                }
                checked.push(CheckedOp::Supersede {
                    id: id.clone(),
                    replacement,
                    reason: reason.clone(),
                });
            }
            Op::Archive {
                id,
                reason,
                sources,
            } => {
                let entry = ctx
                    .entry(id)
                    .ok_or_else(|| format!("ops[{i}]: unknown entry id '{id}'"))?;
                if !targeted.insert(&entry.id) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                if ctx.frozen.contains(id) {
                    return Err(format!(
                        "ops[{i}]: entry '{id}' is frozen by an unresolved pending forget note"
                    ));
                }
                let source_ok = sources.iter().any(|s| ctx.source_qualifies(s));
                if !source_ok && !ctx.age_qualifies(entry) {
                    return Err(format!(
                        "ops[{i}]: archive of '{id}' lacks authority — needs validated \
                         sources or a real (updated:) stamp older than {} days",
                        ctx.unused_days
                    ));
                }
                removed_from_memory += 1;
                checked.push(CheckedOp::Archive {
                    id: id.clone(),
                    reason: reason.clone(),
                });
            }
            Op::HardDelete { id, authorized_by } => {
                let in_memory = ctx.entry(id).is_some();
                if !in_memory && !ctx.interim.contains_key(id) {
                    return Err(format!("ops[{i}]: unknown entry id '{id}'"));
                }
                // Interim-archived targets are tracked too: two ops on one
                // id are a conflict wherever the entry lives.
                if !targeted.insert(id.as_str()) {
                    return Err(format!("ops[{i}]: duplicate op targeting '{id}'"));
                }
                // Machine-enforced hard-delete gate: the authorizing note
                // must be a HOST-authored forget note in THIS batch whose
                // content names exactly this entry id. The model's claim is
                // never sufficient.
                let Some(note) = ctx.notes.get(authorized_by) else {
                    return Err(format!(
                        "ops[{i}]: hard_delete of '{id}' cites unknown authorizing note \
                         '{authorized_by}'"
                    ));
                };
                if note.origin != NoteOrigin::Host || note.kind != NoteKind::Forget {
                    return Err(format!(
                        "ops[{i}]: hard_delete of '{id}' not authorized — '{authorized_by}' \
                         is not a host forget note"
                    ));
                }
                if !note.named_entry_ids().iter().any(|n| n == id) {
                    return Err(format!(
                        "ops[{i}]: hard_delete of '{id}' not authorized — note \
                         '{authorized_by}' does not name id:{id}"
                    ));
                }
                // NOTE: a valid id-bound hard_delete is exempt from freezing
                // — it IS the confirmation path for pending candidates.
                if in_memory {
                    removed_from_memory += 1;
                }
                checked.push(CheckedOp::HardDelete {
                    id: id.clone(),
                    authorized_by: authorized_by.clone(),
                });
            }
        }
    }

    // Entry-loss guard: ops may not remove >50% of entries unless the file
    // is tiny (previous count ≤ 6). INIT already enforced 0 removals above.
    let prev = ctx.entries.len();
    if prev > 6 && removed_from_memory * 2 > prev {
        return Err(format!(
            "entry-loss guard: ops remove {removed_from_memory} of {prev} entries (>50%)"
        ));
    }

    // Add-back guard: a bad or prompt-injected reply must not survive a
    // hard delete by copying the deleted entry's text into an add/update/
    // supersede under a new id. Any folded line of new text matching a
    // hard-deleted target's folded lines rejects the whole merge.
    // Compare CONTENT with bookkeeping stripped: the deleted line carries
    // its `(updated:)` stamp and `^m` id while a re-added copy would not.
    // Strip every `^m<6 of [a-z2-7]>` token: the prompt shows entry ids,
    // so a verbatim-copied line can carry the OLD token — folding it
    // un-stripped would defeat the equality check.
    fn strip_all_id_tokens(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'^'
                && i + 7 < bytes.len() + 1
                && bytes.get(i + 1) == Some(&b'm')
                && bytes[i + 2..]
                    .iter()
                    .take(6)
                    .filter(|c| matches!(c, b'a'..=b'z' | b'2'..=b'7'))
                    .count()
                    == 6
            {
                i += 8;
                continue;
            }
            // Safe: we only skip full ASCII token bytes above.
            let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
        out
    }
    let strip_fold = |id: &str, text: &str| -> Vec<String> {
        let entry = super::entry::Entry {
            id: id.to_string(),
            text: text.to_string(),
        };
        strip_all_id_tokens(&super::pending::strippable_entry_text(&entry))
            .lines()
            .map(super::entry::fold_whitespace)
            .filter(|l| !l.is_empty())
            .collect()
    };
    let mut deleted_lines: HashSet<String> = HashSet::new();
    for op in &checked {
        if let CheckedOp::HardDelete { id, .. } = op {
            if let Some(entry) = ctx.entry(id) {
                deleted_lines.extend(strip_fold(&entry.id, &entry.text));
            } else if let Some(text) = ctx.interim.get(id) {
                deleted_lines.extend(strip_fold(id, text));
            }
        }
    }
    if !deleted_lines.is_empty() {
        for (i, op) in checked.iter().enumerate() {
            let new_text: Option<&str> = match op {
                CheckedOp::Add { text, .. } => Some(text),
                CheckedOp::Update { new_text, .. } => Some(new_text),
                CheckedOp::Supersede { replacement, .. } => replacement.as_deref(),
                _ => None,
            };
            let Some(new_text) = new_text else { continue };
            for folded in strip_fold("", new_text) {
                if deleted_lines.contains(&folded) {
                    return Err(format!(
                        "ops[{i}]: new text repeats a line of a hard-deleted entry — \
                         deleted content may not be re-added in the same merge"
                    ));
                }
            }
        }
    }

    validate_consumption(output, ctx)?;

    Ok(ValidatedOps {
        ops: checked,
        dropped: output.dropped.clone(),
        pending_suggestions: output.pending.clone(),
    })
}

/// Consumed/dropped completeness: every staging item is consumed or dropped
/// with a reason; host asks can never be silently dropped; free-text host
/// forget notes must stay pending.
fn validate_consumption(output: &ModelOutput, ctx: &ValidationCtx) -> Result<(), String> {
    let mut consumed: HashSet<&str> = HashSet::new();
    for id in &output.consumed_ids {
        if !ctx.source_known(id) {
            return Err(format!("consumed_ids contains unknown id '{id}'"));
        }
        if !consumed.insert(id.as_str()) {
            return Err(format!("consumed_ids contains duplicate id '{id}'"));
        }
    }
    let mut dropped: HashSet<&str> = HashSet::new();
    for d in &output.dropped {
        if !ctx.source_known(&d.id) {
            return Err(format!("dropped contains unknown id '{}'", d.id));
        }
        if consumed.contains(d.id.as_str()) {
            return Err(format!("id '{}' is both consumed and dropped", d.id));
        }
        if !dropped.insert(d.id.as_str()) {
            return Err(format!("dropped contains duplicate id '{}'", d.id));
        }
    }

    for (id, note) in ctx.notes.iter() {
        let id = id.as_str();
        if note.is_free_text_forget() {
            // Free-text forget notes go pending — consuming or dropping one
            // would fake a resolution that never happened.
            if consumed.contains(id) || dropped.contains(id) {
                return Err(format!(
                    "free-text host forget note '{id}' must be left pending, not \
                     consumed/dropped"
                ));
            }
            continue;
        }
        // HOST-authored notes are the hard-protected class. A MODEL-captured
        // user_request (memory_note tool) is durable against quarantine but
        // must stay consumable: it carries no delete authority, so a
        // forget-phrased ask could otherwise never be validly consumed and
        // would spin the fast lane forever. Dropping it requires a reason,
        // which the outcome surfaces (the host CLI is the delete path).
        let is_protected = note.origin == NoteOrigin::Host;
        if is_protected {
            if !consumed.contains(id) {
                return Err(format!(
                    "host/user_request note '{id}' was not consumed — a user ask may never \
                     be silently dropped"
                ));
            }
            // Consumption must be BACKED by an accepted op: `ops: []` with
            // the note in consumed_ids would delete the request file while
            // changing nothing — silently dropping an explicit user ask.
            let applied = output.ops.iter().any(|op| match op {
                Op::Add { sources, .. } => sources.iter().any(|s| s == id),
                Op::Update { sources, .. } => sources.iter().any(|s| s == id),
                Op::Supersede { sources, .. } => sources.iter().any(|s| s == id),
                Op::Archive { sources, .. } => sources.iter().any(|s| s == id),
                // A hard_delete covering an id this forget note names
                // applies it too (two notes can name the same id; one op
                // honors both — only one can be the cited authorizer).
                Op::HardDelete {
                    id: target,
                    authorized_by,
                } => {
                    authorized_by == id
                        || (note.kind == NoteKind::Forget
                            && note.named_entry_ids().iter().any(|n| n == target))
                }
            });
            if !applied {
                return Err(format!(
                    "host/user_request note '{id}' is consumed but no op cites it — \
                     a user ask may not be consumed without being applied"
                ));
            }
        } else if !consumed.contains(id) && !dropped.contains(id) {
            return Err(format!(
                "staging note '{id}' is neither consumed nor dropped-with-reason"
            ));
        }

        // Id-bound host forget notes must be honored by matching hard_delete
        // ops — consuming the note without acting on it drops the ask.
        // Two notes can name the SAME id (the user asked twice): one
        // hard_delete honors both — requiring each note to be the cited
        // authorizer would make the pair unsatisfiable (duplicate-target
        // ops are rejected) and wedge consolidation.
        if note.origin == NoteOrigin::Host && note.kind == NoteKind::Forget {
            for named in note.named_entry_ids() {
                // Ids that exist in neither MEMORY.md nor interim were
                // already handled Rust-side (archived-only scrub /
                // satisfied recovery) — the model cannot and need not
                // hard_delete them.
                if ctx.entry(&named).is_none() && !ctx.interim.contains_key(&named) {
                    continue;
                }
                let honored = output.ops.iter().any(|op| {
                    matches!(op, Op::HardDelete { id: target, authorized_by: auth }
                    if *target == named
                        && ctx.notes.get(auth.as_str()).is_some_and(|a| {
                            a.origin == NoteOrigin::Host
                                && a.kind == NoteKind::Forget
                                && a.named_entry_ids().contains(&named)
                        }))
                });
                if !honored {
                    return Err(format!(
                        "id-bound forget note '{id}' names {named} but no hard_delete op \
                         honors it"
                    ));
                }
            }
        }
    }

    for id in ctx.items.keys() {
        if !consumed.contains(id.as_str()) && !dropped.contains(id.as_str()) {
            return Err(format!(
                "extraction item '{id}' is neither consumed nor dropped-with-reason"
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::memory_consolidate::staging::parse_note;

    // --- fixtures -------------------------------------------------------

    fn note(id: &str, origin: &str, kind: &str, content: &str) -> NoteFile {
        let raw = format!(
            "---\norigin: {origin}\nkind: {kind}\ncreated_at: 2026-07-01T10:00:00+00:00\n---\n\n{content}\n"
        );
        parse_note(&PathBuf::from(format!("/staging/notes/{id}.md")), &raw).unwrap()
    }

    fn item(id: &str, evidence: EvidenceKind) -> ExtractionItem {
        ExtractionItem {
            id: id.to_string(),
            kind: "fact".to_string(),
            content: "extracted content".to_string(),
            evidence_kind: evidence,
            evidence_idx: vec![1],
            date: None,
        }
    }

    fn entries() -> Vec<Entry> {
        vec![
            Entry {
                id: "^maaaaaa".into(),
                text: "Lives in Portland. (updated: 2026-06-01) ^maaaaaa".into(),
            },
            Entry {
                id: "^mbbbbbb".into(),
                text: "Prefers tabs. (updated: 2026-01-01) ^mbbbbbb".into(),
            },
            Entry {
                id: "^mcccccc".into(),
                text: "Imported old fact. (updated: unknown, imported: 2026-07-01) ^mcccccc".into(),
            },
        ]
    }

    struct Fixture {
        entries: Vec<Entry>,
        interim: HashMap<String, String>,
        frozen: HashSet<String>,
        notes: Vec<NoteFile>,
        items: Vec<ExtractionItem>,
        init_mode: bool,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Self {
                entries: entries(),
                interim: HashMap::new(),
                frozen: HashSet::new(),
                notes: Vec::new(),
                items: Vec::new(),
                init_mode: false,
            }
        }
    }

    impl Fixture {
        fn validate(&self, output: &ModelOutput) -> Result<ValidatedOps, String> {
            let notes: HashMap<String, &NoteFile> =
                self.notes.iter().map(|n| (n.id.clone(), n)).collect();
            let items: HashMap<String, &ExtractionItem> =
                self.items.iter().map(|i| (i.id.clone(), i)).collect();
            let ctx = ValidationCtx {
                entries: &self.entries,
                interim: &self.interim,
                frozen: &self.frozen,
                notes: &notes,
                items: &items,
                init_mode: self.init_mode,
                today: NaiveDate::from_ymd_opt(2026, 7, 7).unwrap(),
                unused_days: 30,
            };
            validate(output, &ctx)
        }
    }

    fn output(ops: Vec<Op>, consumed: &[&str]) -> ModelOutput {
        ModelOutput {
            ops,
            consumed_ids: consumed.iter().map(|s| s.to_string()).collect(),
            dropped: vec![],
            pending: vec![],
        }
    }

    // --- parse ----------------------------------------------------------

    #[test]
    fn should_parse_all_op_kinds_when_valid_json() {
        let raw = r#"{"ops":[
            {"op":"add","section":null,"text":"new fact","sources":["n1"]},
            {"op":"update","id":"^maaaaaa","new_text":"newer","sources":["n1"]},
            {"op":"supersede","id":"^mbbbbbb","replacement":null,"reason":"stale","sources":["n1"]},
            {"op":"archive","id":"^mcccccc","reason":"old","sources":[]},
            {"op":"hard_delete","id":"^mdddddd","authorized_by":"n2"}
        ],"consumed_ids":["n1","n2"],"dropped":[{"id":"x1","reason":"noise"}]}"#;
        let out = parse_model_output(raw).unwrap();
        assert_eq!(out.ops.len(), 5);
        assert_eq!(out.consumed_ids, ["n1", "n2"]);
        assert_eq!(out.dropped.len(), 1);
        assert!(matches!(&out.ops[4], Op::HardDelete { id, .. } if id == "^mdddddd"));
    }

    #[test]
    fn should_parse_when_reply_is_code_fenced() {
        let raw = "```json\n{\"ops\":[],\"consumed_ids\":[]}\n```";
        assert!(parse_model_output(raw).is_ok());
        let raw2 = "```\n{\"ops\":[]}\n```";
        assert!(parse_model_output(raw2).is_ok());
    }

    #[test]
    fn should_reject_parse_when_schema_violated() {
        assert!(parse_model_output("I merged your memory, boss!").is_err());
        assert!(
            parse_model_output(r#"{"consumed_ids":[]}"#).is_err(),
            "ops required"
        );
        assert!(parse_model_output(r#"{"ops":[],"extra_key":1}"#).is_err());
        assert!(
            parse_model_output(r#"{"ops":[{"op":"obliterate","id":"^maaaaaa"}]}"#).is_err(),
            "unknown op kind"
        );
        assert!(
            parse_model_output(r#"{"ops":[{"op":"add"}]}"#).is_err(),
            "missing required field"
        );
        assert!(
            parse_model_output(r#"{"ops":[{"op":"add","text":"x","bonus":1}]}"#).is_err(),
            "unknown op field"
        );
        assert!(
            parse_model_output(r#"{"ops":[{"op":"hard_delete","id":"^maaaaaa"}]}"#).is_err(),
            "hard_delete requires authorized_by"
        );
    }

    // --- authority gates --------------------------------------------------

    #[test]
    fn should_accept_update_when_host_note_source() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("h1", "host", "fact", "user lives in Seattle"));
        let out = output(
            vec![Op::Update {
                id: "^maaaaaa".into(),
                new_text: "Lives in Seattle.".into(),
                sources: vec!["h1".into()],
            }],
            &["h1"],
        );
        let validated = fx.validate(&out).unwrap();
        assert_eq!(validated.ops.len(), 1);
    }

    #[test]
    fn should_accept_update_when_user_said_extraction_source() {
        let mut fx = Fixture::default();
        fx.items.push(item("e1#0", EvidenceKind::UserSaid));
        let out = output(
            vec![Op::Update {
                id: "^maaaaaa".into(),
                new_text: "Lives in Seattle.".into(),
                sources: vec!["e1#0".into()],
            }],
            &["e1#0"],
        );
        assert!(fx.validate(&out).is_ok());
    }

    #[test]
    fn should_reject_merge_when_update_sources_lack_host_or_validated_evidence() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("m1", "model", "fact", "user lives in Seattle"));
        fx.items.push(item("e1#0", EvidenceKind::AssistantClaimed));
        let model_note_only = output(
            vec![Op::Update {
                id: "^maaaaaa".into(),
                new_text: "Lives in Seattle.".into(),
                sources: vec!["m1".into()],
            }],
            &["m1", "e1#0"],
        );
        let err = fx.validate(&model_note_only).unwrap_err();
        assert!(err.contains("lacks validated authority"), "got: {err}");

        let claimed_only = output(
            vec![Op::Update {
                id: "^maaaaaa".into(),
                new_text: "Lives in Seattle.".into(),
                sources: vec!["e1#0".into()],
            }],
            &["m1", "e1#0"],
        );
        assert!(fx.validate(&claimed_only).is_err());
    }

    #[test]
    fn should_reject_update_when_only_source_is_free_text_forget_note() {
        let mut fx = Fixture::default();
        // Host forget note WITHOUT an id token: pending-confirm material,
        // not edit authority.
        fx.notes
            .push(note("h1", "host", "forget", "forget the portland stuff"));
        let out = output(
            vec![Op::Update {
                id: "^mbbbbbb".into(),
                new_text: "Prefers spaces.".into(),
                sources: vec!["h1".into()],
            }],
            &[],
        );
        let err = fx.validate(&out).unwrap_err();
        assert!(err.contains("lacks validated authority"), "got: {err}");
    }

    #[test]
    fn should_mark_unverified_when_add_sources_are_model_notes_only() {
        let mut fx = Fixture::default();
        fx.notes.push(note("m1", "model", "fact", "likes rust"));
        let out = output(
            vec![Op::Add {
                section: None,
                text: "Likes Rust.".into(),
                sources: vec!["m1".into()],
            }],
            &["m1"],
        );
        let validated = fx.validate(&out).unwrap();
        assert!(matches!(
            &validated.ops[0],
            CheckedOp::Add {
                unverified: true,
                ..
            }
        ));

        // With a qualifying source the add is verified.
        fx.notes.push(note("h1", "host", "fact", "likes rust"));
        let out2 = output(
            vec![Op::Add {
                section: None,
                text: "Likes Rust.".into(),
                sources: vec!["m1".into(), "h1".into()],
            }],
            &["m1", "h1"],
        );
        let validated2 = fx.validate(&out2).unwrap();
        assert!(matches!(
            &validated2.ops[0],
            CheckedOp::Add {
                unverified: false,
                ..
            }
        ));
    }

    #[test]
    fn should_reject_merge_when_hard_delete_authorized_by_model_note() {
        let mut fx = Fixture::default();
        fx.notes.push(note(
            "m1",
            "model",
            "user_request",
            "user said forget id:^maaaaaa",
        ));
        let out = output(
            vec![Op::HardDelete {
                id: "^maaaaaa".into(),
                authorized_by: "m1".into(),
            }],
            &["m1"],
        );
        let err = fx.validate(&out).unwrap_err();
        assert!(err.contains("not a host forget note"), "got: {err}");
    }

    #[test]
    fn should_accept_hard_delete_when_id_bound_host_forget_note() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("h1", "host", "forget", "forget id:^maaaaaa"));
        let out = output(
            vec![Op::HardDelete {
                id: "^maaaaaa".into(),
                authorized_by: "h1".into(),
            }],
            &["h1"],
        );
        assert!(fx.validate(&out).is_ok());
    }

    #[test]
    fn should_reject_hard_delete_when_note_names_different_id() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("h1", "host", "forget", "forget id:^mbbbbbb"));
        let out = ModelOutput {
            ops: vec![
                Op::HardDelete {
                    id: "^maaaaaa".into(),
                    authorized_by: "h1".into(),
                },
                Op::HardDelete {
                    id: "^mbbbbbb".into(),
                    authorized_by: "h1".into(),
                },
            ],
            consumed_ids: vec!["h1".into()],
            dropped: vec![],
            pending: vec![],
        };
        let err = fx.validate(&out).unwrap_err();
        assert!(err.contains("does not name id:^maaaaaa"), "got: {err}");
    }

    #[test]
    fn should_reject_duplicate_hard_deletes_when_target_interim_archived() {
        let mut fx = Fixture::default();
        fx.interim
            .insert("^mzzzzzz".to_string(), "somehash".to_string());
        fx.notes
            .push(note("h1", "host", "forget", "forget id:^mzzzzzz"));
        let dup = output(
            vec![
                Op::HardDelete {
                    id: "^mzzzzzz".into(),
                    authorized_by: "h1".into(),
                },
                Op::HardDelete {
                    id: "^mzzzzzz".into(),
                    authorized_by: "h1".into(),
                },
            ],
            &["h1"],
        );
        let err = fx.validate(&dup).unwrap_err();
        assert!(err.contains("duplicate op targeting"), "got: {err}");
    }

    #[test]
    fn should_allow_archive_when_age_qualified_without_sources() {
        let fx = Fixture::default();
        // ^mbbbbbb updated 2026-01-01, today 2026-07-07, unused_days 30.
        let out = output(
            vec![Op::Archive {
                id: "^mbbbbbb".into(),
                reason: "stale".into(),
                sources: vec![],
            }],
            &[],
        );
        assert!(fx.validate(&out).is_ok());
    }

    #[test]
    fn should_reject_archive_when_young_or_unknown_stamp() {
        let fx = Fixture::default();
        // ^maaaaaa updated 2026-06-01 — only 36 days? No: 2026-06-01 → 2026-07-07 is 36 days.
        // Use a young entry: craft output against ^maaaaaa with unused_days 30 → 36 > 30
        // would qualify, so target the unknown-stamp entry instead and a fresh one.
        let unknown_stamp = output(
            vec![Op::Archive {
                id: "^mcccccc".into(),
                reason: "old import".into(),
                sources: vec![],
            }],
            &[],
        );
        let err = fx.validate(&unknown_stamp).unwrap_err();
        assert!(
            err.contains("lacks authority"),
            "unknown never auto-archives: {err}"
        );

        let mut fx2 = Fixture::default();
        fx2.entries[0].text = "Lives in Portland. (updated: 2026-07-01) ^maaaaaa".into();
        let young = output(
            vec![Op::Archive {
                id: "^maaaaaa".into(),
                reason: "meh".into(),
                sources: vec![],
            }],
            &[],
        );
        assert!(fx2.validate(&young).is_err());
    }

    #[test]
    fn should_reject_ops_when_frozen_candidate_targeted() {
        let mut fx = Fixture::default();
        fx.frozen.insert("^maaaaaa".to_string());
        fx.notes
            .push(note("h1", "host", "fact", "user lives in Seattle"));
        let out = output(
            vec![Op::Update {
                id: "^maaaaaa".into(),
                new_text: "Lives in Seattle.".into(),
                sources: vec!["h1".into()],
            }],
            &["h1"],
        );
        let err = fx.validate(&out).unwrap_err();
        assert!(err.contains("frozen"), "got: {err}");

        // But a valid id-bound hard_delete (confirmation) is exempt.
        let mut fx2 = Fixture::default();
        fx2.frozen.insert("^maaaaaa".to_string());
        fx2.notes
            .push(note("h2", "host", "forget", "forget id:^maaaaaa"));
        let confirm = output(
            vec![Op::HardDelete {
                id: "^maaaaaa".into(),
                authorized_by: "h2".into(),
            }],
            &["h2"],
        );
        assert!(fx2.validate(&confirm).is_ok());
    }

    #[test]
    fn should_reject_non_add_ops_when_init_mode() {
        let mut fx = Fixture {
            init_mode: true,
            ..Fixture::default()
        };
        fx.notes.push(note("h1", "host", "fact", "x"));
        let out = output(
            vec![Op::Update {
                id: "^maaaaaa".into(),
                new_text: "changed".into(),
                sources: vec!["h1".into()],
            }],
            &["h1"],
        );
        let err = fx.validate(&out).unwrap_err();
        assert!(err.contains("0-loss"), "got: {err}");
    }

    #[test]
    fn should_reject_merge_when_over_half_entries_removed() {
        let fx = Fixture {
            entries: (0..8)
                .map(|i| {
                    let id = format!("^maaaaa{}", char::from(b'a' + i));
                    Entry {
                        text: format!("Fact {i}. (updated: 2026-01-01) {id}"),
                        id,
                    }
                })
                .collect(),
            ..Fixture::default()
        };
        let ops: Vec<Op> = fx.entries[..5]
            .iter()
            .map(|e| Op::Archive {
                id: e.id.clone(),
                reason: "stale".into(),
                sources: vec![],
            })
            .collect();
        let err = fx.validate(&output(ops, &[])).unwrap_err();
        assert!(err.contains("entry-loss guard"), "got: {err}");

        // Small files (≤6 entries) are exempt.
        let fx2 = Fixture::default();
        let ops2 = vec![Op::Archive {
            id: "^mbbbbbb".into(),
            reason: "stale".into(),
            sources: vec![],
        }];
        assert!(fx2.validate(&output(ops2, &[])).is_ok());
    }

    // --- consumption completeness ----------------------------------------

    #[test]
    fn should_reject_merge_when_host_note_not_consumed_and_not_pending() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("h1", "host", "forget", "forget id:^maaaaaa"));
        // Model neither consumed the note nor emitted the hard_delete.
        let out = output(vec![], &[]);
        let err = fx.validate(&out).unwrap_err();
        assert!(err.contains("was not consumed"), "got: {err}");
    }

    #[test]
    fn should_reject_merge_when_host_request_note_dropped() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("h1", "host", "user_request", "remember my birthday"));
        let out = ModelOutput {
            ops: vec![],
            consumed_ids: vec![],
            dropped: vec![Dropped {
                id: "h1".into(),
                reason: "seems unimportant".into(),
            }],
            pending: vec![],
        };
        let err = fx.validate(&out).unwrap_err();
        assert!(err.contains("never be silently dropped"), "got: {err}");
    }

    #[test]
    fn should_allow_dropping_model_user_request_with_reason() {
        // Model-captured user_requests carry no delete authority — a
        // forget-phrased ask could never be validly consumed, so dropping
        // WITH a surfaced reason must be legal (the host CLI is the
        // delete path).
        let mut fx = Fixture::default();
        fx.notes.push(note(
            "m1",
            "model",
            "user_request",
            "user asked to forget the address",
        ));
        let out = ModelOutput {
            ops: vec![],
            consumed_ids: vec![],
            dropped: vec![Dropped {
                id: "m1".into(),
                reason: "forget requests need host confirmation (octos memory forget)".into(),
            }],
            pending: vec![],
        };
        assert!(fx.validate(&out).is_ok());
    }

    #[test]
    fn should_reject_merge_when_id_bound_forget_not_honored() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("h1", "host", "forget", "forget id:^maaaaaa"));
        // Consumed, but no hard_delete op emitted — the applied-consumption
        // gate fires first (no op cites the note at all).
        let out = output(vec![], &["h1"]);
        let err = fx.validate(&out).unwrap_err();
        assert!(
            err.contains("consumed without being applied")
                || err.contains("no hard_delete op honors it"),
            "got: {err}"
        );
    }

    #[test]
    fn should_require_free_text_forget_to_stay_pending() {
        let mut fx = Fixture::default();
        fx.notes
            .push(note("h1", "host", "forget", "forget the portland stuff"));
        let consumed = output(vec![], &["h1"]);
        let err = fx.validate(&consumed).unwrap_err();
        assert!(err.contains("must be left pending"), "got: {err}");
        // Left alone (pending) is legal.
        assert!(fx.validate(&output(vec![], &[])).is_ok());
    }

    #[test]
    fn should_reject_merge_when_staging_item_unaccounted() {
        let mut fx = Fixture::default();
        fx.notes.push(note("m1", "model", "fact", "likes tea"));
        let err = fx.validate(&output(vec![], &[])).unwrap_err();
        assert!(err.contains("neither consumed nor dropped"), "got: {err}");

        let mut fx2 = Fixture::default();
        fx2.items.push(item("e1#0", EvidenceKind::UserSaid));
        let err2 = fx2.validate(&output(vec![], &[])).unwrap_err();
        assert!(err2.contains("neither consumed nor dropped"), "got: {err2}");
    }

    #[test]
    fn should_reject_merge_when_ids_unknown_or_conflicting() {
        let fx = Fixture::default();
        let unknown_target = output(
            vec![Op::Archive {
                id: "^mzzzzzz".into(),
                reason: "x".into(),
                sources: vec![],
            }],
            &[],
        );
        assert!(fx.validate(&unknown_target).is_err());

        let unknown_source = output(
            vec![Op::Add {
                section: None,
                text: "x".into(),
                sources: vec!["ghost".into()],
            }],
            &[],
        );
        assert!(fx.validate(&unknown_source).is_err());

        let unknown_consumed = output(vec![], &["ghost"]);
        assert!(fx.validate(&unknown_consumed).is_err());

        let mut fx2 = Fixture::default();
        fx2.notes.push(note("m1", "model", "fact", "x"));
        let both = ModelOutput {
            ops: vec![],
            consumed_ids: vec!["m1".into()],
            dropped: vec![Dropped {
                id: "m1".into(),
                reason: "also dropped".into(),
            }],
            pending: vec![],
        };
        assert!(fx2.validate(&both).is_err());

        // Duplicate destructive ops on one entry.
        let dup = output(
            vec![
                Op::Archive {
                    id: "^mbbbbbb".into(),
                    reason: "a".into(),
                    sources: vec![],
                },
                Op::Archive {
                    id: "^mbbbbbb".into(),
                    reason: "b".into(),
                    sources: vec![],
                },
            ],
            &[],
        );
        assert!(fx.validate(&dup).is_err());
    }

    // --- sanitation -------------------------------------------------------

    #[test]
    fn should_strip_id_echo_and_collapse_blank_lines_when_sanitizing() {
        let mut fx = Fixture::default();
        fx.notes.push(note("h1", "host", "fact", "seattle"));
        let out = output(
            vec![Op::Update {
                id: "^maaaaaa".into(),
                new_text: "Lives in Seattle.\n\nMoved in March. ^maaaaaa".into(),
                sources: vec!["h1".into()],
            }],
            &["h1"],
        );
        let validated = fx.validate(&out).unwrap();
        let CheckedOp::Update { new_text, .. } = &validated.ops[0] else {
            panic!("expected update");
        };
        assert_eq!(new_text, "Lives in Seattle.\nMoved in March.");
    }

    #[test]
    fn should_finalize_entry_text_with_markers_and_id() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 7).unwrap();
        assert_eq!(
            finalize_entry_text("Likes Rust.", true, today, "^mabc234"),
            "Likes Rust. (unverified) (updated: 2026-07-07) ^mabc234"
        );
        // Existing stamp is preserved; no double marker.
        assert_eq!(
            finalize_entry_text("Moved. (updated: 2026-07-01)", false, today, "^mabc234"),
            "Moved. (updated: 2026-07-01) ^mabc234"
        );
    }

    #[test]
    fn should_reject_empty_text_when_sanitizing() {
        let fx = Fixture::default();
        let out = output(
            vec![Op::Add {
                section: None,
                text: "  \n\n ".into(),
                sources: vec![],
            }],
            &[],
        );
        assert!(fx.validate(&out).is_err());
    }
}
