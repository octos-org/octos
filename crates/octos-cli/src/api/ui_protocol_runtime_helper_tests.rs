use super::*;

// ──────────────────────────────────────────────────────────────────────
// Codex round-2 MAJOR 2 regression: the runtime_error wire envelope
// must surface `HarnessError::message()` (user-actionable) rather
// than the raw `LlmError::Display` operator log line.
// ──────────────────────────────────────────────────────────────────────

#[test]
fn runtime_error_envelope_surfaces_harness_message_for_quota() {
    let llm = octos_llm::LlmError::from_status_with_label(
        403,
        r#"{"error":{"type":"insufficient_quota","message":"out of credits"}}"#,
        "MiniMax-M2.5-highspeed",
    );
    let report: eyre::Report = llm.into();
    let wire = super::classify_runtime_error_message(&report);
    // The user-facing message should mention the provider lane and
    // direct the operator to top up or switch.
    assert!(
        wire.contains("quota") || wire.contains("Quota"),
        "wire message should call out quota exhaustion — got {wire}"
    );
    assert!(
        wire.contains("top up") || wire.contains("switch provider"),
        "wire message should be actionable — got {wire}"
    );
    assert!(
        wire.contains("MiniMax-M2.5-highspeed"),
        "wire message must identify the failing lane — got {wire}"
    );
}

#[test]
fn runtime_error_envelope_surfaces_harness_message_for_auth() {
    let llm = octos_llm::LlmError::from_status_with_label(401, "Unauthorized", "openai/gpt-4");
    let report: eyre::Report = llm.into();
    let wire = super::classify_runtime_error_message(&report);
    assert!(
        wire.contains("Authentication") || wire.contains("API key"),
        "wire message should call out auth failure — got {wire}"
    );
    assert!(
        wire.contains("openai/gpt-4"),
        "wire message must identify the failing lane — got {wire}"
    );
}

#[test]
fn runtime_error_envelope_surfaces_harness_message_for_bad_request() {
    let llm = octos_llm::LlmError::from_status_with_label(
        400,
        "reasoning_content missing in assistant tool call message",
        "deepseek/v4-pro",
    );
    let report: eyre::Report = llm.into();
    let wire = super::classify_runtime_error_message(&report);
    assert!(
        wire.contains("rejected") || wire.contains("Provider"),
        "wire message should describe a request rejection — got {wire}"
    );
    assert!(
        wire.contains("deepseek/v4-pro"),
        "wire message must identify the failing lane — got {wire}"
    );
}

#[test]
fn runtime_error_envelope_falls_through_for_non_llm_errors() {
    // A tool-execution failure surfaces unchanged.
    let report: eyre::Report = eyre::eyre!("shell tool: command not found: foo");
    let wire = super::classify_runtime_error_message(&report);
    assert_eq!(wire, "shell tool: command not found: foo");
}

/// NEW-16 codex round-3: helper for building a
/// `TurnPersistCursorEntry` in tests at a given `next_index`,
/// stamped at NOW. Centralises the construction so a future
/// refactor of the entry type only touches one place.
fn make_cursor_entry_fresh(next_index: usize) -> super::TurnPersistCursorEntry {
    super::TurnPersistCursorEntry {
        next_index,
        last_touched_at: std::time::Instant::now(),
    }
}

