//! 语音轮（voice turn）STT/TTS 封装。
//!
//! 把 serve/WS turn 路径需要的两件事——"音频→文本"与"文本→音频文件"——
//! 收敛成两个无状态 async 函数，包住共享的 `OminixClient`。turn 状态机
//! （见 `ui_protocol.rs`）只调用这里，不直接碰 ominix。

use std::path::{Path, PathBuf};

use octos_core::{Message, MessageRole};
use octos_llm::ominix::OminixClient;

use crate::config::CloudTtsConfig;

/// 解析 OminiX 服务基址（平台级，env 优先）。与 `api/admin.rs` 的同名 helper 等价；
/// 抽到此处避免跨模块可见性问题。
// TODO(later-tasks): remove dead_code allow once callers are wired up.
#[allow(dead_code)]
fn ominix_base_url() -> String {
    crate::skills_scope::discover_ominix_url()
        .unwrap_or_else(|| "http://127.0.0.1:8081".to_string())
}

/// 从混合媒体路径里挑出音频文件，保持原顺序。
// TODO(later-tasks): remove dead_code allow once callers are wired up.
#[allow(dead_code)]
pub(crate) fn audio_paths(media: &[String]) -> Vec<String> {
    media
        .iter()
        .filter(|p| octos_bus::media::is_audio(p))
        .cloned()
        .collect()
}

/// 转写 turn 内全部音频媒体。无音频时返回空 vec（调用方据此判定是否"语音轮"）。
/// 单条转写失败只记日志并跳过，不让整轮失败。
// TODO(later-tasks): remove dead_code allow once callers are wired up.
#[allow(dead_code)]
pub(crate) async fn transcribe_audio_media(
    media: &[String],
    language: Option<&str>,
) -> Vec<String> {
    let audios = audio_paths(media);
    if audios.is_empty() {
        return Vec::new();
    }
    let client =
        OminixClient::new(&ominix_base_url()).with_language(language.map(|s| s.to_string()));
    let asr_t = std::time::Instant::now();
    let mut out = Vec::new();
    for path in audios {
        match client.transcribe(Path::new(&path)).await {
            Ok(text) if !text.trim().is_empty() => out.push(text),
            Ok(_) => tracing::warn!(audio = %path, "voice_turn: empty transcript, skipping"),
            Err(e) => tracing::warn!(audio = %path, error = %e, "voice_turn: transcription failed"),
        }
    }
    eprintln!(
        "[TIMING] ASR_done dur_ms={} epoch_ms={}",
        asr_t.elapsed().as_millis(),
        now_ms()
    );
    out
}

/// Tools kept active during a voice turn; everything else is deferred.
///
/// A spoken turn is almost always a single-iteration conversational reply that
/// calls no tools, yet the full registry (50+ specs in a skill-rich profile)
/// otherwise dominates the prompt's prefill — measured as the single largest
/// contributor to voice-turn latency (≈half of a 34k-token input). Deferring
/// keeps the tools *recoverable* via `activate_tools`, so a voice query that
/// genuinely needs one pays a single extra round-trip instead of taxing every
/// turn. We keep `activate_tools` itself so that recovery path stays reachable.
const VOICE_TURN_KEEP_TOOLS: &[&str] = &["activate_tools"];

/// Pure core of [`defer_tools_for_voice_turn`]: every registered name that is
/// not on the keep-list. Split out so it is unit-testable without standing up
/// a real `ToolRegistry`.
fn voice_turn_deferred_names(all: &[String], keep: &[&str]) -> Vec<String> {
    all.iter()
        .filter(|name| !keep.contains(&name.as_str()))
        .cloned()
        .collect()
}

/// Whether deferral is safe on this registry: at least one keep-list (recovery)
/// tool is actually registered. Without it, deferring would hide every tool with
/// no `activate_tools` path back, stranding a voice request that genuinely needs
/// one of the remaining allowed tools. Split out for unit-testing.
fn voice_turn_can_defer(all: &[String], keep: &[&str]) -> bool {
    all.iter().any(|name| keep.contains(&name.as_str()))
}

/// Defer every tool except the voice-turn keep-list on a per-turn registry
/// snapshot, so the spoken turn's first LLM call carries a lean tool set.
/// Returns the number of tools deferred. Safe on any registry: `defer` only
/// acts on names that are actually registered. Call on the mutable per-turn
/// snapshot BEFORE it is wrapped in `Arc`.
pub(crate) fn defer_tools_for_voice_turn(registry: &mut octos_agent::ToolRegistry) -> usize {
    let all = registry.tool_names();
    // If the recovery tool isn't registered (e.g. a tool surface small enough to
    // skip auto-defer), deferring everything would leave the first LLM call with
    // no tools AND no `activate_tools` to recover one. Skip deferral entirely.
    if !voice_turn_can_defer(&all, VOICE_TURN_KEEP_TOOLS) {
        return 0;
    }
    let to_defer = voice_turn_deferred_names(&all, VOICE_TURN_KEEP_TOOLS);
    let count = to_defer.len();
    registry.defer(to_defer);
    count
}

/// Whether a char is safe to hand to TTS: letters/digits (incl. CJK),
/// whitespace, and common sentence punctuation. Everything else — emoji,
/// pictographs, math/misc symbols — is dropped, because some on-device engines
/// (GPT-SoVITS) error out on unspeakable input instead of ignoring it.
/// Wall-clock epoch milliseconds (for cross-stage timing in voice turns).
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn is_tts_safe(c: char) -> bool {
    c.is_alphanumeric()
        || c.is_whitespace()
        || matches!(
            c,
            ',' | '.'
                | '!'
                | '?'
                | ';'
                | ':'
                | '\''
                | '"'
                | '-'
                | '('
                | ')'
                | '，'
                | '。'
                | '！'
                | '？'
                | '；'
                | '：'
                | '、'
                | '…'
                | '《'
                | '》'
                | '「'
                | '」'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '%'
        )
}

/// Strip Markdown / formatting noise so the TTS speaks clean prose instead of
/// "swallowing" or mispronouncing symbols. Removes fenced + inline code, link
/// URLs (keeping the visible text), emphasis/heading/list/quote markers, emoji /
/// pictographs / stray symbols, and collapses leftover whitespace.
pub(crate) fn clean_for_tts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue; // drop code-block bodies entirely
        }
        // Strip leading list/quote/heading markers.
        let mut s = trimmed.to_string();
        while let Some(rest) = s
            .strip_prefix("# ")
            .or_else(|| s.strip_prefix("## "))
            .or_else(|| s.strip_prefix("### "))
            .or_else(|| s.strip_prefix("> "))
            .or_else(|| s.strip_prefix("- "))
            .or_else(|| s.strip_prefix("* "))
            .or_else(|| s.strip_prefix("+ "))
        {
            s = rest.to_string();
        }
        out.push_str(&s);
        out.push('\n');
    }

    // `[label](url)` -> `label`; bare emphasis/code chars dropped.
    let mut result = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => {
                // Capture label up to `]`, then skip an optional `(...)`.
                let mut label = String::new();
                for lc in chars.by_ref() {
                    if lc == ']' {
                        break;
                    }
                    label.push(lc);
                }
                if chars.peek() == Some(&'(') {
                    for pc in chars.by_ref() {
                        if pc == ')' {
                            break;
                        }
                    }
                }
                result.push_str(&label);
            }
            '*' | '_' | '`' | '~' | '#' => {} // drop emphasis / code markers
            other if is_tts_safe(other) => result.push(other),
            // Drop emoji / pictographs / stray symbols (😄🌙×🐙 …). Some on-device
            // TTS engines (e.g. GPT-SoVITS) abort synthesis on unspeakable chars
            // rather than skipping them, which fails the whole turn.
            _ => {}
        }
    }

    // Collapse runs of blank lines / spaces.
    result
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

// ── Voice-turn rich output (in-band `[[VISUAL:kind|brief]]` marker) ────────
//
// The fast turn may append a marker after the spoken reply when the model
// decides a visual would help. The backend parses it and dispatches: `html`
// goes to a focused tool-less LLM authoring call (octos-agent `rich_output`),
// the rest to mofa skills. The model never emits a tool call, sidestepping the
// Gemini-3 thought_signature 400.

/// Rich-output kind. `Html` goes to the focused LLM authoring call; the rest
/// go to mofa skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisualKind {
    Html,
    /// Realistic illustration embedded inside interactive HTML (two-stage:
    /// `mofa_image` generates a PNG → inlined into an `author_html` document).
    Illustrated,
    Image,
    Infographic,
}

impl VisualKind {
    fn from_token(s: &str) -> Option<Self> {
        match s.trim() {
            "html" => Some(Self::Html),
            "illustrated" => Some(Self::Illustrated),
            "image" => Some(Self::Image),
            "infographic" => Some(Self::Infographic),
            _ => None,
        }
    }

    /// Wire token for the `visual/generating` event (`kind` field).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Illustrated => "illustrated",
            Self::Image => "image",
            Self::Infographic => "infographic",
        }
    }
}

/// A parsed in-band rich-output directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisualDirective {
    pub kind: VisualKind,
    pub brief: String,
}

