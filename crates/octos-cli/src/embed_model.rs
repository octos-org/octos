//! Provisioning of the default in-process embedding model.
//!
//! octos ships the llama.cpp embedder in its default build (feature
//! `embed-llama`) and uses **EmbeddingGemma-300M** (Q8_0 GGUF, 768-d native,
//! Matryoshka-truncated to 256-d for the Recall index — see
//! `docs/adr/personal-memory-tiers.md`) when no `embedding` section is
//! configured. The 334 MB model file is not compiled into the binary: it is
//! fetched once from the public `ggml-org/embeddinggemma-300M-GGUF` release
//! into `<data_dir>/models/`, verified against a pinned SHA-256, and reused
//! by every profile under that data dir.
//!
//! The download is opt-out (`embedding.auto_download = false` or
//! `OCTOS_NO_MODEL_DOWNLOAD=1`); without the file the runtime stays
//! keyword-only, which every memory path supports.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eyre::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// File name under `<data_dir>/models/`.
pub const DEFAULT_MODEL_FILE: &str = "embeddinggemma-300M-Q8_0.gguf";
/// Public source (no account needed). The Gemma Terms of Use apply to the
/// weights: <https://ai.google.dev/gemma/terms>.
pub const DEFAULT_MODEL_URL: &str = "https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/main/embeddinggemma-300M-Q8_0.gguf";
pub const DEFAULT_MODEL_SHA256: &str =
    "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63";
pub const DEFAULT_MODEL_BYTES: u64 = 333_590_944;
pub const DEFAULT_MODEL_LICENSE_URL: &str = "https://ai.google.dev/gemma/terms";
/// Human-readable identifier recorded with the vectors it produced.
pub const DEFAULT_MODEL_ID: &str = "llamacpp/embeddinggemma-300M-Q8_0";

/// Environment switch that disables the automatic download everywhere.
pub const NO_DOWNLOAD_ENV: &str = "OCTOS_NO_MODEL_DOWNLOAD";

/// Total wall-clock budget for ONE fetch, shared across its retry attempts
/// (each attempt receives the remaining budget as its own total timeout).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// A transfer delivering no bytes for this long is dead: CDN hiccups stall
/// mid-body, and without a read deadline the stream sits on one read until
/// the total timeout, starving whatever awaits the file (#2561).
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Fresh-connection attempts for one fetch. A failed transfer is usually a
/// transient network fault (#2561: an oup-lane run died on a first-use
/// download that never finished within the awaiting RPC's budget), and a new
/// connection often does better — but the attempts share ONE budget, so the
/// fetch still gives up after [`DOWNLOAD_TIMEOUT`] overall: the same
/// 15-minute bound a single un-retried download had before.
const DOWNLOAD_ATTEMPTS: usize = 3;

/// Where a model comes from and how to prove it arrived intact. Injectable so
/// the fetch pipeline (stream, hash, rename) is testable against a local
/// server instead of the pinned 334 MB release.
struct ModelSource<'a> {
    url: &'a str,
    sha256: &'a str,
    bytes: u64,
}

const DEFAULT_MODEL_SOURCE: ModelSource<'static> = ModelSource {
    url: DEFAULT_MODEL_URL,
    sha256: DEFAULT_MODEL_SHA256,
    bytes: DEFAULT_MODEL_BYTES,
};

/// Where the default model lives for `data_dir`.
pub fn default_model_path(data_dir: &Path) -> PathBuf {
    data_dir.join("models").join(DEFAULT_MODEL_FILE)
}

/// What is on disk for the default model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStatus {
    pub path: PathBuf,
    pub present: bool,
    /// Bytes on disk (0 when absent).
    pub bytes: u64,
    /// Present AND the expected size (a partial download is not complete).
    pub complete: bool,
}

pub fn model_status(data_dir: &Path) -> ModelStatus {
    let path = default_model_path(data_dir);
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let present = bytes > 0;
    ModelStatus {
        path,
        present,
        bytes,
        complete: present && bytes == DEFAULT_MODEL_BYTES,
    }
}

/// Whether automatic downloads are allowed for this process.
pub fn downloads_allowed(config_flag: Option<bool>) -> bool {
    if std::env::var_os(NO_DOWNLOAD_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
        return false;
    }
    config_flag.unwrap_or(true)
}

/// Make sure the default model exists under `data_dir`, downloading it when
/// `download` is true. Returns the model path. Fails when the file is absent
/// and downloading is not allowed, or when the download does not verify.
pub fn ensure_default_model(data_dir: &Path, download: bool) -> Result<PathBuf> {
    ensure_default_model_with(data_dir, download, |dest, total| {
        download_default_model(dest, total)
    })
}

