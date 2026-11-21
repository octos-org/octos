//! 语音轮（voice turn）STT/TTS 封装。
//!
//! 把 serve/WS turn 路径需要的两件事——"音频→文本"与"文本→音频文件"——
//! 收敛成两个无状态 async 函数，包住共享的 `OminixClient`。turn 状态机
//! （见 `ui_protocol.rs`）只调用这里，不直接碰 ominix。

use std::path::{Path, PathBuf};

use octos_core::{Message, MessageRole};
use octos_llm::ominix::OminixClient;

use crate::config::CloudTtsConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceAsrStatus {
    NoAudio,
    Speech,
    NoSpeech,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct VoiceAsrOutcome {
    pub(crate) audio_count: usize,
    pub(crate) accepted_transcripts: Vec<String>,
    pub(crate) rejected_count: usize,
    pub(crate) failed_count: usize,
    pub(crate) reject_reasons: Vec<String>,
}

impl VoiceAsrOutcome {
    pub(crate) fn status(&self) -> VoiceAsrStatus {
        if !self.accepted_transcripts.is_empty() {
            VoiceAsrStatus::Speech
        } else if self.audio_count == 0 {
            VoiceAsrStatus::NoAudio
        } else if self.failed_count > 0 {
            VoiceAsrStatus::Failed
        } else {
            VoiceAsrStatus::NoSpeech
        }
    }
}

/// 解析批量 ASR 服务基址。`ASR_API_URL` 指向独立 ASR 时优先；未设置时回退到
/// OminiX，保留既有部署行为。
fn asr_base_url() -> String {
    crate::skills_scope::discover_asr_url().unwrap_or_else(|| "http://127.0.0.1:8081".to_string())
}

/// TTS remains on OminiX even when ASR is redirected to a separate service.
fn ominix_base_url() -> String {
    crate::skills_scope::discover_ominix_url()
        .unwrap_or_else(|| "http://127.0.0.1:8081".to_string())
}

/// 从混合媒体路径里挑出音频文件，保持原顺序。
pub(crate) fn audio_paths(media: &[String]) -> Vec<String> {
    media
        .iter()
        .filter(|p| octos_bus::media::is_audio(p))
        .cloned()
        .collect()
}

/// 转写 turn 内全部音频媒体，并保留拒绝/失败的轮次级状态。
pub(crate) async fn transcribe_audio_media(
    media: &[String],
    language: Option<&str>,
) -> VoiceAsrOutcome {
    transcribe_audio_media_with_base_url(media, language, &asr_base_url()).await
}

async fn transcribe_audio_media_with_base_url(
    media: &[String],
    language: Option<&str>,
    base_url: &str,
) -> VoiceAsrOutcome {
    let audios = audio_paths(media);
    if audios.is_empty() {
        return VoiceAsrOutcome::default();
    }
    let client = OminixClient::new(base_url).with_language(language.map(|s| s.to_string()));
    let asr_t = std::time::Instant::now();
    let mut outcome = VoiceAsrOutcome {
        audio_count: audios.len(),
        ..VoiceAsrOutcome::default()
    };
    for (audio_index, path) in audios.into_iter().enumerate() {
        match client.transcribe(Path::new(&path)).await {
            Ok(transcription) if transcription.rejected => {
                outcome.rejected_count += 1;
                if let Some(reason) = transcription.reject_reason {
                    outcome.reject_reasons.push(reason);
                }
                tracing::debug!(audio_index, "voice_turn: ASR rejected audio");
            }
            Ok(transcription) => outcome.accepted_transcripts.push(transcription.text),
            Err(error) => {
                outcome.failed_count += 1;
                tracing::warn!(audio_index, %error, "voice_turn: transcription failed");
            }
        }
    }
    eprintln!(
        "[TIMING] ASR_done dur_ms={} epoch_ms={}",
        asr_t.elapsed().as_millis(),
        now_ms()
    );
    outcome
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

fn prefers_chinese_speech(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '，'
                | '。'
                | '！'
                | '？'
                | '；'
                | '：'
                | '、'
        )
    })
}