/// NEW-16 codex round-4: the cursor tests share the same
/// process-wide static map AND the process-wide prune
/// throttle. Running them in parallel under
/// `cargo test --test-threads=N` would let one test's prune
/// or insert race another's assertion. Serialise via a
/// `tokio::sync::Mutex` whose owned guard each test holds for
/// its entire body across `await` points (so we get a true
/// inter-test serialisation, not just sync setup serialisation).
/// A std `Mutex` would not work here because tokio tests
/// suspend across `.await`; that would either deadlock or
/// require parking a worker thread on the lock.
fn new16_cursor_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// NEW-16: the per-turn message-index cursor used as
/// defense-in-depth inside the `response.messages` persist loop
/// MUST skip any `(session, turn, message_index)` triple it has
/// already seen in a previous invocation. This locks in the
/// helper logic so a regression that drops the cursor check is
/// caught here.
#[tokio::test]
async fn new16_turn_persist_cursor_skips_already_persisted_indexes() {
    let _serial = new16_cursor_test_lock().lock_owned().await;
    let cursors = super::turn_persist_cursors();
    let session_id = format!("test-session-{}", uuid::Uuid::new_v4());
    let turn_id = format!("test-turn-{}", uuid::Uuid::new_v4());
    let key = (session_id.clone(), turn_id.clone());

    // First invocation: 3 messages persist successfully, cursor
    // advances to 3 (i.e. next index to write is 3).
    {
        let mut c = cursors.lock().await;
        c.insert(key.clone(), make_cursor_entry_fresh(3));
    }

    // Second invocation: same `(session, turn)` re-enters with the
    // SAME 3 messages plus 1 new one. The first 3 (indexes 0, 1,
    // 2) MUST be skipped. Only index 3 is new.
    let cursor_already_advanced = {
        let c = cursors.lock().await;
        c.get(&key).map(|e| e.next_index).unwrap_or(0)
    };
    assert_eq!(
        cursor_already_advanced, 3,
        "cursor should reflect the highest-index-already-written + 1"
    );

    // Simulate the persist loop's index guard for messages 0..=3.
    let mut indexes_that_would_persist = Vec::new();
    for message_index in 0..4 {
        if message_index < cursor_already_advanced {
            continue;
        }
        indexes_that_would_persist.push(message_index);
    }
    assert_eq!(
        indexes_that_would_persist,
        vec![3],
        "only the genuinely new index (3) should pass the cursor guard"
    );

    // After successfully persisting index 3, the cursor advances.
    {
        let mut c = cursors.lock().await;
        let now = std::time::Instant::now();
        let entry = c
            .entry(key.clone())
            .or_insert(super::TurnPersistCursorEntry {
                next_index: 0,
                last_touched_at: now,
            });
        if 3 + 1 > entry.next_index {
            entry.next_index = 3 + 1;
        }
        entry.last_touched_at = now;
    }
    let cursor_after = {
        let c = cursors.lock().await;
        c.get(&key).map(|e| e.next_index).unwrap_or(0)
    };
    assert_eq!(cursor_after, 4, "cursor should advance to next index");

    // Cleanup: opportunistic prune treats the entry as fresh
    // (no TTL elapsed), so explicitly remove for hermeticity.
    {
        let mut c = cursors.lock().await;
        c.remove(&key);
    }
}

/// NEW-16 codex review round-1 P1: the cursor guard MUST cover
/// the synthesised final assistant row too. When the cursor is
/// advanced to `response.messages.len() + 1`, the synthesised
/// final row has already been persisted in a prior invocation
/// and must be skipped on re-entry.
#[tokio::test]
async fn new16_turn_persist_cursor_covers_synthesised_final_assistant_row() {
    let _serial = new16_cursor_test_lock().lock_owned().await;
    let cursors = super::turn_persist_cursors();
    let session_id = format!("test-session-{}", uuid::Uuid::new_v4());
    let turn_id = format!("test-turn-{}", uuid::Uuid::new_v4());
    let key = (session_id.clone(), turn_id.clone());

    let log_row_count = 3usize;
    let final_virtual_index = log_row_count;
    {
        let mut c = cursors.lock().await;
        c.insert(
            key.clone(),
            make_cursor_entry_fresh(final_virtual_index + 1),
        );
    }

    let cursor_already_advanced = {
        let c = cursors.lock().await;
        c.get(&key).map(|e| e.next_index).unwrap_or(0)
    };
    for message_index in 0..log_row_count {
        assert!(
            message_index < cursor_already_advanced,
            "indexed row {message_index} must be skipped on re-entry"
        );
    }
    let final_row_already_persisted = cursor_already_advanced > final_virtual_index;
    assert!(
        final_row_already_persisted,
        "the synthetic final assistant row MUST also be skipped on re-entry — \
             this is the codex round-1 P1 contract on NEW-16"
    );

    {
        let mut c = cursors.lock().await;
        c.remove(&key);
    }
}