/// [`ensure_default_model`] with the fetch injected, so the concurrency
/// contract can be tested without downloading 300 MB. The fetch may be
/// invoked several times — failed attempts are retried, bounded by
/// [`DOWNLOAD_ATTEMPTS`] tries within one shared [`DOWNLOAD_TIMEOUT`] budget
/// (passed to each attempt as its remaining time) — and every attempt writes
/// a fresh `.part` file.
fn ensure_default_model_with(
    data_dir: &Path,
    download: bool,
    mut fetch: impl FnMut(&Path, Duration) -> Result<()>,
) -> Result<PathBuf> {
    let status = model_status(data_dir);
    if status.complete {
        return Ok(status.path);
    }
    if status.present {
        tracing::warn!(
            path = %status.path.display(),
            bytes = status.bytes,
            expected = DEFAULT_MODEL_BYTES,
            "default embedding model is incomplete; fetching it again"
        );
    }
    if !download {
        bail!(
            "embedding model {} is not present at {} and automatic download is disabled \
             (set embedding.auto_download = true, unset {NO_DOWNLOAD_ENV}, or run \
             `octos memory embedder --fetch`)",
            DEFAULT_MODEL_FILE,
            status.path.display()
        );
    }
    // Serialize fetches within this process. The `.part.<pid>` suffix already
    // keeps separate processes apart, but threads in one process share a pid,
    // so without this lock two overlapping first-use calls — parallel tests, or
    // a gateway starting several profiles — write the same part file, and
    // `File::create` truncates it under the other writer. Each download still
    // passes its own hash (it hashes what it streamed, not the file on disk),
    // so the corrupt file is renamed into place and llama.cpp aborts the whole
    // process loading it. A poisoned lock is recovered: the guard protects no
    // data, only ordering.
    static FETCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = FETCH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Re-check under the lock: whoever held it may have just finished, and
    // then there is nothing left to fetch.
    let status = model_status(data_dir);
    if status.complete {
        return Ok(status.path);
    }
    let deadline = Instant::now() + DOWNLOAD_TIMEOUT;
    let mut last_err: Option<eyre::Report> = None;
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::warn!("default embedding model fetch budget exhausted; giving up");
            break;
        }
        match fetch(&status.path, remaining) {
            Ok(()) => return Ok(status.path),
            Err(err) => {
                tracing::warn!(
                    attempt,
                    attempts = DOWNLOAD_ATTEMPTS,
                    error = %err,
                    "default embedding model fetch failed"
                );
                last_err = Some(err);
            }
        }
    }
    match last_err {
        Some(err) => Err(err),
        None => bail!("default embedding model fetch budget exhausted before any attempt"),
    }
}

/// Download + verify on a dedicated thread with its own HTTP client, so this
/// can be called from sync code, from inside a tokio runtime, or from the FFI
/// without caring about the caller's executor. `total` bounds the attempt.
fn download_default_model(dest: &Path, total: Duration) -> Result<()> {
    let dest = dest.to_path_buf();
    tracing::info!(
        url = DEFAULT_MODEL_URL,
        dest = %dest.display(),
        bytes = DEFAULT_MODEL_BYTES,
        license = DEFAULT_MODEL_LICENSE_URL,
        "fetching the default embedding model (EmbeddingGemma-300M; Gemma Terms of Use apply)"
    );
    let handle = std::thread::Builder::new()
        .name("octos-model-download".into())
        .spawn(move || download_and_verify(&DEFAULT_MODEL_SOURCE, &dest, READ_TIMEOUT, total))
        .wrap_err("failed to spawn the model download thread")?;
    handle
        .join()
        .map_err(|_| eyre::eyre!("model download thread panicked"))?
}

