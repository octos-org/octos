//! Bounded command capture with a durable, workspace-readable full log.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::write_grant::{ConfinedLeaf, open_confined};

const TAIL_BYTES: usize = 64 * 1024;
static NEXT_LOG: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct CommandOutput(Arc<Mutex<State>>);

struct State {
    file: Option<std::fs::File>,
    path: PathBuf,
    tail: Vec<u8>,
    total: u64,
    cursor: u64,
    error: Option<String>,
    finished: bool,
}

pub(crate) struct OutputView {
    pub text: String,
    pub path: PathBuf,
    pub from: u64,
    pub to: u64,
    pub truncated: bool,
    pub error: Option<String>,
}

impl CommandOutput {
    pub fn new(workspace: &Path) -> std::io::Result<Self> {
        // Runtime files must not make a command appear to have edited source.
        // create_new preserves any existing user ignore rules.
        if let Ok(mut ignore) = open_confined(
            workspace,
            Path::new(".octos/tool-output/.gitignore"),
            ConfinedLeaf::CreateNew,
        ) {
            ignore.write_all(b"*\n")?;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = NEXT_LOG.fetch_add(1, Ordering::Relaxed);
        let relative = PathBuf::from(format!(
            ".octos/tool-output/{stamp}-{}-{id}.log",
            std::process::id()
        ));
        let file = open_confined(workspace, &relative, ConfinedLeaf::CreateNew)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self(Arc::new(Mutex::new(State {
            file: Some(file),
            path: workspace.join(relative),
            tail: Vec::new(),
            total: 0,
            cursor: 0,
            error: None,
            finished: false,
        }))))
    }

    pub fn append(&self, bytes: &[u8]) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.finished {
            return;
        }
        if state.error.is_none() {
            if let Err(e) = state
                .file
                .as_mut()
                .expect("unfinished capture owns file")
                .write_all(bytes)
            {
                state.error = Some(e.to_string());
            }
        }
        state.total = state.total.saturating_add(bytes.len() as u64);
        if bytes.len() >= TAIL_BYTES {
            state.tail.clear();
            state
                .tail
                .extend_from_slice(&bytes[bytes.len() - TAIL_BYTES..]);
        } else {
            let drop = state
                .tail
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(TAIL_BYTES);
            state.tail.drain(..drop);
            state.tail.extend_from_slice(bytes);
        }
    }

    pub fn record_error(&self, error: impl ToString) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).error = Some(error.to_string());
    }

    pub fn finish(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.file.take();
        state.finished = true;
    }

    /// Atomically consumes the available byte range for incremental polling.
    /// Truncated bytes remain in the full log; the range makes omissions explicit.
    pub fn view(&self, incremental: bool) -> OutputView {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let from = if incremental { s.cursor } else { 0 };
        let tail_start = s.total - s.tail.len() as u64;
        let offset = from.saturating_sub(tail_start).min(s.tail.len() as u64) as usize;
        let mut bytes = &s.tail[offset..];
        // A ring-buffer boundary may split a UTF-8 character.
        if from < tail_start {
            while bytes.first().is_some_and(|b| b & 0xc0 == 0x80) {
                bytes = &bytes[1..];
            }
        }
        let mut to = s.total;
        // A pipe read may end in the middle of a character. Leave those bytes
        // for the next poll rather than emitting a replacement twice.
        if incremental && !s.finished {
            if let Err(e) = std::str::from_utf8(bytes) {
                if e.error_len().is_none() {
                    to -= (bytes.len() - e.valid_up_to()) as u64;
                    bytes = &bytes[..e.valid_up_to()];
                }
            }
        }
        let text = String::from_utf8_lossy(bytes).into_owned();
        let view = OutputView {
            text,
            path: s.path.clone(),
            from,
            to,
            truncated: from < tail_start,
            error: s.error.clone(),
        };
        if incremental {
            s.cursor = to;
        }
        view
    }
}

impl OutputView {
    pub fn apply_token_budget(&mut self, text: String, budget: Option<usize>) -> String {
        let Some(budget) = budget else { return text };
        if octos_llm::context::estimate_tokens(&text) as usize <= budget.max(1) {
            return text;
        }
        self.truncated = true;
        let footer = format!("\n[Output truncated. Full output: {}]", self.path.display());
        let reserve = octos_llm::context::estimate_tokens(&footer) as usize + 2;
        if budget <= reserve {
            return limit_tokens(footer, Some(budget));
        }
        format!("{}{footer}", limit_tokens(text, Some(budget - reserve)))
    }
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({"full_output_path":self.path,"output_start":self.from,"output_end":self.to,
            "truncated":self.truncated,"log_complete":self.error.is_none(),"log_error":self.error})
    }

    pub fn render(&mut self, max_bytes: usize) -> String {
        let cap = max_bytes.max(1);
        self.truncated |= self.text.len() > cap;
        if !self.truncated && self.text.len() <= cap && self.error.is_none() {
            return self.text.clone();
        }
        let footer = format!(
            "\n[{}Full output: {}{}]",
            if self.truncated {
                "Output truncated. "
            } else {
                ""
            },
            self.path.display(),
            self.error
                .as_ref()
                .map(|e| format!("; log incomplete: {e}"))
                .unwrap_or_default()
        );
        let available = cap.saturating_sub(footer.len());
        let mut start = self.text.len().saturating_sub(available);
        while start < self.text.len() && !self.text.is_char_boundary(start) {
            start += 1;
        }
        format!("{}{footer}", &self.text[start..])
    }
}