fn push_spoken_operator(result: &mut String, spoken: &str) {
    if result
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace())
    {
        result.push(' ');
    }
    result.push_str(spoken);
    result.push(' ');
}

fn spoken_math_operator(c: char, speaks_chinese: bool) -> Option<&'static str> {
    let (chinese, english) = match c {
        '=' => ("等于", "equals"),
        '+' => ("加", "plus"),
        '−' => ("减", "minus"),
        '×' => ("乘", "times"),
        '÷' | '/' => ("除以", "divided by"),
        '≈' => ("约等于", "approximately equals"),
        '≠' => ("不等于", "does not equal"),
        '≤' => ("小于等于", "is less than or equal to"),
        '≥' => ("大于等于", "is greater than or equal to"),
        '√' => ("根号", "square root of"),
        _ => return None,
    };
    Some(if speaks_chinese { chinese } else { english })
}

/// Strip Markdown / formatting noise so the TTS speaks clean prose instead of
/// "swallowing" or mispronouncing symbols. Removes fenced + inline code, link
/// URLs (keeping the visible text), emphasis/heading/list/quote markers, emoji /
/// pictographs / stray symbols, verbalizes common math operators, and collapses
/// leftover whitespace.
pub(crate) fn clean_for_tts(text: &str) -> String {
    let speaks_chinese = prefers_chinese_speech(text);
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
            other => {
                if let Some(spoken) = spoken_math_operator(other, speaks_chinese) {
                    push_spoken_operator(&mut result, spoken);
                } else if is_tts_safe(other) {
                    result.push(other);
                }
                // Drop emoji / pictographs / stray symbols (😄🌙🐙 …). Some
                // on-device TTS engines (e.g. GPT-SoVITS) abort synthesis on
                // unspeakable chars rather than skipping them.
            }
        }
    }

    // Collapse runs of blank lines / spaces.
    result
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
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

/// Lift the trailing `[[VISUAL:...]]` directive out of the turn's authoritative
/// reply surfaces and strip it in place, so the internal control protocol never
/// reaches the WIRE (`message/delta` from `done`, `projection/envelope`) or
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
/// reaches the WIRE (`message/delta`, `projection/envelope`) or STORAGE (session
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
/// (`message/delta`, `projection/envelope`) and in storage (session JSONL), and —
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

pub(crate) fn cloud_audio_file_extension(encoding: &str) -> &'static str {
    match encoding {
        "wav" => "wav",
        "pcm" => "pcm",
        "ogg_opus" => "ogg",
        _ => "mp3",
    }
}

pub(crate) fn cloud_audio_content_type(encoding: &str) -> &'static str {
    match encoding {
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        "ogg_opus" => "audio/ogg",
        _ => "audio/mpeg",
    }
}

/// Whether the route wants the cloud path. Legacy `volcano` aliases `cloud`.
fn wants_cloud(provider: &str) -> bool {
    matches!(provider, "auto" | "cloud" | "volcano")
}

/// The effective TTS route for pre-flight readiness reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TtsRoute {
    /// Cloud Volcano. For explicit `cloud`/`volcano` this is reported even when
    /// the credentials are missing, so a pre-flight check can flag "cloud
    /// chosen but not configured" instead of masking it behind the silent
    /// on-device fallback that [`synthesize_reply`] applies at request time.
    Cloud,
    /// On-device GPT-SoVITS via ominix-api.
    Local,
}

/// Classify the effective TTS route for readiness, mirroring
/// [`synthesize_reply`]'s routing decision (minus the on-failure fallback):
/// `auto` resolves to cloud only when the credentials are present; explicit
/// `cloud`/`volcano` always reports cloud (so missing creds surface as a
/// not-ready cloud leg, matching the user's chosen route); everything else
/// (`local`, legacy `sovits`/`qwen3`, unknown) is on-device.
pub(crate) fn classify_tts_route(provider: &str, cloud_configured: bool) -> TtsRoute {
    match provider {
        "cloud" | "volcano" => TtsRoute::Cloud,
        "auto" => {
            if cloud_configured {
                TtsRoute::Cloud
            } else {
                TtsRoute::Local
            }
        }
        _ => TtsRoute::Local,
    }
}