/// Stream `source.url` into `dest`, verifying the pinned SHA-256, under a
/// `read_timeout` under a `total` budget. The download runs on a private
/// single-thread runtime: the async client exposes a read deadline, which the
/// blocking client lacks, and a transfer that goes silent mid-body must error
/// out instead of hanging until the total timeout (#2561 — such a transfer
/// starves whatever bootstrap awaits the model for the rest of the budget).
/// Bounding the failure is all this does: it cannot make a stalled or slow
/// transfer finish within a caller's own deadline.
fn download_and_verify(
    source: &ModelSource,
    dest: &Path,
    read_timeout: Duration,
    total: Duration,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    let part = dest.with_extension(format!("gguf.part.{}", std::process::id()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .wrap_err("failed to build the download runtime")?;
    runtime.block_on(async move {
        let client = reqwest::Client::builder()
            .timeout(total)
            .read_timeout(read_timeout)
            .user_agent(concat!("octos/", env!("CARGO_PKG_VERSION")))
            .build()
            .wrap_err("failed to build the download client")?;
        let mut response = client
            .get(source.url)
            .send()
            .await
            .wrap_err("model download request failed")?
            .error_for_status()
            .wrap_err("model download refused")?;
        let mut file = std::fs::File::create(&part)
            .wrap_err_with(|| format!("failed to create {}", part.display()))?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut next_report: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .wrap_err("model download read failed")?
        {
            hasher.update(&chunk);
            file.write_all(&chunk)
                .wrap_err("model download write failed")?;
            total += chunk.len() as u64;
            if total >= next_report {
                tracing::info!(
                    downloaded_mb = total / (1024 * 1024),
                    total_mb = DEFAULT_MODEL_BYTES / (1024 * 1024),
                    "embedding model download progress"
                );
                next_report += 64 * 1024 * 1024;
            }
        }
        file.flush()?;
        drop(file);
        let digest = format!("{:x}", hasher.finalize());
        if digest != source.sha256 || total != source.bytes {
            let _ = std::fs::remove_file(&part);
            bail!(
                "downloaded embedding model does not match the pinned release \
                 (sha256 {digest}, {total} bytes; expected {}, {}) — \
                 the file was discarded",
                source.sha256,
                source.bytes
            );
        }
        std::fs::rename(&part, dest).wrap_err_with(|| {
            format!("failed to move the model into place at {}", dest.display())
        })?;
        tracing::info!(path = %dest.display(), "default embedding model ready");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_report_absent_incomplete_and_complete_models() {
        let dir = tempfile::tempdir().unwrap();
        let s = model_status(dir.path());
        assert!(!s.present && !s.complete && s.bytes == 0);
        assert_eq!(s.path, dir.path().join("models").join(DEFAULT_MODEL_FILE));
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        std::fs::write(&s.path, b"partial").unwrap();
        let s = model_status(dir.path());
        assert!(s.present && !s.complete);
    }

    #[test]
    fn should_refuse_when_absent_and_download_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_default_model(dir.path(), false).unwrap_err();
        assert!(
            err.to_string().contains("automatic download is disabled"),
            "{err}"
        );
    }

    /// Parallel callers in ONE process must never fetch at the same time.
    /// Every fetch writes the same `<model>.gguf.part.<pid>` file — the pid
    /// separates processes, not threads — and `File::create` truncates it, so
    /// two overlapping fetches interleave into a corrupt file. Each still passes
    /// its own SHA-256 check (it hashes the bytes IT streamed, not the file on
    /// disk), renames the garbage into place, and llama.cpp then aborts the
    /// whole process loading it: `GGML_ASSERT(!key.empty())`. That is the
    /// intermittent octos-cli CI crash, and a real first-run crash for any
    /// process that builds two embedders at once.
    #[test]
    fn should_never_run_two_fetches_at_once_when_callers_race() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(4));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let data_dir = dir.path().to_path_buf();
                let (in_flight, max_in_flight, barrier) =
                    (in_flight.clone(), max_in_flight.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    ensure_default_model_with(&data_dir, true, |_, _| {
                        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        max_in_flight.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(
            max_in_flight.load(Ordering::SeqCst),
            1,
            "two fetches overlapped — they would share and corrupt one .part file"
        );
    }

    #[test]
    fn should_default_to_downloading_unless_config_says_no() {
        if std::env::var_os(NO_DOWNLOAD_ENV).is_none() {
            assert!(downloads_allowed(None));
            assert!(downloads_allowed(Some(true)));
        }
        assert!(!downloads_allowed(Some(false)));
    }

    /// One failed attempt (a stalled transfer, a reset connection) must not
    /// fail the whole fetch: #2561 saw an oup-lane run die on a first-use
    /// download that never finished in time, with no retry to recover. A
    /// fresh connection often does better, so the fetch retries within
    /// bounds.
    #[test]
    fn should_retry_transient_fetch_failures_and_recover() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fetch = calls.clone();
        let path = ensure_default_model_with(dir.path(), true, move |dest, _| {
            let attempt = calls_for_fetch.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt < 3 {
                bail!("simulated stall on attempt {attempt}");
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(dest, b"model-bytes")?;
            Ok(())
        })
        .expect("a transient failure should be retried, not surfaced");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(path, dir.path().join("models").join(DEFAULT_MODEL_FILE));
    }

    #[test]
    fn should_surface_the_failure_after_bounded_retry_attempts() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fetch = calls.clone();
        let err = ensure_default_model_with(dir.path(), true, move |_, _| {
            calls_for_fetch.fetch_add(1, Ordering::SeqCst);
            bail!("persistent failure")
        })
        .unwrap_err();
        assert!(err.to_string().contains("persistent failure"), "{err}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            DOWNLOAD_ATTEMPTS,
            "the fetch must give up after a bounded number of attempts"
        );
    }

    /// Retry attempts must share one total budget: three unbounded 15-minute
    /// attempts would triple the worst-case hang the deadline exists to
    /// bound. The attempts each see the REMAINING budget, so the sum can
    /// never exceed [`DOWNLOAD_TIMEOUT`].
    #[test]
    fn should_split_one_shared_budget_across_retry_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let mut seen: Vec<Duration> = Vec::new();
        let err = ensure_default_model_with(dir.path(), true, |_, remaining| {
            seen.push(remaining);
            std::thread::sleep(Duration::from_millis(50));
            bail!("always fails")
        })
        .unwrap_err();
        assert!(err.to_string().contains("always fails"), "{err}");
        assert_eq!(seen.len(), DOWNLOAD_ATTEMPTS);
        assert!(
            seen[0] <= DOWNLOAD_TIMEOUT,
            "the first attempt must be capped by the total budget"
        );
        for pair in seen.windows(2) {
            assert!(
                pair[1] < pair[0],
                "each retry must see a strictly smaller remaining budget"
            );
        }
    }

    /// The full happy path of the real HTTP pipeline — stream, hash, rename —
    /// against a local server serving bytes that match an injected source
    /// spec, so the pinned 334 MB release is not needed.
    #[test]
    fn should_stream_hash_and_rename_a_matching_download() {
        let body = b"octos model bytes";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (served_tx, served_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            drain_request_head(&mut socket);
            use std::io::Write as _;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            socket.write_all(response.as_bytes()).unwrap();
            socket.write_all(body).unwrap();
            let _ = served_tx.send(());
        });

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("models").join("served.gguf");
        let digest = Sha256::digest(body);
        let source = ModelSource {
            url: &format!("http://{addr}/gguf"),
            sha256: &format!("{digest:x}"),
            bytes: body.len() as u64,
        };
        let served = download_and_verify(&source, &dest, READ_TIMEOUT, Duration::from_secs(30));
        // A proxy configured via the environment could swallow the loopback
        // connection; fail loudly instead of hanging on the server join.
        served_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the local server never accepted a connection");
        server.join().unwrap();
        served.expect("a matching download should verify and land");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let part = dest.with_extension(format!("gguf.part.{}", std::process::id()));
        assert!(!part.exists(), "the .part file must be renamed into place");
    }

    /// Drain one request head the way hyper's h1 client expects before
    /// responding: writing while the client is still sending its request
    /// races `Conn::require_empty_read` and surfaces as a spurious
    /// `UnexpectedMessage`, not as the behavior under test.
    fn drain_request_head(socket: &mut std::net::TcpStream) {
        use std::io::Read as _;
        let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 512];
        let mut seen = Vec::new();
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// A transfer that stops delivering bytes mid-body must error at the read
    /// deadline instead of hanging until the total timeout: #2561 saw a CI
    /// transfer die mid-model, and without a deadline the awaiting
    /// `profile/llm/upsert` hangs past its own budget while the bootstrap
    /// holds the runtime. Drives the real HTTP path against a local server
    /// that promises a body and then goes silent.
    #[test]
    fn should_error_when_the_transfer_stalls_mid_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            drain_request_head(&mut socket);
            use std::io::Write as _;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n0123456789")
                .unwrap();
            let _ = accepted_tx.send(());
            // Hold the socket open without sending the rest — a stall, not a
            // close, so a plain EOF cannot mask the missing deadline.
            std::thread::sleep(Duration::from_secs(2));
        });

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("models").join("stalled.gguf");
        let source = ModelSource {
            url: &format!("http://{addr}/gguf"),
            sha256: "unused",
            bytes: 1000,
        };
        let started = std::time::Instant::now();
        // The read deadline (500 ms) fires long before the total budget, so
        // this exercises the read deadline itself, not the total.
        let result = download_and_verify(
            &source,
            &dest,
            Duration::from_millis(500),
            Duration::from_secs(30),
        );
        let elapsed = started.elapsed();
        // A proxy configured via the environment could swallow the loopback
        // connection; fail loudly instead of hanging on the server join.
        accepted_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the local server never accepted a connection");
        server.join().unwrap();
        let _must_err = result.expect_err("a stalled transfer must error, not hang");
        // Which deadline names the error varies with load (the read deadline
        // also bounds the header phase); the invariants that matter are that
        // the failure arrived promptly and mid-body, after real streaming.
        assert!(
            elapsed < Duration::from_secs(10),
            "the read deadline should fire promptly, took {elapsed:?}"
        );
        // The stall hit mid-body: the sliver that arrived must be on disk in
        // the part file, proving the failure came after real streaming.
        let part = dest.with_extension(format!("gguf.part.{}", std::process::id()));
        assert_eq!(std::fs::read(&part).unwrap(), b"0123456789");
    }
}