/// Parse a trailing `[[VISUAL:kind|brief]]` marker from the model reply.
/// Returns `None` (treat as a plain spoken reply) when absent, the kind is
/// unrecognized, the marker is malformed, or the brief is empty.
pub(crate) fn parse_visual_marker(reply: &str) -> Option<VisualDirective> {
    // The marker is appended AFTER the spoken reply, so only accept it when its
    // closing `]]` ends the (right-trimmed) reply. This stops a mid-reply
    // mention or quote of the `[[VISUAL:...]]` syntax from triggering an artifact.
    let trimmed = reply.trim_end();
    let start = trimmed.rfind("[[VISUAL:")?;
    let after = &trimmed[start + "[[VISUAL:".len()..];
    let end = after.find("]]")?;
    if start + "[[VISUAL:".len() + end + "]]".len() != trimmed.len() {
        return None; // not the trailing marker
    }
    let body = &after[..end];
    let (kind_tok, brief) = body.split_once('|')?;
    let kind = VisualKind::from_token(kind_tok)?;
    let brief = brief.trim().to_string();
    if brief.is_empty() {
        return None;
    }
    Some(VisualDirective { kind, brief })
}

/// The in-band visual marker opener. Streaming holds back only text that is —
/// or could still grow into — this exact prefix, **not** any `[[`, so ordinary
/// bracket notation (e.g. a `[[1]]` citation) never suppresses the rest of the
/// TTS.
const MARKER: &str = "[[VISUAL:";

/// The in-band exit-intent marker (UPCR-2026-025). A fixed, self-contained
/// trailing token the model appends after a farewell when the user wants to end
/// / leave / mute. Held back from TTS and the `message/delta` wire exactly like
/// [`MARKER`]; the actual exit decision is lifted off the final reply content
/// via [`parse_exit_marker`] / [`strip_exit_directive`].
const EXIT_MARKER: &str = "[[EXIT]]";

/// Every in-band control marker held back from TTS / the delta wire. All share
/// the `[[` opener; a trailing partial that could still grow into ANY of them is
/// held back, and a full occurrence of any marks the start of the held region.
/// Ordinary `[[…]]` notation (citations) matches none, so it is still spoken.
const CONTROL_MARKERS: &[&str] = &[MARKER, EXIT_MARKER];

/// Length in bytes of the longest suffix of `s` that is a non-empty prefix of
/// `marker`. Both control markers are ASCII, so any match is at a char boundary
/// (safe to slice).
fn single_marker_prefix_hold(s: &str, marker: &str) -> usize {
    let sb = s.as_bytes();
    let mb = marker.as_bytes();
    let max = mb.len().min(sb.len());
    (1..=max)
        .rev()
        .find(|&n| sb[sb.len() - n..] == mb[..n])
        .unwrap_or(0)
}

/// Length in bytes of the longest trailing partial that could still grow into
/// ANY control marker — those chars are held back from TTS because the next
/// streamed token might complete a marker.
fn marker_prefix_hold(s: &str) -> usize {
    CONTROL_MARKERS
        .iter()
        .map(|m| single_marker_prefix_hold(s, m))
        .max()
        .unwrap_or(0)
}

/// Earliest byte index in `s` of a fully-present control-marker opener (visual
/// or exit), or `None`. Everything from that index onward is held back.
fn find_control_marker(s: &str) -> Option<usize> {
    CONTROL_MARKERS.iter().filter_map(|m| s.find(m)).min()
}

/// Sentence chunking: strong boundaries (。！？!?…；;\n) always split; commas /
/// ideographic commas (soft) split once the segment is >=8 chars (faster first
/// audio). Returns the complete sentences; `buf` keeps the unfinished tail.
/// Module-level twin of the `ui_protocol` inline version, used by
/// [`VoiceReplySplitter`] and unit tests.
fn drain_sentences(buf: &mut String) -> Vec<String> {
    const STRONG: &[char] = &['。', '！', '？', '!', '?', '…', '；', ';', '\n'];
    const SOFT: &[char] = &['，', ',', '、'];
    const SOFT_MIN_CHARS: usize = 8;
    let mut out = Vec::new();
    loop {
        let mut cut = None;
        for (count, (i, c)) in buf.char_indices().enumerate() {
            if STRONG.contains(&c) || (SOFT.contains(&c) && count + 1 >= SOFT_MIN_CHARS) {
                cut = Some(i + c.len_utf8());
                break;
            }
        }
        match cut {
            Some(idx) => {
                let sentence = buf[..idx].trim().to_string();
                *buf = buf[idx..].to_string();
                if !sentence.is_empty() {
                    out.push(sentence);
                }
            }
            None => break,
        }
    }
    out
}

/// Streams speakable text apart from a trailing `[[VISUAL:...]]` marker. Once
/// `[[` appears, everything from there on is held back from TTS (it may be the
/// marker); at the end the directive is parsed from the accumulated full text.
pub(crate) struct VoiceReplySplitter {
    /// Text seen but not yet emitted to TTS. Speakable sentences are drained
    /// from its front each push; a (possibly partial) trailing marker is held
    /// back here until `finish` decides whether it was a real marker.
    pending: String,
    /// All text seen so far (used to parse the trailing marker on `finish`).
    full: String,
}

impl VoiceReplySplitter {
    pub(crate) fn new() -> Self {
        Self {
            pending: String::new(),
            full: String::new(),
        }
    }

    /// Feed a streamed token; returns the complete sentences ready for TTS now.
    pub(crate) fn push(&mut self, token: &str) -> Vec<String> {
        self.full.push_str(token);
        self.pending.push_str(token);

        if let Some(idx) = find_control_marker(&self.pending) {
            // A full control marker (`[[VISUAL:` or `[[EXIT]]`) appeared — treat
            // everything from it onward as the (trailing) marker region and hold
            // it back. Emit the speakable text before it; keep the unfinished
            // pre-marker tail + the held region in `pending` (recovered in
            // `finish` if it turns out not to be a real trailing marker).
            let mut head = self.pending[..idx].to_string();
            let rest = self.pending[idx..].to_string();
            let out = drain_sentences(&mut head);
            self.pending = head;
            self.pending.push_str(&rest);
            return out;
        }

        // No full marker yet: hold back only a trailing partial that could still
        // grow into `[[VISUAL:` (not arbitrary `[[`); drain the rest.
        let hold = marker_prefix_hold(&self.pending);
        let split = self.pending.len() - hold;
        let mut head = self.pending[..split].to_string();
        let held = self.pending[split..].to_string();
        let out = drain_sentences(&mut head);
        self.pending = head;
        self.pending.push_str(&held);
        out
    }

    /// End of stream: returns the remaining speakable tail + parsed directive.
    /// If no real trailing marker parsed, any held-back text is recovered as
    /// speech so nothing is lost to a mid-reply `[[` or stray partial.
    pub(crate) fn finish(self) -> (Option<String>, Option<VisualDirective>) {
        let directive = parse_visual_marker(&self.full);
        // Drop ALL real trailing control markers (visual and/or exit, in either
        // order) from the spoken tail so neither reaches TTS — a stacked
        // `…[[VISUAL:…]][[EXIT]]` must peel both. A held-back region that turned
        // out NOT to be a real trailing marker (a rare mid-reply `[[`) is left
        // intact and recovered as speech. The visual directive is returned for
        // dispatch; the exit decision is lifted separately off the final content
        // (`strip_control_directives`).
        let mut speak: &str = self.pending.as_str();
        loop {
            if parse_visual_marker(speak).is_some() {
                speak = strip_visual_marker(speak);
                continue;
            }
            if parse_exit_marker(speak) {
                speak = strip_exit_marker(speak);
                continue;
            }
            break;
        }
        let speak = speak.trim();
        let tail = if speak.is_empty() {
            None
        } else {
            Some(speak.to_string())
        };
        (tail, directive)
    }
}

/// Drop a trailing `[[VISUAL:...]]` marker, returning the speakable prefix. For
/// the **non-streamed fallback** (whole-reply synth) so the marker isn't read
/// aloud as text. Only a TRAILING marker is stripped (consistent with
/// [`parse_visual_marker`]); a mid-reply mention is left intact.
pub(crate) fn strip_visual_marker(reply: &str) -> &str {
    if parse_visual_marker(reply).is_some() {
        let i = reply.rfind("[[VISUAL:").expect("trailing marker present");
        reply[..i].trim_end()
    } else {
        reply
    }
}

/// Whether the model reply carries a TRAILING `[[EXIT]]` control marker
/// (UPCR-2026-025). Trailing-only: the marker must end the right-trimmed reply,
/// so a mid-reply mention / quote of the syntax never triggers an exit (mirrors
/// [`parse_visual_marker`]).
pub(crate) fn parse_exit_marker(reply: &str) -> bool {
    let trimmed = reply.trim_end();
    match trimmed.rfind(EXIT_MARKER) {
        Some(start) => start + EXIT_MARKER.len() == trimmed.len(),
        None => false,
    }
}

/// Drop a trailing `[[EXIT]]` marker, returning the speakable prefix. Only a
/// TRAILING marker is stripped (consistent with [`parse_exit_marker`]); a
/// mid-reply mention is left intact.
pub(crate) fn strip_exit_marker(reply: &str) -> &str {
    if parse_exit_marker(reply) {
        let i = reply
            .rfind(EXIT_MARKER)
            .expect("trailing exit marker present");
        reply[..i].trim_end()
    } else {
        reply
    }
}