/// Token budgets use the same documented estimate as the context manager.
/// This is not a provider tokenizer. Unlike a byte cap it accounts for CJK.
pub(crate) fn limit_tokens(text: String, budget: Option<usize>) -> String {
    let Some(budget) = budget else { return text };
    let budget = budget.max(1).min(u32::MAX as usize) as u32;
    if octos_llm::context::estimate_tokens(&text) <= budget {
        return text;
    }
    let marker = "\n[output truncated]";
    let reserve = octos_llm::context::estimate_tokens(marker);
    let allowed = budget.saturating_sub(reserve);
    let mut weight = 0usize;
    let mut end = 0;
    // ASCII ~1/4 token, non-ASCII ~2/3; twelfths avoid floating drift.
    for (offset, ch) in text.char_indices() {
        let next = weight + if ch.is_ascii() { 3 } else { 8 };
        if next > allowed as usize * 12 {
            break;
        }
        weight = next;
        end = offset + ch.len_utf8();
    }
    if budget < reserve {
        return text.chars().take(budget as usize).collect();
    }
    format!("{}{marker}", &text[..end])
}

pub(crate) async fn capture_reader<R: AsyncRead + Unpin>(mut reader: R, capture: CommandOutput) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => capture.append(&buf[..n]),
            Err(e) => {
                capture.record_error(e);
                break;
            }
        }
    }
}

/// Drain stdout/stderr concurrently without retaining unbounded output in RAM.
pub(crate) async fn wait_with_capture(
    mut child: tokio::process::Child,
    capture: CommandOutput,
) -> std::io::Result<std::process::ExitStatus> {
    struct Readers {
        tasks: Vec<tokio::task::JoinHandle<()>>,
        capture: CommandOutput,
        settled: bool,
    }
    impl Drop for Readers {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
            if !self.settled {
                self.capture
                    .record_error("output capture interrupted before stream EOF");
                self.capture.finish();
            }
        }
    }
    let mut readers = Readers {
        tasks: Vec::new(),
        capture: capture.clone(),
        settled: false,
    };
    if let Some(stdout) = child.stdout.take() {
        readers
            .tasks
            .push(tokio::spawn(capture_reader(stdout, capture.clone())));
    }
    if let Some(stderr) = child.stderr.take() {
        readers
            .tasks
            .push(tokio::spawn(capture_reader(stderr, capture.clone())));
    }
    let status = child.wait().await?;
    let drained = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        for handle in &mut readers.tasks {
            let _ = handle.await;
        }
    })
    .await;
    if drained.is_err() {
        capture.record_error("descendant kept output open after command exit; capture closed");
    }
    capture.finish();
    readers.settled = true;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_keep_split_utf8_and_consume_each_byte_once() {
        let dir = tempfile::tempdir().unwrap();
        let capture = CommandOutput::new(dir.path()).unwrap();
        let bytes = "中文".as_bytes();
        capture.append(&bytes[..2]);
        assert_eq!(capture.view(true).text, "");
        capture.append(&bytes[2..]);
        assert_eq!(capture.view(true).text, "中文");
        assert_eq!(capture.view(true).text, "");
        capture.append(&[0xff]);
        capture.finish();
        assert_eq!(capture.view(true).text, "\u{fffd}");
        assert_eq!(capture.view(true).text, "");
    }

    #[test]
    fn should_bound_memory_without_losing_the_complete_log() {
        let dir = tempfile::tempdir().unwrap();
        let capture = CommandOutput::new(dir.path()).unwrap();
        let bytes = vec![b'x'; 2 * 1024 * 1024];
        capture.append(&bytes);
        capture.finish();
        let view = capture.view(true);
        assert!(view.truncated);
        assert!(view.text.len() <= TAIL_BYTES);
        assert_eq!(view.to, bytes.len() as u64);
        assert_eq!(std::fs::read(&view.path).unwrap(), bytes);
        assert_eq!(capture.view(true).text, "");
    }

    #[test]
    fn should_budget_cjk_as_tokens_and_keep_log_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let capture = CommandOutput::new(dir.path()).unwrap();
        capture.append("中文日志".repeat(5000).as_bytes());
        let mut view = capture.view(false);
        let rendered = view.render(50_000);
        let text = view.apply_token_budget(rendered, Some(400));
        assert!(octos_llm::context::estimate_tokens(&text) <= 400);
        assert!(text.contains("Full output:"));
    }

    #[test]
    fn should_report_truncation_when_only_the_display_budget_is_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let capture = CommandOutput::new(dir.path()).unwrap();
        capture.append(&vec![b'x'; 5000]);
        let mut view = capture.view(false);
        assert!(!view.truncated);
        let _ = view.apply_token_budget(view.text.clone(), Some(400));
        assert_eq!(view.metadata()["truncated"], true);
        let mut view = capture.view(false);
        let _ = view.render(1000);
        assert_eq!(view.metadata()["truncated"], true);
    }

    #[cfg(unix)]
    #[test]
    fn should_refuse_log_directory_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".octos")).unwrap();
        assert!(CommandOutput::new(dir.path()).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