/// Whether the cloud (Volcano) TTS path is fully configured for this profile —
/// token + appid resolve and the endpoint is in the HTTPS allowlist. This is
/// exactly what [`synthesize_reply`] needs to actually take the cloud path, so
/// it is the authoritative "cloud TTS ready" signal for pre-flight checks.
pub(crate) fn cloud_tts_configured(cloud: Option<&CloudTtsConfig>) -> bool {
    resolve_volcano(cloud).is_some()
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
/// Shared with the ws_binary streaming path ([`crate::api::volcano_ws`]) so both
/// transports enforce the same boundary.
pub(crate) const VOLCANO_ALLOWED_HOSTS: &[&str] = &["openspeech.bytedance.com"];

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
                .inspect_err(
                    |e| tracing::warn!(error = %e, "voice_turn: volcano client build failed"),
                )
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

    let ext = cloud_audio_file_extension(&cfg.encoding);
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
    let mime = cloud_audio_content_type(&cfg.encoding);
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

fn local_tts_gate() -> &'static std::sync::Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
}

async fn acquire_local_tts_permit() -> Option<tokio::sync::OwnedSemaphorePermit> {
    local_tts_gate().clone().acquire_owned().await.ok()
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
    // OminiX owns one process-wide model/GPU. Every caller (voice turns and the
    // generic REST endpoint alike) passes through this shared boundary so
    // profiles cannot start overlapping local synthesis work.
    let _local_tts_permit = acquire_local_tts_permit().await?;
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

// Voice fail-fast spoken-error lines. Wording is示意 and tweakable; the
// bucketing / precedence is what matters.
const SPEAK_QUOTA: &str = "我这会儿有点忙不过来，稍后再跟我说一次好吗？";
const SPEAK_RATE: &str = "请求有点多，等几秒再说一遍？";
const SPEAK_NET: &str = "网络好像不太稳，再说一遍试试？";
const SPEAK_CTX: &str = "我们聊得有点长了，新开一段再聊吧。";
const SPEAK_FILTERED: &str = "这个我可能没法回答。";
const SPEAK_GENERIC: &str = "抱歉，我这边出了点小问题，稍后再试。";
const SPEAK_EMPTY: &str = "我好像没太听清，再说一遍？";

/// Map a fail-fast [`octos_agent::TurnFailure`] to a short spoken line.
///
/// Strongly-typed [`octos_agent::HarnessError`] variants map directly;
/// `Internal` / `ProviderUnavailable` (where a streaming 429 loses its
/// quota/rate-limit signal through `classify_report`) fall back to a
/// raw-detail substring scan so the most common 429 still hits the right
/// line. `ContentFiltered` is never spoken as "didn't catch that". Returns a
/// `&'static str` so the caller feeds it straight through the normal TTS path.
pub(crate) fn voice_error_speech(f: &octos_agent::TurnFailure) -> &'static str {
    use octos_agent::{HarnessError as H, TurnFailure};
    let (error, raw) = match f {
        TurnFailure::EmptyResponse => return SPEAK_EMPTY,
        TurnFailure::LlmError { error, raw_detail } => (error, raw_detail.to_lowercase()),
    };
    match error {
        H::Quota { .. } => SPEAK_QUOTA,
        H::RateLimited { .. } => SPEAK_RATE,
        H::Network { .. } | H::Timeout { .. } => SPEAK_NET,
        H::ContextOverflow { .. } => SPEAK_CTX,
        H::ContentFiltered { .. } => SPEAK_FILTERED,
        H::Authentication { .. } | H::InvalidRequest { .. } => SPEAK_GENERIC,
        H::Internal { .. } | H::ProviderUnavailable { .. } => {
            if raw.contains("token_quota_exceeded") || raw.contains("quota") {
                SPEAK_QUOTA
            } else if raw.contains("429") || raw.contains("rate limit") {
                SPEAK_RATE
            } else if raw.contains("timeout") || raw.contains("network") {
                SPEAK_NET
            } else {
                SPEAK_GENERIC
            }
        }
        _ => SPEAK_GENERIC,
    }
}