/// Remove EVERY `[[VISUAL:...]]` span from `s` (not just a trailing one).
///
/// Unlike [`strip_visual_marker`] (which only drops a trailing directive and
/// preserves a mid-text mention), this scrubs the marker wherever it appears —
/// used for sanitizing free-form text that may have folded a marker in at an
/// arbitrary position, e.g. a pre-fix compaction summary. An unterminated
/// `[[VISUAL:` drops the remainder.
pub(crate) fn remove_all_visual_markers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("[[VISUAL:") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find("]]") {
            Some(end) => rest = &after[end + "]]".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Lift the trailing `[[VISUAL:...]]` directive out of the turn's authoritative
/// reply surfaces and strip it in place, so the internal control protocol never
/// reaches the WIRE (`message/delta` from `done`, `message/persisted`) or
/// STORAGE (session JSONL). Strips both `content` (the final `response.content`)
/// and every Assistant carrier in `messages` that ends with the same trailing
/// marker. Returns the parsed directive for background dispatch, or `None` (and
/// leaves everything intact) when there is no real trailing marker. The frontend
/// learns a visual is coming from the typed `visual/generating` event instead.
pub(crate) fn strip_visual_directive(
    content: &mut String,
    messages: &mut [Message],
) -> Option<VisualDirective> {
    let directive = parse_visual_marker(content)?;
    *content = strip_visual_marker(content).to_string();
    for message in messages.iter_mut() {
        if message.role == MessageRole::Assistant {
            let stripped = strip_visual_marker(&message.content);
            if stripped.len() != message.content.len() {
                message.content = stripped.to_string();
            }
        }
    }
    Some(directive)
}

/// Lift the trailing `[[EXIT]]` control marker out of the turn's authoritative
/// reply surfaces and strip it in place, so the internal control protocol never
/// reaches the WIRE (`message/delta`, `message/persisted`) or STORAGE (session
/// JSONL). Strips both `content` (the final `response.content`) and every
/// Assistant carrier in `messages` whose trailing marker matches. Returns `true`
/// when a real trailing marker was found and removed (the caller then emits the
/// typed `voice/exit` event), or `false` (leaving everything intact) otherwise.
/// Mirrors [`strip_visual_directive`]; the client learns to leave the voice
/// screen from the typed `voice/exit` event, not from the marker text.
pub(crate) fn strip_exit_directive(content: &mut String, messages: &mut [Message]) -> bool {
    if !parse_exit_marker(content) {
        return false;
    }
    *content = strip_exit_marker(content).to_string();
    for message in messages.iter_mut() {
        if message.role == MessageRole::Assistant {
            let stripped = strip_exit_marker(&message.content);
            if stripped.len() != message.content.len() {
                message.content = stripped.to_string();
            }
        }
    }
    true
}

/// Strip STACKED trailing control markers ([[VISUAL:...]] and/or [[EXIT]]) from
/// the turn's authoritative reply surfaces in EITHER order, returning the parsed
/// visual directive (if any) and whether an exit was requested.
///
/// A reply may end with both markers (e.g. `…[[VISUAL:…]][[EXIT]]`): stripping
/// only the outermost would leave the inner one trailing on the wire
/// (`message/delta`, `message/persisted`) and in storage (session JSONL), and —
/// for the visual case — never dispatch it. So this loops, peeling whichever
/// marker is currently trailing, until neither is. Reuses (and supersedes on the
/// turn path) the per-marker [`strip_visual_directive`] / [`strip_exit_directive`].
pub(crate) fn strip_control_directives(
    content: &mut String,
    messages: &mut [Message],
) -> (Option<VisualDirective>, bool) {
    let mut directive = None;
    let mut exit = false;
    loop {
        if let Some(d) = strip_visual_directive(content, messages) {
            if directive.is_none() {
                directive = Some(d);
            }
            continue;
        }
        if strip_exit_directive(content, messages) {
            exit = true;
            continue;
        }
        break;
    }
    (directive, exit)
}

/// Byte length of the marker-free visible prefix of `full`: everything up to a
/// (possibly mid-reply) `[[VISUAL:` occurrence, else everything minus a trailing
/// partial that could still grow into the marker. Trailing whitespace before
/// that cut is also held back — the marker convention puts it on its own line,
/// so the preceding newline would otherwise leak as a blank line. Cut points
/// fall on char boundaries (`MARKER` is ASCII; `trim_end` is boundary-safe).
fn visible_prefix_len(full: &str) -> usize {
    let cut = match find_control_marker(full) {
        Some(i) => i,
        None => full.len() - marker_prefix_hold(full),
    };
    full[..cut].trim_end().len()
}

/// Token-granular twin of [`VoiceReplySplitter`] for the **UI message delta**
/// stream of a voice turn: emits the reply text token-by-token while holding
/// back any trailing (or still-forming) `[[VISUAL:...]]` marker, so the live
/// `message/delta` wire never carries the internal control protocol (the
/// durable surfaces are stripped separately by [`strip_visual_directive`]).
pub(crate) struct VisibleDeltaFilter {
    /// All text seen so far.
    full: String,
    /// Byte count already emitted as deltas (monotonic).
    emitted: usize,
}

impl VisibleDeltaFilter {
    pub(crate) fn new() -> Self {
        Self {
            full: String::new(),
            emitted: 0,
        }
    }

    /// Feed a streamed token; returns the newly-visible marker-free text to emit
    /// as a delta now (empty when the token only extended a held-back marker).
    pub(crate) fn push(&mut self, token: &str) -> String {
        self.full.push_str(token);
        let visible = visible_prefix_len(&self.full);
        if visible > self.emitted {
            let out = self.full[self.emitted..visible].to_string();
            self.emitted = visible;
            out
        } else {
            String::new()
        }
    }

    /// End of stream: returns any held-back text that turned out NOT to be a
    /// real trailing marker (a rare mid-reply `[[VISUAL:` / `[[EXIT]]` quote), so
    /// nothing is lost. A real trailing control marker (visual or exit) yields
    /// `""` for that span (stays off the wire).
    pub(crate) fn finish(self) -> String {
        // Peel ALL real trailing control markers (visual and/or exit, in either
        // order) so a stacked `…[[VISUAL:…]][[EXIT]]` never leaks the inner one
        // onto the `message/delta` wire.
        let mut clean: &str = self.full.as_str();
        loop {
            if parse_visual_marker(clean).is_some() {
                clean = strip_visual_marker(clean);
                continue;
            }
            if parse_exit_marker(clean) {
                clean = strip_exit_marker(clean);
                continue;
            }
            break;
        }
        let visible = clean.len();
        if visible > self.emitted {
            self.full[self.emitted..visible].to_string()
        } else {
            String::new()
        }
    }
}

/// Maps a `kind` to a mofa tool name + input args. `Html` is not a skill →
/// `None`.
fn image_skill_call(
    d: &VisualDirective,
    out_dir: &Path,
) -> Option<(&'static str, serde_json::Value)> {
    let out = out_dir.to_string_lossy().to_string();
    // A UNIQUE output filename per dispatch (#1477 follow-up): the mofa skill
    // caches by output path (`is_cached`: returns the existing file when it is
    // >10KB, skipping generation). A fixed name like `image.png` therefore made
    // every turn after the first in a session return the PREVIOUS turn's image
    // (and skip generation entirely, so reference frames were never applied).
    let uniq = uuid::Uuid::now_v7();
    match d.kind {
        VisualKind::Infographic => Some((
            "mofa_infographic",
            serde_json::json!({
                "sections": [{ "prompt": d.brief }],
                "out": format!("{out}/infographic-{uniq}.png"),
            }),
        )),
        // `mofa_image` (not `mofa_cards`): a plain "generate an image" request
        // wants a single picture, and — unlike `mofa_cards`, which emits no
        // `files_to_send` (octos #1041, see `workspace_policy` test) so the
        // backend would get empty rels and deliver nothing — `mofa_image`
        // reports its produced PNG via `files_to_send`, the same proven path
        // the Illustrated stage-1 call relies on.
        VisualKind::Image => Some((
            "mofa_image",
            serde_json::json!({
                "prompt": d.brief,
                "out": format!("{out}/image-{uniq}.png"),
            }),
        )),
        // Html (focused LLM call) and Illustrated (two-stage: run_illustration_image
        // then author_html) are not direct file-delivering skills.
        VisualKind::Html | VisualKind::Illustrated => None,
    }
}

/// Illustrated path, stage 1: generate one PNG via `mofa_image` and return the
/// **absolute** produced path so the caller can read + inline its bytes into the
/// HTML (the PNG itself is not delivered as a separate artifact). `None` when the
/// skill is missing / failed / produced no file.
///
/// `ref_images` (e.g. this turn's camera frame) are forwarded to ground the
/// illustration on the real subject; omitted from the args entirely when empty.
pub(crate) async fn run_illustration_image(
    registry: &std::sync::Arc<octos_agent::ToolRegistry>,
    brief: &str,
    out_dir: &Path,
    ref_images: &[String],
) -> Option<PathBuf> {
    // Unique output filename per dispatch — the mofa skill caches by output
    // path (`is_cached`), so a fixed `illustration.png` returned the prior
    // turn's image and skipped generation (hence reference frames were never
    // applied). See `image_skill_call`.
    let out = out_dir
        .join(format!("illustration-{}.png", uuid::Uuid::now_v7()))
        .to_string_lossy()
        .into_owned();
    let mut args = serde_json::json!({
        "prompt": brief,
        "out": out,
    });
    if !ref_images.is_empty() {
        args["ref_images"] = serde_json::json!(ref_images);
    }
    // #1477 P2: route through the registry (provider policy + arg-size limit +
    // deferred-tool auto-activation) rather than a bare `tool.execute()`, so the
    // background visual call is governed like any other tool execution. `Err`
    // covers skill-not-installed, policy-denied, and generation failure alike.
    match registry.execute("mofa_image", &args).await {
        Ok(res) => res.files_to_send.into_iter().next(),
        Err(e) => {
            tracing::warn!(tool = "mofa_image", error = %e, "voice rich: illustration gen failed");
            None
        }
    }
}

/// Backend orchestration: fetch the mofa skill for `kind` and run it directly
/// (a spawn_only skill is awaited synchronously here — the caller is already in
/// its own `tokio::spawn`, latency-insensitive), returning the produced files'
/// relative names. Skill not installed / execution failed → empty vec. The
/// caller delivers them via files_attached.
pub(crate) async fn run_image_skill(
    registry: &std::sync::Arc<octos_agent::ToolRegistry>,
    d: &VisualDirective,
    out_dir: &Path,
) -> Vec<String> {
    let Some((name, args)) = image_skill_call(d, out_dir) else {
        return Vec::new();
    };
    // #1477 P2: route through the registry (policy + arg-size + auto-activation)
    // instead of a bare `tool.execute()`. `Err` = not installed / denied / failed.
    match registry.execute(name, &args).await {
        Ok(res) => res
            .files_to_send
            .iter()
            .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .collect(),
        Err(e) => {
            tracing::warn!(tool = name, error = %e, "voice rich: image skill failed");
            Vec::new()
        }
    }
}

/// Ensure the text ends on a *strong* terminal boundary so the TTS engine
/// fully renders the final syllable.
///
/// On-device GPT-SoVITS (and similar) clips the last character when the input
/// ends on a soft pause mark (comma / 顿号 / 分号 / 冒号) or a bare content
/// char: it reads the trailing pause as "more is coming" and stops generating
/// mid-syllable, so e.g. `"…往下跳，"` comes back as audio that cuts off just
/// as `跳` begins. The streamed-reply chunker (`drain_voice_sentences`) emits
/// comma-terminated fragments, and the reply's trailing fragment often has no
/// punctuation at all — both hit this. Normalising the tail to a full stop
/// gives a clean sentence-final boundary the engine renders in full.
///
/// Strong terminals already present (`。！？!?…`) are left untouched.
fn ensure_terminal_punctuation(text: &str) -> String {
    const STRONG: &[char] = &['。', '！', '？', '!', '?', '…', '.'];
    const SOFT: &[char] = &['，', ',', '、', '；', ';', '：', ':'];

    let mut s = text.trim_end().to_string();
    if s.is_empty() {
        return s;
    }
    match s.chars().next_back() {
        Some(c) if STRONG.contains(&c) => return s,
        _ => {}
    }
    // Drop any trailing run of soft pause marks / whitespace, then append one
    // full stop so a comma-ended (or bare) fragment gets a real boundary.
    while let Some(c) = s.chars().next_back() {
        if SOFT.contains(&c) || c.is_whitespace() {
            s.pop();
        } else {
            break;
        }
    }
    s.push('。');
    s
}

/// Volcano Engine (ByteDance) cloud-TTS config, sourced from env. Returns
/// `None` (→ fall back to on-device ominix) unless appid + token are set.
/// Moving TTS to the cloud also stops ominix from thrashing ASR↔TTS model
/// reloads under memory pressure, so on-device STT stays fast.
struct VolcanoTts {
    appid: String,
    token: String,
    cluster: String,
    voice: String,
    encoding: String,
    endpoint: String,
}

/// Whether the route wants the cloud path. Legacy `volcano` aliases `cloud`.
fn wants_cloud(provider: &str) -> bool {
    matches!(provider, "auto" | "cloud" | "volcano")
}

/// Pure core: merge typed (non-secret) cloud config over env fallbacks, applying
/// engine defaults. Requires a non-empty token AND a resolvable appid.
fn build_volcano(
    token: Option<String>,
    cloud: Option<&CloudTtsConfig>,
    env_appid: Option<String>,
    env_cluster: Option<String>,
    env_voice: Option<String>,
    env_encoding: Option<String>,
    env_endpoint: Option<String>,
) -> Option<VolcanoTts> {
    let token = token.filter(|s| !s.is_empty())?;
    let pick = |typed: Option<&String>, env: Option<String>| -> Option<String> {
        typed
            .filter(|s| !s.is_empty())
            .cloned()
            .or_else(|| env.filter(|s| !s.is_empty()))
    };
    let appid = pick(cloud.and_then(|c| c.appid.as_ref()), env_appid)?;
    let endpoint = pick(cloud.and_then(|c| c.endpoint.as_ref()), env_endpoint)
        .unwrap_or_else(|| "https://openspeech.bytedance.com/api/v1/tts".to_string());
    // The endpoint is partly tenant-controlled (per-profile `tts_cloud.endpoint`)
    // and the token may be the host-global `VOLC_TTS_TOKEN`. Never send the token
    // anywhere but an HTTPS Volcano host — otherwise a tenant could point the
    // endpoint at an internal/attacker address and exfiltrate the token (SSRF).
    if !is_allowed_volcano_endpoint(&endpoint) {
        tracing::warn!(
            endpoint = %endpoint,
            "voice_turn: refusing cloud TTS — endpoint not in the HTTPS Volcano allowlist; token NOT sent"
        );
        return None;
    }
    Some(VolcanoTts {
        appid,
        token,
        cluster: pick(cloud.and_then(|c| c.cluster.as_ref()), env_cluster)
            .unwrap_or_else(|| "volcano_tts".to_string()),
        voice: pick(cloud.and_then(|c| c.voice.as_ref()), env_voice)
            .unwrap_or_else(|| "BV001_streaming".to_string()),
        encoding: pick(cloud.and_then(|c| c.encoding.as_ref()), env_encoding)
            .unwrap_or_else(|| "mp3".to_string()),
        endpoint,
    })
}

/// HTTPS Volcano TTS hosts the token may be sent to. Keep this tight — it is the
/// SSRF / token-exfiltration boundary for the partly tenant-controlled endpoint.
const VOLCANO_ALLOWED_HOSTS: &[&str] = &["openspeech.bytedance.com"];

/// True only for an `https://` URL whose host is in [`VOLCANO_ALLOWED_HOSTS`].
fn is_allowed_volcano_endpoint(endpoint: &str) -> bool {
    match reqwest::Url::parse(endpoint) {
        Ok(u) => {
            u.scheme() == "https"
                && u.host_str()
                    .is_some_and(|h| VOLCANO_ALLOWED_HOSTS.contains(&h))
        }
        Err(_) => false,
    }
}

/// Resolve a Volcano config from typed per-profile cloud settings + env token.
fn resolve_volcano(cloud: Option<&CloudTtsConfig>) -> Option<VolcanoTts> {
    let env = |k: &str| std::env::var(k).ok();
    // Token precedence: the runtime-resolved per-profile token (from `env_vars`,
    // set by `ProfileRuntime::bootstrap`) wins; fall back to the process env for
    // legacy pure-`export` setups.
    let token = cloud
        .and_then(|c| c.token.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| env("VOLC_TTS_TOKEN"));
    build_volcano(
        token,
        cloud,
        env("VOLC_TTS_APPID"),
        env("VOLC_TTS_CLUSTER"),
        env("VOLC_TTS_VOICE"),
        env("VOLC_TTS_ENCODING"),
        env("VOLC_TTS_ENDPOINT"),
    )
}

/// Process-wide HTTP client for Volcano TTS, with redirects DISABLED. Even though the
/// endpoint is allowlisted to an HTTPS Volcano host, that host could still
/// respond with a 3xx to an off-allowlist address; `reqwest` would otherwise
/// follow it and 307/308 preserve the POST body — replaying the token to the
/// redirect target. `Policy::none()` makes a redirect a terminal response we
/// never follow, closing that exfiltration path. The client is reused across
/// per-sentence syntheses so each sentence does not pay a fresh TCP+TLS
/// handshake to Volcano.
fn volcano_http_client() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano client build failed"))
                .ok()
        })
        .as_ref()
}