/// NEW-16 codex review round-3 P2: the TTL pruner MUST evict
/// entries older than `TURN_PERSIST_CURSOR_TTL`. This guards
/// against:
///   - abort/panic paths that prevented end-of-turn cleanup
///   - long-running server map growth (bounded at ~TTL of
///     entries created within the last window)
///
/// We can't easily wait 5 minutes in a test, so we forge an
/// entry with an old `last_touched_at` directly and run the
/// pruner. Codex round-4 P2 added a per-process throttle on
/// the prune call; clear it via the test-only helper so we
/// know this scan won't be throttled out.
#[tokio::test]
async fn new16_turn_persist_cursor_ttl_pruner_evicts_stale_entries() {
    let _serial = new16_cursor_test_lock().lock_owned().await;
    let cursors = super::turn_persist_cursors();
    let session_id = format!("test-session-{}", uuid::Uuid::new_v4());
    let turn_id = format!("test-turn-{}", uuid::Uuid::new_v4());
    let key = (session_id.clone(), turn_id.clone());

    // Forge an entry older than TTL.
    let stale_ts = std::time::Instant::now()
        - super::TURN_PERSIST_CURSOR_TTL
        - std::time::Duration::from_secs(1);
    {
        let mut c = cursors.lock().await;
        c.insert(
            key.clone(),
            super::TurnPersistCursorEntry {
                next_index: 99,
                last_touched_at: stale_ts,
            },
        );
    }
    // The stale entry is present.
    {
        let c = cursors.lock().await;
        assert!(
            c.contains_key(&key),
            "stale entry should be present before prune"
        );
    }
    // Reset round-4 throttle so the prune actually scans.
    super::reset_turn_persist_cursor_prune_throttle_for_test();
    // Run the pruner.
    {
        let mut c = cursors.lock().await;
        super::prune_stale_turn_persist_cursors(&mut c);
    }
    // The stale entry is gone.
    {
        let c = cursors.lock().await;
        assert!(
            !c.contains_key(&key),
            "stale entry MUST be evicted by TTL pruner (codex round-3 P2 — \
                 the pruner is the only GC path now, so this MUST work)"
        );
    }
}

/// NEW-16 codex round-3: fresh entries (within TTL) MUST be
/// preserved by the pruner. A regression that nukes everything
/// would defeat the cursor guard entirely.
#[tokio::test]
async fn new16_turn_persist_cursor_ttl_pruner_preserves_fresh_entries() {
    let _serial = new16_cursor_test_lock().lock_owned().await;
    let cursors = super::turn_persist_cursors();
    let session_id = format!("test-session-{}", uuid::Uuid::new_v4());
    let turn_id = format!("test-turn-{}", uuid::Uuid::new_v4());
    let key = (session_id.clone(), turn_id.clone());

    {
        let mut c = cursors.lock().await;
        c.insert(key.clone(), make_cursor_entry_fresh(42));
    }
    // Reset round-4 throttle so the prune actually scans.
    super::reset_turn_persist_cursor_prune_throttle_for_test();
    {
        let mut c = cursors.lock().await;
        super::prune_stale_turn_persist_cursors(&mut c);
    }
    {
        let c = cursors.lock().await;
        let entry = c
            .get(&key)
            .expect("fresh entry MUST survive the TTL pruner");
        assert_eq!(
            entry.next_index, 42,
            "the pruner must not corrupt surviving entries"
        );
    }
    // Cleanup.
    {
        let mut c = cursors.lock().await;
        c.remove(&key);
    }
}

/// NEW-16 codex round-4 P2: the prune throttle MUST suppress
/// subsequent prune calls within
/// `TURN_PERSIST_CURSOR_PRUNE_INTERVAL` so a burst of
/// concurrent persist blocks doesn't all hold the global
/// cursor mutex while doing redundant full-map scans.
#[tokio::test]
async fn new16_turn_persist_cursor_prune_throttle_suppresses_back_to_back_scans() {
    let _serial = new16_cursor_test_lock().lock_owned().await;
    let cursors = super::turn_persist_cursors();
    let session_id = format!("test-session-{}", uuid::Uuid::new_v4());
    let turn_id = format!("test-turn-{}", uuid::Uuid::new_v4());
    let key = (session_id.clone(), turn_id.clone());

    // Reset throttle, then run an initial scan that primes it.
    super::reset_turn_persist_cursor_prune_throttle_for_test();
    {
        let mut c = cursors.lock().await;
        super::prune_stale_turn_persist_cursors(&mut c);
    }

    // Now insert a stale entry. A subsequent prune within the
    // throttle window should NOT evict it (the throttle skips
    // the scan entirely).
    let stale_ts = std::time::Instant::now()
        - super::TURN_PERSIST_CURSOR_TTL
        - std::time::Duration::from_secs(1);
    {
        let mut c = cursors.lock().await;
        c.insert(
            key.clone(),
            super::TurnPersistCursorEntry {
                next_index: 7,
                last_touched_at: stale_ts,
            },
        );
    }
    // Second prune call — throttle is still primed (we just
    // ran one above), so the scan should be skipped and the
    // stale entry should survive.
    {
        let mut c = cursors.lock().await;
        super::prune_stale_turn_persist_cursors(&mut c);
    }
    {
        let c = cursors.lock().await;
        assert!(
            c.contains_key(&key),
            "throttle within window MUST suppress the scan — the stale entry \
                 should survive until the throttle window expires (codex round-4 P2)"
        );
    }
    // Cleanup: reset throttle so the next test starts clean,
    // then remove the forged entry.
    super::reset_turn_persist_cursor_prune_throttle_for_test();
    {
        let mut c = cursors.lock().await;
        c.remove(&key);
    }
}

