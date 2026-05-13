//! Integration tests for the install-time archive SHA-256 gate in
//! `manage_skills`. These cover Section D of the per-profile skills +
//! signature enforcement PR:
//!
//! - **passing case** — archive bytes match the declared `binaries.<platform>.sha256`
//!   → install succeeds and `<dir>/main` ends up with the archived payload.
//! - **failing case** — the declared digest mismatches → install returns
//!   `Ok(false)` and no `main` file is written.
//! - **missing `binaries.<platform>` entry** — install path falls through
//!   gracefully (no panic, no `main` file from this lane).
//!
//! The tests avoid network and HTTP-server scaffolding by exercising the
//! split-out `install_bytes_into_dir` helper directly. The behavior under
//! test is the SHA-256 verification step that originally lived inside
//! `download_binary` (the network-bound function in `manage_skills.rs`).

use sha2::{Digest, Sha256};
use std::io::Write;

use octos_agent::tools::manage_skills::install_bytes_into_dir;

/// Build an in-memory `.tar.gz` archive containing one file named `main`
/// with the supplied payload bytes. Returns the raw archive bytes — both
/// `install_bytes_into_dir` and the SHA-256 in the matching manifest are
/// computed from this exact buffer.
fn build_targz_archive(payload: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut tar = tar::Builder::new(&mut gz);
        let mut header = tar::Header::new_gnu();
        header.set_path("main").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append(&header, payload).unwrap();
        tar.finish().unwrap();
    }
    gz.finish().unwrap()
}

/// Passing case: archive bytes match the declared SHA-256 → install
/// succeeds and `<dir>/main` carries the archived payload.
#[test]
fn install_bytes_into_dir_accepts_matching_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"#!/bin/sh\necho hello from skill\n";
    let archive = build_targz_archive(payload);
    let digest = format!("{:x}", Sha256::digest(&archive));

    let ok = install_bytes_into_dir(
        dir.path(),
        "https://example.invalid/skill-v1.tar.gz",
        &archive,
        Some(&digest),
    )
    .expect("install_bytes_into_dir must not error on matching hash");
    assert!(
        ok,
        "matching SHA-256 must result in `Ok(true)` (i.e. install accepted)"
    );

    let installed = dir.path().join("main");
    assert!(installed.exists(), "`main` must be written");
    let got = std::fs::read(&installed).unwrap();
    assert_eq!(
        got, payload,
        "installed bytes must match the archive payload"
    );
}

/// Failing case: declared digest does NOT match the archive bytes →
/// `install_bytes_into_dir` returns `Ok(false)` (install refused) and
/// `<dir>/main` is NOT written.
#[test]
fn install_bytes_into_dir_rejects_mismatched_sha256() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"#!/bin/sh\necho legitimate\n";
    let archive = build_targz_archive(payload);
    let bogus_digest = "0".repeat(64);

    let ok = install_bytes_into_dir(
        dir.path(),
        "https://example.invalid/skill-v1.tar.gz",
        &archive,
        Some(&bogus_digest),
    )
    .expect("install_bytes_into_dir must return Ok(false) on mismatch, not Err");
    assert!(
        !ok,
        "mismatched SHA-256 must produce `Ok(false)` so callers can fall through"
    );

    let installed = dir.path().join("main");
    assert!(
        !installed.exists(),
        "no `main` must be written when the hash check fails (got {})",
        installed.display()
    );
}

/// Missing `binaries.<platform>` entry (i.e. caller passes `sha256: None`):
/// archive is still installed (no signature gate to honour). This mirrors
/// the legacy permissive path through `download_binary` — the manifest's
/// `binaries` resolution layer skips the URL entirely when no entry exists
/// for the current platform, but if a caller does invoke the install path
/// without a digest the bytes still land.
#[test]
fn install_bytes_into_dir_no_hash_falls_back_gracefully() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"#!/bin/sh\necho no-hash\n";
    let archive = build_targz_archive(payload);

    let ok = install_bytes_into_dir(
        dir.path(),
        "https://example.invalid/skill-v1.tar.gz",
        &archive,
        None,
    )
    .expect("install_bytes_into_dir must not error when sha256 is None");
    assert!(
        ok,
        "absent sha256 must take the legacy permissive path (install accepted)"
    );

    let installed = dir.path().join("main");
    assert!(installed.exists(), "`main` must be written");
}

/// Bonus coverage: when the URL does not look like an archive the helper
/// treats the bytes as a raw binary and writes them verbatim. The
/// hash-check still gates installation — a mismatch refuses regardless of
/// archive vs raw.
#[test]
fn install_bytes_into_dir_raw_binary_hash_check_applies() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"#!/usr/bin/env python\nprint('raw')\n";
    let digest = format!("{:x}", Sha256::digest(payload));

    // Passing case (raw URL ↔ payload hash matches).
    let ok = install_bytes_into_dir(
        dir.path(),
        "https://example.invalid/raw-skill-binary",
        payload,
        Some(&digest),
    )
    .unwrap();
    assert!(ok, "raw binary with matching hash must be accepted");
    assert_eq!(std::fs::read(dir.path().join("main")).unwrap(), payload);

    // Failing case (same URL, wrong hash).
    let dir2 = tempfile::tempdir().unwrap();
    let bogus = "f".repeat(64);
    let ok = install_bytes_into_dir(
        dir2.path(),
        "https://example.invalid/raw-skill-binary",
        payload,
        Some(&bogus),
    )
    .unwrap();
    assert!(!ok, "raw binary with bogus hash must be refused");
    assert!(!dir2.path().join("main").exists());
}

/// Pull `Write` into scope so the test module compiles cleanly even when
/// `flate2`'s Builder construction below changes its trait-bound surface
/// in a future bump. The trait is intentionally unused at the top level —
/// it's only referenced via the `tar::Builder` writer interface in
/// [`build_targz_archive`].
#[allow(dead_code)]
fn _ensure_write_in_scope() -> Box<dyn Write> {
    Box::new(std::io::sink())
}