/// Synthesize via Volcano Engine HTTP TTS (non-streaming `operation:"query"`,
/// returns base64 audio in JSON). Writes the decoded audio to `out_dir` and
/// returns the path. `None` on any failure (caller falls back to ominix).
async fn synthesize_volcano(cfg: &VolcanoTts, text: &str, out_dir: &Path) -> Option<PathBuf> {
    use base64::Engine;

    let reqid = uuid::Uuid::now_v7().to_string();
    let body = serde_json::json!({
        "app": { "appid": cfg.appid, "token": cfg.token, "cluster": cfg.cluster },
        "user": { "uid": "octos-voice" },
        "audio": { "voice_type": cfg.voice, "encoding": cfg.encoding, "speed_ratio": 1.0 },
        "request": { "reqid": reqid, "text": text, "operation": "query", "text_type": "plain" },
    });

    let client = volcano_http_client()?;
    let resp = client
        .post(&cfg.endpoint)
        // Volcano's quirky scheme: literal "Bearer;" + token (semicolon, no space).
        .header("Authorization", format!("Bearer;{}", cfg.token))
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS request failed"))
        .ok()?;

    let json: serde_json::Value = resp
        .json()
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS bad JSON"))
        .ok()?;

    // code 3000 == success per Volcano TTS API.
    if json.get("code").and_then(|c| c.as_i64()) != Some(3000) {
        tracing::warn!(resp = %json, "voice_turn: volcano TTS non-success code");
        return None;
    }
    let b64 = json.get("data").and_then(|d| d.as_str())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS base64 decode"))
        .ok()?;
    if bytes.is_empty() {
        return None;
    }

    let ext = if cfg.encoding == "wav" { "wav" } else { "mp3" };
    let out_path = out_dir.join(format!("reply-{reqid}.{ext}"));
    tokio::fs::write(&out_path, &bytes)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano TTS write failed"))
        .ok()?;
    Some(out_path)
}