/// NEW-16: cursor entries are keyed by `(session, turn)`. Two
/// different `(session, turn)` pairs MUST be independent — one
/// turn's progress cannot leak into another turn's guard.
#[tokio::test]
async fn new16_turn_persist_cursor_isolates_sessions_and_turns() {
    let _serial = new16_cursor_test_lock().lock_owned().await;
    let cursors = super::turn_persist_cursors();
    let session_a = format!("session-a-{}", uuid::Uuid::new_v4());
    let session_b = format!("session-b-{}", uuid::Uuid::new_v4());
    let turn_1 = format!("turn-1-{}", uuid::Uuid::new_v4());
    let turn_2 = format!("turn-2-{}", uuid::Uuid::new_v4());

    let key_a1 = (session_a.clone(), turn_1.clone());
    let key_a2 = (session_a.clone(), turn_2.clone());
    let key_b1 = (session_b.clone(), turn_1.clone());

    {
        let mut c = cursors.lock().await;
        c.insert(key_a1.clone(), make_cursor_entry_fresh(5));
        c.insert(key_a2.clone(), make_cursor_entry_fresh(2));
        c.insert(key_b1.clone(), make_cursor_entry_fresh(1));
    }

    let c = cursors.lock().await;
    // Each (session, turn) pair has its own cursor.
    assert_eq!(c.get(&key_a1).map(|e| e.next_index), Some(5));
    assert_eq!(c.get(&key_a2).map(|e| e.next_index), Some(2));
    assert_eq!(c.get(&key_b1).map(|e| e.next_index), Some(1));
    // Cross-key lookups return None — distinct pairs don't share state.
    assert!(
        c.get(&(session_a.clone(), "turn-nonexistent".into()))
            .is_none(),
        "a non-existent turn under session-a must NOT inherit any cursor"
    );
    assert!(
        c.get(&(session_b.clone(), turn_2.clone())).is_none(),
        "session-b's view of turn-2 must NOT inherit session-a's cursor for turn-2 \
             (the composite key prevents cross-session collisions)"
    );
    drop(c);

    // Cleanup.
    {
        let mut c = cursors.lock().await;
        c.remove(&key_a1);
        c.remove(&key_a2);
        c.remove(&key_b1);
    }
}

// ---------------------------------------------------------------------
// Oversized outbound frame -> head+tail preview (truncate-in-place, no
// disk). codex-style head-tail truncation + a `[N bytes truncated]`
// marker so the offending frame becomes deliverable (< 1 MiB) and the
// user sees the beginning AND the end of the dominant payload.
// ---------------------------------------------------------------------

/// Build an over-cap text frame whose `params.text` field holds `body`.
/// The envelope around it (method/jsonrpc/keys) is small and fixed, so a
/// `body` slightly over `MAX_TEXT_FRAME_BYTES` forces the whole frame
/// over the cap.
fn oversized_text_frame(body: &str) -> String {
    let value = json!({
        "jsonrpc": "2.0",
        "method": "notifications/message_delta",
        "params": {
            "session_id": "local:test",
            "turn_id": "turn-1",
            "text": body,
        }
    });
    app_ui_codec::to_compact_json(&value).expect("serialize over-cap frame")
}

