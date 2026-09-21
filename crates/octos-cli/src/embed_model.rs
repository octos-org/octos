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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

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
    ensure_default_model_with(data_dir, download, download_default_model)
}

/// [`ensure_default_model`] with the fetch injected, so the concurrency
/// contract can be tested without downloading 300 MB.
fn ensure_default_model_with(
    data_dir: &Path,
    download: bool,
    fetch: impl FnOnce(&Path) -> Result<()>,
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
    fetch(&status.path)?;
    Ok(status.path)
}

/// Download + verify on a dedicated thread with its own blocking HTTP
/// client, so this can be called from sync code, from inside a tokio runtime,
/// or from the FFI without caring about the caller's executor.
fn download_default_model(dest: &Path) -> Result<()> {
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
        .spawn(move || download_and_verify(&dest))
        .wrap_err("failed to spawn the model download thread")?;
    handle
        .join()
        .map_err(|_| eyre::eyre!("model download thread panicked"))?
}

fn download_and_verify(dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    let part = dest.with_extension(format!("gguf.part.{}", std::process::id()));
    let client = reqwest::blocking::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent(concat!("octos/", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("failed to build the download client")?;
    let response = client
        .get(DEFAULT_MODEL_URL)
        .send()
        .wrap_err("model download request failed")?
        .error_for_status()
        .wrap_err("model download refused")?;
    let mut reader = response;
    let mut file = std::fs::File::create(&part)
        .wrap_err_with(|| format!("failed to create {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total: u64 = 0;
    let mut next_report: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .wrap_err("model download read failed")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .wrap_err("model download write failed")?;
        total += n as u64;
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
    if digest != DEFAULT_MODEL_SHA256 || total != DEFAULT_MODEL_BYTES {
        let _ = std::fs::remove_file(&part);
        bail!(
            "downloaded embedding model does not match the pinned release \
             (sha256 {digest}, {total} bytes; expected {DEFAULT_MODEL_SHA256}, {DEFAULT_MODEL_BYTES}) — \
             the file was discarded"
        );
    }
    std::fs::rename(&part, dest)
        .wrap_err_with(|| format!("failed to move the model into place at {}", dest.display()))?;
    tracing::info!(path = %dest.display(), "default embedding model ready");
    Ok(())
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
                    ensure_default_model_with(&data_dir, true, |_| {
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
}