/// Synthesize a reply to an audio file, picking the TTS route from `provider`:
/// - `"auto"`: cloud Volcano when a token resolves, else on-device.
/// - `"cloud"` (alias `"volcano"`): force cloud Volcano; falls back to
///   on-device when the token/appid is missing or the request fails.
/// - `"local"` (or any other value, incl. legacy `"sovits"`/`"qwen3"`):
///   on-device synthesis using the default engine.
///
/// `voice` is the on-device voice preset (voices.json); the cloud route uses
/// its own `VOLC_TTS_VOICE` env instead. Returns `None` on failure.
/// Streaming cloud-TTS variant: synthesize `text` over the Volcano v1 ws
/// `submit` protocol and invoke `on_chunk(bytes, is_last, mime)` for each audio
/// frame as it arrives. Returns `Some(())` when the cloud stream completed (the
/// caller delivered audio progressively and should NOT also write a file), or
/// `None` when the request is not cloud-routed / env is missing / the stream
/// failed — in which case the caller falls back to [`synthesize_reply`].
///
/// Only the cloud (`"auto"`/`"cloud"`/`"volcano"`) route streams; on-device
/// engines keep the whole-file path. Voice/speed come from the same resolved
/// cloud config as the non-streaming cloud path.
pub(crate) async fn synthesize_reply_streaming(
    text: &str,
    provider: &str,
    cloud: Option<&CloudTtsConfig>,
    mut on_chunk: impl FnMut(&[u8], bool, &str),
) -> Option<()> {
    if !wants_cloud(provider) {
        return None;
    }
    let speak = ensure_terminal_punctuation(&clean_for_tts(text));
    if speak.trim().is_empty() {
        return None;
    }
    let cfg = resolve_volcano(cloud)?;
    let mime = match cfg.encoding.as_str() {
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        "ogg_opus" => "audio/ogg",
        _ => "audio/mpeg",
    };
    crate::api::volcano_ws::synthesize_ws_stream(
        &cfg.appid,
        &cfg.token,
        &cfg.cluster,
        &cfg.voice,
        &cfg.encoding,
        &speak,
        |bytes, last| on_chunk(bytes, last, mime),
    )
    .await
}