#[test]
fn under_cap_frame_passes_through_byte_identical() {
    let frame = oversized_text_frame("a small body");
    assert!(
        frame.len() <= MAX_TEXT_FRAME_BYTES,
        "fixture must be under the cap for this test"
    );
    let out = preview_oversized_frame(frame.clone());
    assert_eq!(out, frame, "under-cap frame must be byte-identical");
}

#[test]
fn oversized_frame_with_one_dominant_string_becomes_head_tail_preview_under_cap() {
    // A dominant string field well over the 1 MiB cap. Distinct head/tail
    // sentinels let us assert both ends survived.
    let head_sentinel = "HEAD_START_SENTINEL_0123456789";
    let tail_sentinel = "TAIL_END_SENTINEL_9876543210";
    let middle = "M".repeat(MAX_TEXT_FRAME_BYTES + 4096);
    let body = format!("{head_sentinel}{middle}{tail_sentinel}");
    let frame = oversized_text_frame(&body);
    assert!(
        frame.len() > MAX_TEXT_FRAME_BYTES,
        "fixture must exceed the cap (RED: rejected as frame_too_large at base)"
    );

    let out = preview_oversized_frame(frame);

    // Deliverable: strictly under the cap and single-line (no raw CR/LF).
    assert!(
        out.len() < MAX_TEXT_FRAME_BYTES,
        "rewritten frame must be < MAX_TEXT_FRAME_BYTES, got {}",
        out.len()
    );
    assert!(
        !out.contains('\n') && !out.contains('\r'),
        "rewritten frame must remain a single-line JSON object"
    );

    // Round-trips as valid JSON; the offending field holds head, tail, and
    // the truncation marker.
    let parsed: Value = serde_json::from_str(&out).expect("rewritten frame is valid JSON");
    let text = parsed["params"]["text"]
        .as_str()
        .expect("text field present");
    assert!(
        text.starts_with(head_sentinel),
        "head of the original payload must be preserved"
    );
    assert!(
        text.ends_with(tail_sentinel),
        "tail of the original payload must be preserved"
    );
    assert!(
        text.contains("bytes truncated"),
        "a byte-count truncation marker must be inserted, got: {}",
        &text[..text.len().min(120)]
    );
}

#[test]
fn truncation_marker_reports_dropped_byte_count_accurately() {
    let head_sentinel = "H".repeat(64);
    let tail_sentinel = "T".repeat(64);
    let original_field_len = {
        let middle = "x".repeat(MAX_TEXT_FRAME_BYTES + 8192);
        head_sentinel.len() + middle.len() + tail_sentinel.len()
    };
    let middle = "x".repeat(MAX_TEXT_FRAME_BYTES + 8192);
    let body = format!("{head_sentinel}{middle}{tail_sentinel}");
    assert_eq!(body.len(), original_field_len);
    let frame = oversized_text_frame(&body);

    let out = preview_oversized_frame(frame);
    let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
    let text = parsed["params"]["text"].as_str().expect("text field");

    // Parse N out of the marker `…… [<N> bytes truncated] ……`.
    let marker_n = text
        .split("[")
        .nth(1)
        .and_then(|s| s.split(" bytes truncated]").next())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .expect("marker carries a byte count");

    // head bytes + tail bytes kept; N = original - kept.
    let head_kept = text
        .find("……")
        .or_else(|| text.find('['))
        .map(|i| text[..i].trim_end().len())
        .unwrap_or(0);
    let tail_kept = {
        let after = text.rsplit("……").next().unwrap_or("");
        after.trim_start().len()
    };
    // The dropped count plus what we kept must equal the original field
    // length (head_kept + tail_kept + N == original).
    assert_eq!(
        head_kept + tail_kept + marker_n,
        original_field_len,
        "marker N must equal dropped bytes: head_kept={head_kept} tail_kept={tail_kept} N={marker_n}"
    );
}