pub(crate) async fn synthesize_reply(
    text: &str,
    voice: &str,
    provider: &str,
    cloud: Option<&CloudTtsConfig>,
    out_dir: &Path,
) -> Option<PathBuf> {
    let speak = clean_for_tts(text);
    if speak.trim().is_empty() {
        return None;
    }
    // Normalise the tail to a strong terminal so engines (notably on-device
    // GPT-SoVITS) don't clip the final syllable of comma-ended / bare fragments.
    let speak = ensure_terminal_punctuation(&speak);
    let tts_t = std::time::Instant::now();
    eprintln!("[TIMING] TTS_start epoch_ms={}", now_ms());

    // Cloud route. "auto" uses cloud only when env is present; "cloud"/"volcano"
    // force it (still falls back to on-device on failure). Cloud is faster (no
    // on-device model reload) and higher quality when available.
    let want_cloud = wants_cloud(provider);
    if want_cloud {
        if let Some(cfg) = resolve_volcano(cloud) {
            if let Some(path) = synthesize_volcano(&cfg, &speak, out_dir).await {
                return Some(path);
            }
            tracing::warn!("voice_turn: volcano TTS failed; falling back to ominix");
        } else if provider == "cloud" || provider == "volcano" {
            tracing::warn!(
                provider = %provider,
                "voice_turn: tts cloud route but VOLC_TTS_TOKEN/appid missing; falling back to on-device"
            );
        }
    }

    // On-device route: always the default engine (no qwen3 UI split).
    let engine = "sovits";
    let out_path = out_dir.join(format!("reply-{}.wav", uuid::Uuid::now_v7()));
    let client = OminixClient::new(&ominix_base_url());
    match client
        .synthesize_to_file(&speak, voice, engine, None, &out_path)
        .await
    {
        Ok(_) => {
            eprintln!(
                "[TIMING] TTS_done dur_ms={} epoch_ms={}",
                tts_t.elapsed().as_millis(),
                now_ms()
            );
            Some(out_path)
        }
        Err(e) => {
            tracing::warn!(error = %e, "voice_turn: synthesis failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CloudTtsConfig;

    #[test]
    fn should_want_cloud_for_auto_cloud_and_legacy_volcano() {
        for p in ["auto", "cloud", "volcano"] {
            assert!(wants_cloud(p), "{p} should want cloud");
        }
        for p in ["local", "sovits", "qwen3", ""] {
            assert!(!wants_cloud(p), "{p} should NOT want cloud");
        }
    }

    #[test]
    fn should_return_none_when_token_missing() {
        let cloud = CloudTtsConfig {
            appid: Some("1".into()),
            ..Default::default()
        };
        assert!(build_volcano(None, Some(&cloud), None, None, None, None, None).is_none());
    }

    #[test]
    fn should_prefer_typed_cloud_over_env_and_apply_defaults() {
        let cloud = CloudTtsConfig {
            appid: Some("typed".into()),
            voice: Some("BV700".into()),
            ..Default::default()
        };
        let v = build_volcano(
            Some("tok".into()),
            Some(&cloud),
            Some("envid".into()), // typed appid wins
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(v.appid, "typed");
        assert_eq!(v.voice, "BV700");
        assert_eq!(v.cluster, "volcano_tts"); // default
        assert_eq!(v.encoding, "mp3"); // default
        assert_eq!(v.endpoint, "https://openspeech.bytedance.com/api/v1/tts");
        assert_eq!(v.token, "tok");
    }

    #[test]
    fn should_fall_back_to_env_when_cloud_none() {
        let v = build_volcano(
            Some("tok".into()),
            None,
            Some("envid".into()),
            Some("clu".into()),
            Some("envvoice".into()),
            Some("wav".into()),
            Some("https://openspeech.bytedance.com/api/v1/tts".into()),
        )
        .unwrap();
        assert_eq!(v.token, "tok");
        assert_eq!(v.appid, "envid");
        assert_eq!(v.voice, "envvoice");
        assert_eq!(v.cluster, "clu");
        assert_eq!(v.encoding, "wav");
        assert_eq!(v.endpoint, "https://openspeech.bytedance.com/api/v1/tts");
    }

    #[test]
    fn should_return_none_when_no_appid_anywhere() {
        assert!(build_volcano(Some("tok".into()), None, None, None, None, None, None).is_none());
    }

    #[test]
    fn should_reject_non_volcano_endpoint_to_prevent_ssrf() {
        // A tenant-controlled endpoint pointing off the Volcano allowlist must
        // never receive the token — build_volcano returns None.
        let cloud = CloudTtsConfig {
            appid: Some("1".into()),
            endpoint: Some("https://attacker.example/tts".into()),
            ..Default::default()
        };
        assert!(
            build_volcano(
                Some("tok".into()),
                Some(&cloud),
                None,
                None,
                None,
                None,
                None
            )
            .is_none()
        );
    }

    #[test]
    fn should_reject_http_volcano_endpoint() {
        // Even the right host over plain http is rejected (must be https).
        assert!(!is_allowed_volcano_endpoint(
            "http://openspeech.bytedance.com/api/v1/tts"
        ));
        assert!(!is_allowed_volcano_endpoint(
            "https://evil.openspeech.bytedance.com.attacker.com/"
        ));
        assert!(!is_allowed_volcano_endpoint("not a url"));
    }

    #[test]
    fn should_allow_default_volcano_endpoint() {
        assert!(is_allowed_volcano_endpoint(
            "https://openspeech.bytedance.com/api/v1/tts"
        ));
    }

    #[tokio::test]
    async fn volcano_client_does_not_follow_redirects() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // A one-shot server that answers every connection with a 307 to another
        // host. A client that followed redirects would replay the POST there;
        // ours must instead surface the 307 as a terminal response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let resp = "HTTP/1.1 307 Temporary Redirect\r\n\
                    Location: http://attacker.example/steal\r\n\
                    Content-Length: 0\r\n\r\n";
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        let client = volcano_http_client().expect("client builds");
        let resp = client
            .post(format!("http://{addr}/"))
            .body("token=secret")
            .send()
            .await
            .expect("request completes");
        assert_eq!(
            resp.status().as_u16(),
            307,
            "redirects must NOT be followed (token would leak to the target)"
        );
    }

    #[test]
    fn volcano_client_is_reused_across_calls() {
        // Per-sentence synthesis must not rebuild the HTTP client each time —
        // a fresh client per sentence pays a new TCP+TLS handshake to bytedance.
        // A shared process-wide client returns the same instance every call.
        let a = volcano_client();
        let b = volcano_client();
        assert!(
            std::ptr::eq(a, b),
            "volcano TTS client should be a shared instance, not rebuilt per call"
        );
    }

    #[tokio::test]
    async fn synthesize_reply_returns_none_for_blank_text() {
        let dir = std::env::temp_dir();
        let got = synthesize_reply("   ", "vivian", "auto", None, &dir).await;
        assert!(got.is_none());
    }

    #[test]
    fn trailing_comma_becomes_full_stop_so_final_syllable_is_not_clipped() {
        // GPT-SoVITS clips the last syllable when a chunk ends on a soft comma
        // (it reads the trailing pause as "more coming" and stops generating
        // mid-syllable). Normalising to a full stop gives a clean sentence-final
        // boundary so the engine renders the whole last character.
        assert_eq!(
            ensure_terminal_punctuation("就每天背着壳爬到山顶往下跳，"),
            "就每天背着壳爬到山顶往下跳。"
        );
    }

    #[test]
    fn bare_ending_gets_terminal_punctuation() {
        // The reply's trailing fragment often has no punctuation at all; a bare
        // content char is just as prone to clipping as a trailing comma.
        assert_eq!(
            ensure_terminal_punctuation("好的我知道了"),
            "好的我知道了。"
        );
    }

    #[test]
    fn strong_terminal_is_left_unchanged() {
        assert_eq!(ensure_terminal_punctuation("你好！"), "你好！");
        assert_eq!(ensure_terminal_punctuation("真的吗？"), "真的吗？");
        assert_eq!(ensure_terminal_punctuation("等等…"), "等等…");
        assert_eq!(ensure_terminal_punctuation("Okay."), "Okay.");
    }

    #[test]
    fn trailing_soft_marks_and_whitespace_collapse_to_one_full_stop() {
        assert_eq!(ensure_terminal_punctuation("走吧， "), "走吧。");
        assert_eq!(ensure_terminal_punctuation("等一下；"), "等一下。");
    }

    #[test]
    fn clean_for_tts_strips_markdown_noise() {
        let md = "# 标题\n\n这是**重点**和 `代码` 还有 [链接](https://x.com)。\n\n```rust\nfn main() {}\n```\n\n- 一项\n- 两项";
        let got = clean_for_tts(md);
        assert!(!got.contains('#'));
        assert!(!got.contains('*'));
        assert!(!got.contains('`'));
        assert!(!got.contains("https://"));
        assert!(!got.contains("fn main")); // code block dropped
        assert!(got.contains("这是重点和 代码 还有 链接。"));
        assert!(got.contains("一项"));
    }

    #[test]
    fn clean_for_tts_blank_when_only_formatting() {
        assert_eq!(clean_for_tts("```\ncode\n```").as_str(), "");
    }

    #[test]
    fn clean_for_tts_strips_emoji_and_symbols() {
        // GPT-SoVITS aborts synthesis on emoji/symbols; they must be dropped.
        let got = clean_for_tts("晚上好呀！😄🌙 今天第 N 次×2 打招呼 😂🐙");
        for bad in ['😄', '🌙', '😂', '🐙', '×'] {
            assert!(!got.contains(bad), "should strip {bad:?}: got {got:?}");
        }
        // Speakable content + punctuation preserved.
        assert!(got.contains("晚上好呀！"));
        assert!(got.contains("今天第 N 次"));
        assert!(got.contains("打招呼"));
    }

    #[test]
    fn voice_turn_defers_everything_but_the_keep_list() {
        // A spoken turn keeps only the recovery tool; every other registered
        // tool is deferred so the first LLM call is not taxed by the full set.
        let all = vec![
            "read_file".to_string(),
            "shell".to_string(),
            "web_search".to_string(),
            "activate_tools".to_string(),
            "spawn".to_string(),
        ];
        let deferred = voice_turn_deferred_names(&all, VOICE_TURN_KEEP_TOOLS);
        assert_eq!(
            deferred,
            vec![
                "read_file".to_string(),
                "shell".to_string(),
                "web_search".to_string(),
                "spawn".to_string(),
            ]
        );
        assert!(
            !deferred.contains(&"activate_tools".to_string()),
            "activate_tools must stay active so deferred tools remain recoverable"
        );
    }

    #[test]
    fn voice_turn_defer_is_noop_when_only_keep_tools_present() {
        let all = vec!["activate_tools".to_string()];
        assert!(voice_turn_deferred_names(&all, VOICE_TURN_KEEP_TOOLS).is_empty());
    }

    #[test]
    fn voice_turn_skips_defer_when_no_recovery_tool_present() {
        // Regression (#1464 P2): without `activate_tools` on the surface,
        // deferring everything would strand the turn — no tools and no way to
        // recover one. `defer_tools_for_voice_turn` must skip in that case.
        let without = vec!["read_file".to_string(), "shell".to_string()];
        assert!(
            !voice_turn_can_defer(&without, VOICE_TURN_KEEP_TOOLS),
            "no recovery tool ⇒ must not defer"
        );
        let with = vec!["read_file".to_string(), "activate_tools".to_string()];
        assert!(
            voice_turn_can_defer(&with, VOICE_TURN_KEEP_TOOLS),
            "recovery tool present ⇒ deferral is safe"
        );
    }

    #[test]
    fn audio_paths_filters_non_audio() {
        let media = vec![
            "/tmp/a/photo.png".to_string(),
            "/tmp/a/note.ogg".to_string(),
            "/tmp/a/doc.pdf".to_string(),
            "/tmp/a/clip.wav".to_string(),
        ];
        let got = audio_paths(&media);
        assert_eq!(
            got,
            vec!["/tmp/a/note.ogg".to_string(), "/tmp/a/clip.wav".to_string()]
        );
    }

    #[test]
    fn parse_visual_marker_extracts_kind_and_brief() {
        let d =
            parse_visual_marker("好的我给你画一个。\n[[VISUAL:html|可调增益的负反馈电路交互演示]]")
                .expect("marker present");
        assert_eq!(d.kind, VisualKind::Html);
        assert_eq!(d.brief, "可调增益的负反馈电路交互演示");
    }

    #[test]
    fn parse_visual_marker_maps_image_kinds() {
        assert_eq!(
            parse_visual_marker("x [[VISUAL:image|一只猫]]")
                .unwrap()
                .kind,
            VisualKind::Image
        );
        assert_eq!(
            parse_visual_marker("x [[VISUAL:infographic|三段]]")
                .unwrap()
                .kind,
            VisualKind::Infographic
        );
        assert_eq!(
            parse_visual_marker("讲解细胞 [[VISUAL:illustrated|人类细胞结构]]")
                .unwrap()
                .kind,
            VisualKind::Illustrated
        );
    }

    #[test]
    fn parse_visual_marker_none_when_absent_or_unknown_kind() {
        assert!(parse_visual_marker("纯口播回复，没有标记。").is_none());
        assert!(parse_visual_marker("[[VISUAL:bogus|x]]").is_none());
        assert!(parse_visual_marker("[[VISUAL:html|]]").is_none()); // empty brief
    }

    #[test]
    fn parse_visual_marker_requires_trailing_position() {
        // A mid-reply mention / quote of the syntax must NOT trigger an artifact.
        assert!(
            parse_visual_marker("我用 [[VISUAL:html|x]] 这种写法举个例子，然后继续说。").is_none()
        );
        // Trailing marker (with trailing whitespace/newline) is accepted.
        let d = parse_visual_marker("好的。\n[[VISUAL:html|电路]]  \n").expect("trailing");
        assert_eq!(d.kind, VisualKind::Html);
        assert_eq!(d.brief, "电路");
    }

    #[test]
    fn strip_visual_marker_only_strips_trailing() {
        assert_eq!(
            strip_visual_marker("我画一个。\n[[VISUAL:html|电路]]"),
            "我画一个。"
        );
        // Mid-reply mention is left intact (not truncated).
        let mid = "用 [[VISUAL:html|x]] 举例，然后继续。";
        assert_eq!(strip_visual_marker(mid), mid);
    }

    #[test]
    fn splitter_ignores_non_visual_double_bracket() {
        let mut sp = VoiceReplySplitter::new();
        let mut spoken = String::new();
        for tok in ["看这个 [[1]] 参考。", "后面还有内容。"] {
            for s in sp.push(tok) {
                spoken.push_str(&s);
            }
        }
        let (tail, directive) = sp.finish();
        if let Some(t) = tail {
            spoken.push_str(&t);
        }
        assert!(spoken.contains("看这个"));
        assert!(
            spoken.contains("后面还有内容"),
            "ordinary [[ must not truncate TTS: {spoken:?}"
        );
        assert!(directive.is_none());
    }

    #[test]
    fn splitter_holds_back_marker_from_tts_even_when_token_split() {
        let mut sp = VoiceReplySplitter::new();
        let mut spoken = String::new();
        // The marker streams in as several chunks; none may leak "[[VIS..." to TTS.
        for tok in ["你好。\n", "[[VIS", "UAL:html|画个", "电路]]"] {
            for s in sp.push(tok) {
                spoken.push_str(&s);
                spoken.push('\n');
            }
        }
        let (tail, directive) = sp.finish();
        if let Some(t) = tail {
            spoken.push_str(&t);
        }
        assert!(spoken.contains("你好"));
        assert!(
            !spoken.contains("VISUAL"),
            "marker must never reach TTS: {spoken:?}"
        );
        assert!(!spoken.contains("[["), "no bracket leak: {spoken:?}");
        assert_eq!(directive.unwrap().kind, VisualKind::Html);
    }

    #[test]
    fn splitter_holds_back_when_double_bracket_split_across_tokens() {
        let mut sp = VoiceReplySplitter::new();
        let mut spoken = String::new();
        for tok in ["你好世界[", "[VISUAL:image|猫]]"] {
            for s in sp.push(tok) {
                spoken.push_str(&s);
            }
        }
        let (tail, directive) = sp.finish();
        if let Some(t) = tail {
            spoken.push_str(&t);
        }
        assert!(spoken.contains("你好世界"));
        assert!(
            !spoken.contains("["),
            "single bracket must not leak: {spoken:?}"
        );
        assert_eq!(directive.unwrap().kind, VisualKind::Image);
    }

    #[test]
    fn splitter_passes_through_when_no_marker() {
        let mut sp = VoiceReplySplitter::new();
        let mut spoken = String::new();
        for s in sp.push("第一句。第二句！") {
            spoken.push_str(&s);
        }
        let (tail, directive) = sp.finish();
        if let Some(t) = tail {
            spoken.push_str(&t);
        }
        assert!(spoken.contains("第一句"));
        assert!(spoken.contains("第二句"));
        assert!(directive.is_none());
    }

    #[test]
    fn strip_visual_marker_drops_trailing_directive() {
        assert_eq!(
            strip_visual_marker("我给你画一个。\n[[VISUAL:html|电路]]"),
            "我给你画一个。"
        );
        assert_eq!(strip_visual_marker("纯口播没有标记"), "纯口播没有标记");
    }

    #[tokio::test]
    async fn run_image_skill_delivers_relative_filenames() {
        use octos_agent::{Tool, ToolRegistry, ToolResult};
        use std::path::PathBuf;

        struct FakeImageTool;
        #[async_trait::async_trait]
        impl Tool for FakeImageTool {
            fn name(&self) -> &str {
                "mofa_infographic"
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
                Ok(ToolResult {
                    success: true,
                    output: "ok".into(),
                    files_to_send: vec![PathBuf::from("/tmp/out/poster.png")],
                    ..Default::default()
                })
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(FakeImageTool);
        let reg = std::sync::Arc::new(reg);
        let d = VisualDirective {
            kind: VisualKind::Infographic,
            brief: "三段信息图".into(),
        };
        let rels = run_image_skill(&reg, &d, Path::new("/tmp/out")).await;
        assert_eq!(rels, vec!["poster.png".to_string()]);
    }

    #[tokio::test]
    async fn run_image_skill_empty_when_skill_missing() {
        use octos_agent::ToolRegistry;
        let reg = std::sync::Arc::new(ToolRegistry::new());
        let d = VisualDirective {
            kind: VisualKind::Image,
            brief: "猫".into(),
        };
        assert!(
            run_image_skill(&reg, &d, Path::new("/tmp"))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn run_illustration_image_returns_produced_path() {
        use octos_agent::{Tool, ToolRegistry, ToolResult};
        use std::path::PathBuf;

        struct FakeMofaImage;
        #[async_trait::async_trait]
        impl Tool for FakeMofaImage {
            fn name(&self) -> &str {
                "mofa_image"
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(&self, args: &serde_json::Value) -> eyre::Result<ToolResult> {
                // The brief is forwarded as the generation prompt, and any camera
                // frame is forwarded as a reference image to ground the result.
                assert_eq!(args["prompt"], serde_json::json!("细胞结构"));
                assert_eq!(args["ref_images"], serde_json::json!(["/cam/frame.jpg"]));
                Ok(ToolResult {
                    success: true,
                    output: "ok".into(),
                    files_to_send: vec![PathBuf::from("/tmp/out/illustration.png")],
                    ..Default::default()
                })
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(FakeMofaImage);
        let reg = std::sync::Arc::new(reg);
        let refs = vec!["/cam/frame.jpg".to_string()];
        let got = run_illustration_image(&reg, "细胞结构", Path::new("/tmp/out"), &refs).await;
        assert_eq!(got, Some(PathBuf::from("/tmp/out/illustration.png")));
    }

    #[tokio::test]
    async fn run_illustration_image_omits_ref_images_when_none() {
        use octos_agent::{Tool, ToolRegistry, ToolResult};
        use std::path::PathBuf;

        struct NoRefTool;
        #[async_trait::async_trait]
        impl Tool for NoRefTool {
            fn name(&self) -> &str {
                "mofa_image"
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(&self, args: &serde_json::Value) -> eyre::Result<ToolResult> {
                // No frame this turn → the arg is absent (not an empty array).
                assert!(args.get("ref_images").is_none());
                Ok(ToolResult {
                    success: true,
                    output: "ok".into(),
                    files_to_send: vec![PathBuf::from("/tmp/out/illustration.png")],
                    ..Default::default()
                })
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(NoRefTool);
        let reg = std::sync::Arc::new(reg);
        let got = run_illustration_image(&reg, "x", Path::new("/tmp/out"), &[]).await;
        assert_eq!(got, Some(PathBuf::from("/tmp/out/illustration.png")));
    }

    #[tokio::test]
    async fn run_illustration_image_none_when_skill_missing() {
        use octos_agent::ToolRegistry;
        let reg = std::sync::Arc::new(ToolRegistry::new());
        assert!(
            run_illustration_image(&reg, "x", Path::new("/tmp"), &[])
                .await
                .is_none()
        );
    }

    // octos #1041: `image` kind must route to `mofa_image` (which reports its
    // PNG via `files_to_send`), NOT `mofa_cards` (which emits none, so the old
    // mapping delivered nothing for a plain "generate an image" request).
    #[tokio::test]
    async fn run_image_skill_image_kind_uses_mofa_image_and_delivers_file() {
        use octos_agent::{Tool, ToolRegistry, ToolResult};
        use std::path::PathBuf;

        struct FakeMofaImage;
        #[async_trait::async_trait]
        impl Tool for FakeMofaImage {
            fn name(&self) -> &str {
                "mofa_image"
            }
            fn description(&self) -> &str {
                "fake"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(&self, args: &serde_json::Value) -> eyre::Result<ToolResult> {
                // The brief is forwarded as the generation prompt, and the
                // produced file is named UNIQUELY under the turn's out dir
                // (`image-<uuid>.png`) so the mofa skill's path cache never
                // returns a stale image.
                assert_eq!(args["prompt"], serde_json::json!("一只猫"));
                let out = args["out"].as_str().expect("out is a string");
                assert!(
                    out.starts_with("/tmp/out/image-") && out.ends_with(".png"),
                    "out must be a unique image-<uuid>.png under the out dir, got: {out}"
                );
                Ok(ToolResult {
                    success: true,
                    output: "ok".into(),
                    files_to_send: vec![PathBuf::from(out)],
                    ..Default::default()
                })
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(FakeMofaImage);
        let reg = std::sync::Arc::new(reg);
        let d = VisualDirective {
            kind: VisualKind::Image,
            brief: "一只猫".into(),
        };
        let rels = run_image_skill(&reg, &d, Path::new("/tmp/out")).await;
        assert_eq!(rels.len(), 1);
        assert!(
            rels[0].starts_with("image-") && rels[0].ends_with(".png"),
            "delivered a unique image-<uuid>.png, got: {}",
            rels[0]
        );
    }

    #[test]
    fn strip_visual_directive_strips_content_and_assistant_carriers() {
        let mut content = "好的我给你画一个。\n[[VISUAL:html|可调增益电路]]".to_string();
        let mut messages = vec![
            Message::user("画个电路".to_string()),
            Message::assistant("好的我给你画一个。\n[[VISUAL:html|可调增益电路]]".to_string()),
        ];
        let directive = strip_visual_directive(&mut content, &mut messages);
        assert_eq!(directive.as_ref().map(|d| d.kind), Some(VisualKind::Html));
        assert_eq!(content, "好的我给你画一个。");
        assert_eq!(messages[1].content, "好的我给你画一个。");
        assert_eq!(messages[0].content, "画个电路");
    }

    #[test]
    fn strip_visual_directive_noop_without_trailing_marker() {
        let mut content = "用 [[VISUAL:html|x]] 举例。".to_string();
        let mut messages = vec![Message::assistant(content.clone())];
        assert!(strip_visual_directive(&mut content, &mut messages).is_none());
        assert!(content.contains("[[VISUAL:html|x]]"));
    }

    #[test]
    fn visible_delta_filter_hides_trailing_marker() {
        let mut f = VisibleDeltaFilter::new();
        let mut seen = String::new();
        for tok in ["你好", "世界。", "\n[[VIS", "UAL:html|猫]]"] {
            seen.push_str(&f.push(tok));
        }
        seen.push_str(&f.finish());
        assert_eq!(seen, "你好世界。");
    }

    #[test]
    fn visible_delta_filter_recovers_false_marker_on_finish() {
        let mut f = VisibleDeltaFilter::new();
        let mut seen = String::new();
        for tok in ["用 ", "[[VISUAL:html|x]]", " 举例。"] {
            seen.push_str(&f.push(tok));
        }
        seen.push_str(&f.finish());
        assert_eq!(seen, "用 [[VISUAL:html|x]] 举例。");
    }

    #[test]
    fn remove_all_visual_markers_scrubs_every_occurrence() {
        // Mid-text + trailing markers are both removed (unlike strip_visual_marker).
        assert_eq!(
            remove_all_visual_markers(
                "讲了电路 [[VISUAL:html|图A]] 又讲了细胞[[VISUAL:image|图B]]"
            ),
            "讲了电路  又讲了细胞"
        );
        // Marker-free text is returned unchanged.
        assert_eq!(remove_all_visual_markers("普通总结文本"), "普通总结文本");
        // An unterminated marker drops the remainder.
        assert_eq!(
            remove_all_visual_markers("abc [[VISUAL:html|没闭合"),
            "abc "
        );
    }

    // ── voice exit intent (UPCR-2026-025) ─────────────────────────────────

    #[test]
    fn parse_exit_marker_requires_trailing_position() {
        // Trailing marker (with trailing whitespace / newline) is accepted.
        assert!(parse_exit_marker("好的，再见啦！\n[[EXIT]]"));
        assert!(parse_exit_marker("拜拜。[[EXIT]]  \n"));
        // A mid-reply mention / quote must NOT trigger an exit.
        assert!(!parse_exit_marker("我说 [[EXIT]] 只是举个例子，然后继续。"));
        // Absent.
        assert!(!parse_exit_marker("普通口播回复，没有标记。"));
    }

    #[test]
    fn strip_exit_marker_only_strips_trailing() {
        assert_eq!(strip_exit_marker("再见啦。\n[[EXIT]]"), "再见啦。");
        // Mid-reply mention is left intact (not truncated).
        let mid = "用 [[EXIT]] 举例，然后继续。";
        assert_eq!(strip_exit_marker(mid), mid);
        // No marker → unchanged.
        assert_eq!(strip_exit_marker("纯口播没有标记"), "纯口播没有标记");
    }

    #[test]
    fn strip_exit_directive_strips_content_and_assistant_carriers() {
        let mut content = "好的，再见！\n[[EXIT]]".to_string();
        let mut messages = vec![
            Message::user("再见".to_string()),
            Message::assistant("好的，再见！\n[[EXIT]]".to_string()),
        ];
        assert!(strip_exit_directive(&mut content, &mut messages));
        assert_eq!(content, "好的，再见！");
        assert_eq!(messages[1].content, "好的，再见！");
        assert_eq!(messages[0].content, "再见");
    }

    #[test]
    fn strip_exit_directive_noop_without_trailing_marker() {
        let mut content = "用 [[EXIT]] 举例。".to_string();
        let mut messages = vec![Message::assistant(content.clone())];
        assert!(!strip_exit_directive(&mut content, &mut messages));
        assert!(content.contains("[[EXIT]]"));
    }

    #[test]
    fn splitter_holds_back_exit_marker_from_tts_even_when_token_split() {
        let mut sp = VoiceReplySplitter::new();
        let mut spoken = String::new();
        // The exit marker streams in as several chunks; none may leak to TTS.
        for tok in ["好的，", "再见啦！\n", "[[EX", "IT]]"] {
            for s in sp.push(tok) {
                spoken.push_str(&s);
                spoken.push('\n');
            }
        }
        let (tail, directive) = sp.finish();
        if let Some(t) = tail {
            spoken.push_str(&t);
        }
        assert!(spoken.contains("再见啦"));
        assert!(
            !spoken.contains("EXIT"),
            "marker must never reach TTS: {spoken:?}"
        );
        assert!(!spoken.contains("[["), "no bracket leak: {spoken:?}");
        // The splitter returns no visual directive for an exit-only reply.
        assert!(directive.is_none());
    }

    #[test]
    fn visible_delta_filter_hides_trailing_exit_marker() {
        let mut f = VisibleDeltaFilter::new();
        let mut seen = String::new();
        for tok in ["再见", "啦。", "\n[[EX", "IT]]"] {
            seen.push_str(&f.push(tok));
        }
        seen.push_str(&f.finish());
        assert_eq!(seen, "再见啦。");
    }

    #[test]
    fn visible_delta_filter_recovers_false_exit_marker_on_finish() {
        // A mid-reply `[[EXIT]]` quote is NOT a trailing marker → recovered.
        let mut f = VisibleDeltaFilter::new();
        let mut seen = String::new();
        for tok in ["用 ", "[[EXIT]]", " 举例。"] {
            seen.push_str(&f.push(tok));
        }
        seen.push_str(&f.finish());
        assert_eq!(seen, "用 [[EXIT]] 举例。");
    }

    #[test]
    fn ordinary_double_bracket_still_reaches_tts_with_exit_marker_added() {
        // Regression: generalizing the hold-back to the control-marker SET must
        // not suppress ordinary `[[1]]` citations — they are a prefix of neither
        // `[[VISUAL:` nor `[[EXIT]]` past the shared `[[`.
        let mut sp = VoiceReplySplitter::new();
        let mut spoken = String::new();
        for tok in ["看这个 [[1]] 参考。", "后面还有内容。"] {
            for s in sp.push(tok) {
                spoken.push_str(&s);
            }
        }
        let (tail, _directive) = sp.finish();
        if let Some(t) = tail {
            spoken.push_str(&t);
        }
        assert!(spoken.contains("看这个"));
        assert!(
            spoken.contains("[[1]]"),
            "citation must still be spoken: {spoken:?}"
        );
        assert!(spoken.contains("后面还有内容"));
    }

    // ── stacked control markers (review fix: visual + exit, either order) ──

    #[test]
    fn strip_control_directives_handles_visual_then_exit() {
        // `…[[VISUAL:…]][[EXIT]]` — peeling only the outer EXIT would leave the
        // visual marker trailing in content/messages. The combined strip must
        // return the visual directive AND exit=true, leaving the text clean.
        let mut content = "好的，给你画一个，再见！\n[[VISUAL:html|电路]]\n[[EXIT]]".to_string();
        let mut messages = vec![
            Message::user("画个电路然后再见".to_string()),
            Message::assistant(
                "好的，给你画一个，再见！\n[[VISUAL:html|电路]]\n[[EXIT]]".to_string(),
            ),
        ];
        let (directive, exit) = strip_control_directives(&mut content, &mut messages);
        assert_eq!(directive.as_ref().map(|d| d.kind), Some(VisualKind::Html));
        assert!(exit);
        assert_eq!(content, "好的，给你画一个，再见！");
        assert_eq!(messages[1].content, "好的，给你画一个，再见！");
        assert!(!content.contains("[["), "no marker may leak: {content:?}");
    }

    #[test]
    fn strip_control_directives_handles_exit_then_visual() {
        // Reverse order: `…[[EXIT]][[VISUAL:…]]`. Order-independent.
        let mut content = "再见！\n[[EXIT]]\n[[VISUAL:image|一只猫]]".to_string();
        let mut messages = vec![Message::assistant(
            "再见！\n[[EXIT]]\n[[VISUAL:image|一只猫]]".to_string(),
        )];
        let (directive, exit) = strip_control_directives(&mut content, &mut messages);
        assert_eq!(directive.as_ref().map(|d| d.kind), Some(VisualKind::Image));
        assert!(exit);
        assert_eq!(content, "再见！");
        assert!(!content.contains("[["));
    }

    #[test]
    fn strip_control_directives_single_markers_and_none() {
        // Exit only.
        let mut c = "再见！\n[[EXIT]]".to_string();
        let (d, e) = strip_control_directives(&mut c, &mut []);
        assert!(d.is_none() && e);
        assert_eq!(c, "再见！");
        // Visual only.
        let mut c = "看图。\n[[VISUAL:html|电路]]".to_string();
        let (d, e) = strip_control_directives(&mut c, &mut []);
        assert_eq!(d.map(|d| d.kind), Some(VisualKind::Html));
        assert!(!e);
        assert_eq!(c, "看图。");
        // Neither.
        let mut c = "普通回复。".to_string();
        let (d, e) = strip_control_directives(&mut c, &mut []);
        assert!(d.is_none() && !e);
        assert_eq!(c, "普通回复。");
    }

    #[test]
    fn splitter_holds_back_stacked_visual_and_exit_markers() {
        // Neither marker may reach TTS when both trail the reply.
        let mut sp = VoiceReplySplitter::new();
        let mut spoken = String::new();
        for tok in ["好的，再见！\n", "[[VISUAL:html|电路]]", "\n[[EXIT]]"] {
            for s in sp.push(tok) {
                spoken.push_str(&s);
                spoken.push('\n');
            }
        }
        let (tail, _directive) = sp.finish();
        if let Some(t) = tail {
            spoken.push_str(&t);
        }
        assert!(spoken.contains("再见"));
        assert!(
            !spoken.contains("VISUAL"),
            "visual leaked to TTS: {spoken:?}"
        );
        assert!(!spoken.contains("EXIT"), "exit leaked to TTS: {spoken:?}");
        assert!(!spoken.contains("[["), "no bracket leak: {spoken:?}");
    }

    #[test]
    fn visible_delta_filter_hides_stacked_visual_and_exit_markers() {
        let mut f = VisibleDeltaFilter::new();
        let mut seen = String::new();
        for tok in ["好的，再见！", "\n[[VISUAL:html|电路]]", "\n[[EXIT]]"] {
            seen.push_str(&f.push(tok));
        }
        seen.push_str(&f.finish());
        assert_eq!(seen, "好的，再见！");
    }
}