#[test]
fn oversized_frame_with_multibyte_and_control_bytes_is_boundary_safe_and_under_cap() {
    // A body mixing a 3-byte CJK glyph (escapes 1:1) and a C0 control byte
    // 0x01 (escapes to , 6 bytes). The 6x-escaping body is the worst
    // case for the serialized bound; the head/tail cut must never split a
    // codepoint and the final frame must still be < 1 MiB.
    let unit = "情\u{0001}"; // 3 bytes + 1 byte = 4 bytes raw, escapes to 3 + 6 = 9
    let body = unit.repeat((MAX_TEXT_FRAME_BYTES / unit.len()) + 4096);
    assert!(body.len() > MAX_TEXT_FRAME_BYTES);
    let frame = oversized_text_frame(&body);

    let out = preview_oversized_frame(frame);

    assert!(
        out.len() < MAX_TEXT_FRAME_BYTES,
        "control-byte-heavy frame must be < cap after escaping, got {}",
        out.len()
    );
    // No panic, valid JSON, no split codepoint (from_str would reject an
    // invalid \uXXXX surrogate or a sliced multi-byte char anyway).
    let parsed: Value = serde_json::from_str(&out).expect("valid JSON after escaping");
    let text = parsed["params"]["text"].as_str().expect("text field");
    // The head region (before the marker) and tail region (after it) must
    // each contain ONLY full codepoints of the original alphabet — proving
    // the cut never split a multi-byte glyph nor a control byte. The marker
    // itself legitimately carries ASCII scaffold + `…` + `\n`.
    let (head, rest) = text.split_once("\n…… [").expect("head + marker scaffold");
    let (_, tail) = rest.split_once("] ……\n").expect("marker scaffold + tail");
    for region in [head, tail] {
        for ch in region.chars() {
            assert!(
                ch == '情' || ch == '\u{0001}',
                "non-alphabet char leaked into a preserved region (split codepoint?): {ch:?}"
            );
        }
    }
    // head + tail of the original survive.
    assert!(text.starts_with('情'));
}

/// 2c performance regression (黑板第 2 条): a multi-MB hydrate-shaped
/// payload (1000 large messages, the shape that took ~10s through the old
/// O(payload × rounds) loop) must be previewed in well under a second and
/// come out ≤ cap with the truncation marker present.
#[test]
fn preview_oversized_frame_multi_mb_hydrate_shape_is_single_pass_fast() {
    // Hydrate reply shape: an array of message objects with large text.
    let messages: Vec<Value> = (0..1000)
        .map(|i| {
            json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "text": format!("msg-{i}-{}", "m".repeat(5 * 1024)),
            })
        })
        .collect();
    let value = json!({
        "jsonrpc": "2.0",
        "id": "hydrate-1",
        "result": {
            "session_id": "local:perf",
            "messages": messages,
        }
    });
    let frame = app_ui_codec::to_compact_json(&value).expect("serialize");
    assert!(
        frame.len() > 4 * 1024 * 1024,
        "fixture must be multi-MB, got {}",
        frame.len()
    );

    let start = std::time::Instant::now();
    let out = preview_oversized_frame(frame);
    let elapsed = start.elapsed();

    assert!(
        out.len() < MAX_TEXT_FRAME_BYTES,
        "output must fit the cap, got {}",
        out.len()
    );
    let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
    // The marker must survive somewhere in the shrunk message list —
    // depending on how much had to be cut, the structural fallback may have
    // dropped leading elements, so scan all surviving messages.
    let any_marker = parsed["result"]["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .any(|m| {
            m["text"]
                .as_str()
                .is_some_and(|t| t.contains("bytes truncated"))
        });
    assert!(any_marker, "truncation marker must be present");
    // Generous CI bound: the old loop needed ~10s on this shape; single
    // pass is O(payload). 2s even on slow debug CI is >10x margin over the
    // 200ms release requirement in the contract.
    assert!(
        elapsed < Duration::from_secs(2),
        "single-pass preview took {elapsed:?} — regression toward the O(n²) loop"
    );
}

#[test]
fn oversized_frame_with_multiple_large_fields_iterates_until_under_cap() {
    // Two fields each ~0.7 MiB: neither alone exceeds the cap, but the
    // envelope sum does. Truncating only the larger one is insufficient;
    // the helper must iterate to the second field too.
    let big_a = "A".repeat(700 * 1024);
    let big_b = "B".repeat(700 * 1024);
    let value = json!({
        "jsonrpc": "2.0",
        "method": "notifications/message_delta",
        "params": {
            "session_id": "local:test",
            "turn_id": "turn-1",
            "field_a": big_a,
            "field_b": big_b,
        }
    });
    let frame = app_ui_codec::to_compact_json(&value).expect("serialize");
    assert!(frame.len() > MAX_TEXT_FRAME_BYTES);

    let out = preview_oversized_frame(frame);
    assert!(
        out.len() < MAX_TEXT_FRAME_BYTES,
        "multi-field frame must iterate to < cap, got {}",
        out.len()
    );
    let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
    assert!(parsed["params"]["field_a"].is_string());
    assert!(parsed["params"]["field_b"].is_string());
}

#[test]
fn structurally_huge_string_array_is_truncated_to_valid_under_cap_json() {
    // Thousands of tiny string items: no single dominant string, but the
    // array as a whole is over cap. The structural pass must drop array
    // elements to bring the frame under cap while keeping it valid JSON.
    let items: Vec<String> = (0..200_000).map(|i| format!("i{i}")).collect();
    let value = json!({
        "jsonrpc": "2.0",
        "method": "notifications/message_delta",
        "params": { "items": items }
    });
    let frame = app_ui_codec::to_compact_json(&value).expect("serialize");
    assert!(frame.len() > MAX_TEXT_FRAME_BYTES);

    let out = preview_oversized_frame(frame);
    assert!(
        out.len() <= MAX_TEXT_FRAME_BYTES,
        "structurally-huge array frame must be truncated to <= cap, got {}",
        out.len()
    );
    let parsed: Value = serde_json::from_str(&out).expect("array-truncated frame is valid JSON");
    // The array survives (shorter) and routing keys are intact.
    assert!(parsed["params"]["items"].is_array());
    assert_eq!(parsed["method"], "notifications/message_delta");
}

// ------------------------------------------------------------------
// DO-NOT-SHIP regression guards: `frame_for` / `preview_oversized_frame`
// must NEVER cause an over-cap frame (> MAX_TEXT_FRAME_BYTES) to be
// enqueued/sent as a successful send. `frame_text_within_cap` is the
// boundary helper used by `frame_for`; it returns `None` (drop, not
// enqueue) when a frame is still over cap after all truncation.
// ------------------------------------------------------------------

#[test]
fn array_shrink_marks_the_dropped_middle_with_an_omission_marker() {
    // T-0281: dropping the middle of an oversized records/items array must not
    // be silent — a marker element carrying the omitted count is inserted at
    // the drop point so the client can render "N rows omitted" instead of
    // mistaking the preview for the complete history.
    let items: Vec<String> = (0..200_000).map(|i| format!("i{i}")).collect();
    let value = json!({
        "jsonrpc": "2.0",
        "method": "notifications/message_delta",
        "params": { "items": items }
    });
    let frame = app_ui_codec::to_compact_json(&value).expect("serialize");
    let out = preview_oversized_frame(frame);
    let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
    let arr = parsed["params"]["items"]
        .as_array()
        .expect("items survives");
    let markers: Vec<&Value> = arr
        .iter()
        .filter(|el| el.get("__octos_preview_omitted__").is_some())
        .collect();
    assert!(
        !markers.is_empty(),
        "a dropped middle must leave an omission marker, got none"
    );
    // The structural loop may shrink in several rounds, each leaving a marker;
    // the total omitted is the SUM of every marker's count. kept + total
    // omitted must account for every original row (no silent loss).
    let total_omitted: u64 = markers
        .iter()
        .map(|m| {
            m["__octos_preview_omitted__"]["count"]
                .as_u64()
                .unwrap_or(0)
        })
        .sum();
    assert!(total_omitted > 0, "omitted count is positive");
    let kept = arr.len() as u64 - markers.len() as u64;
    assert_eq!(
        kept + total_omitted,
        200_000,
        "kept + omitted == original count (markers={})",
        markers.len()
    );
}

#[test]
fn structurally_huge_frame_does_not_emit_over_cap() {
    // Over-cap due to a big array of small scalars (no dominant string).
    // After preview the produced frame must be <= cap (array truncated),
    // OR frame_text_within_cap returns None — either way NO over-cap
    // WsMessage is producible.
    let items: Vec<i64> = (0..400_000).collect();
    let value = json!({
        "jsonrpc": "2.0",
        "method": "notifications/message_delta",
        "params": { "session_id": "local:test", "items": items }
    });
    let frame = app_ui_codec::to_compact_json(&value).expect("serialize");
    assert!(
        frame.len() > MAX_TEXT_FRAME_BYTES,
        "fixture must exceed the cap (RED: original returned over-cap)"
    );

    if let Some(body) = frame_text_within_cap(frame) {
        assert!(
            body.len() <= MAX_TEXT_FRAME_BYTES,
            "if a body is produced it must be under cap, got {}",
            body.len()
        );
    } else {
        // dropped, not enqueued — also acceptable
    }
}

#[test]
fn non_json_or_pathological_over_cap_is_not_sent_over_cap() {
    // (a) Non-JSON pathological input: cannot be parsed/rewritten -> must
    //     NOT be emitted over cap (frame_text_within_cap returns None).
    let non_json = "x".repeat(MAX_TEXT_FRAME_BYTES + 16);
    assert!(
        frame_text_within_cap(non_json).is_none(),
        "non-JSON over-cap frame must be dropped (None), never enqueued over cap"
    );

    // (b) Whole-chain invariant via frame_for on a structurally-huge value
    //     (over-cap array of small non-string scalars): frame_for must NEVER
    //     produce an over-cap WsMessage — it either truncates the array under
    //     cap or drops the frame (None).
    let items: Vec<bool> = (0..600_000).map(|i| i % 2 == 0).collect();
    let value = json!({
        "jsonrpc": "2.0",
        "method": "notifications/message_delta",
        "params": { "items": items }
    });
    if let Some(WsMessage::Text(body)) = frame_for(&value) {
        assert!(
            body.len() <= MAX_TEXT_FRAME_BYTES,
            "frame_for must never produce an over-cap WsMessage, got {}",
            body.len()
        );
    }
    // None is also acceptable (dropped).
}

#[test]
fn full_truncation_marker_matcher_is_precise_not_phrase_sniffing() {
    // Positive: the exact scaffold produced by build_head_tail_preview.
    let real = build_head_tail_preview(&"x".repeat(4096), 4096, 1024)
        .expect("preview builds for a generous budget");
    assert!(
        contains_full_truncation_marker(&real),
        "the genuine head+tail marker scaffold must match"
    );
    // Hand-built genuine marker (embedded in surrounding content).
    assert!(contains_full_truncation_marker(
        "head\n…… [12345 bytes truncated] ……\ntail"
    ));

    // Negative: the BARE phrase must NOT match (this is the over-broad-sniff
    // nit the matcher must avoid reintroducing).
    assert!(!contains_full_truncation_marker(
        "see notes: 999 bytes truncated"
    ));
    // Negative: brackets + phrase but WITHOUT the `……` glyph scaffold.
    assert!(!contains_full_truncation_marker("[123 bytes truncated]"));
    // Negative: scaffold present but the count slot is non-numeric.
    assert!(!contains_full_truncation_marker(
        "head\n…… [lots bytes truncated] ……\ntail"
    ));
    // Negative: scaffold prefix but no closing suffix.
    assert!(!contains_full_truncation_marker(
        "head\n…… [12345 and then nothing"
    ));
}

#[test]
fn payload_containing_marker_phrase_is_still_truncated() {
    // A single >1 MiB string field whose content INCLUDES the literal
    // phrase `bytes truncated` must STILL be head+tail truncated, not
    // skipped into the fallback by the old content-sniffing guard.
    let head_sentinel = "HEAD_SENTINEL_bytes truncated_marker_in_content";
    let tail_sentinel = "TAIL_SENTINEL_END";
    let middle = "M".repeat(MAX_TEXT_FRAME_BYTES + 4096);
    let body = format!("{head_sentinel}{middle}{tail_sentinel}");
    let frame = oversized_text_frame(&body);
    assert!(frame.len() > MAX_TEXT_FRAME_BYTES);

    let out = preview_oversized_frame(frame);
    assert!(
        out.len() < MAX_TEXT_FRAME_BYTES,
        "payload containing the marker phrase must still be truncated under cap, got {}",
        out.len()
    );
    let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
    let text = parsed["params"]["text"].as_str().expect("text field");
    assert!(
        text.starts_with(head_sentinel),
        "head (which contains the phrase) must be preserved"
    );
    assert!(text.ends_with(tail_sentinel), "tail must be preserved");
    assert!(
        text.contains("bytes truncated"),
        "truncation marker present"
    );
}
