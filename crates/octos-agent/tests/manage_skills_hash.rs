//! Remote manage_skills downloads are accepted only with a trusted Ed25519
//! signature AND a matching SHA-256. These are the regression cases for #2321A.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use octos_agent::tools::manage_skills::{RemoteBinaryAttestation, install_attested_bytes_into_dir};
use octos_plugin::integrity::remote_binary_signing_message;

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

/// (operator trust root, archive digest, signature over package/platform/hash)
fn proof(bytes: &[u8]) -> (String, String, String) {
    proof_with_key(bytes, &SigningKey::from_bytes(&[7; 32]))
}

fn proof_with_key(bytes: &[u8], key: &SigningKey) -> (String, String, String) {
    let public = BASE64.encode(key.verifying_key().to_bytes());
    let digest = format!("{:x}", Sha256::digest(bytes));
    let signature = BASE64.encode(
        key.sign(&remote_binary_signing_message(
            "demo-skill",
            "linux-x86_64",
            &digest,
            None,
            None,
        ))
        .to_bytes(),
    );
    (public, digest, signature)
}

fn install(
    dir: &tempfile::TempDir,
    bytes: &[u8],
    digest: Option<&str>,
    signature: Option<&str>,
    key: Option<&str>,
) -> eyre::Result<bool> {
    install_attested_bytes_into_dir(
        dir.path(),
        "https://example.invalid/skill-v1.tar.gz",
        bytes,
        RemoteBinaryAttestation {
            sha256: digest,
            signature,
            package: "demo-skill",
            platform: "linux-x86_64",
            source_commit: None,
            reviewed_at: None,
            trusted_public_key: key,
        },
    )
}

#[test]
fn signed_archive_with_matching_hash_installs() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"#!/bin/sh\necho hello from skill\n";
    let archive = build_targz_archive(payload);
    let (public, digest, signature) = proof(&archive);
    assert!(
        install(
            &dir,
            &archive,
            Some(&digest),
            Some(&signature),
            Some(&public)
        )
        .unwrap()
    );
    assert_eq!(std::fs::read(dir.path().join("main")).unwrap(), payload);
}

#[test]
fn unsigned_or_unhashed_remote_archive_is_rejected_without_main() {
    let archive = build_targz_archive(b"payload");
    let (public, digest, signature) = proof(&archive);
    for (hash, sig, key) in [
        (None, Some(signature.as_str()), Some(public.as_str())),
        (Some(digest.as_str()), None, Some(public.as_str())),
        (Some(digest.as_str()), Some(signature.as_str()), None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let _ =
            install(&dir, &archive, hash, sig, key).expect_err("missing proof must fail closed");
        assert!(!dir.path().join("main").exists());
    }
}

#[test]
fn bad_signature_is_an_error_not_a_source_build_fallback() {
    let archive = build_targz_archive(b"payload");
    let (public, digest, _) = proof(&archive);
    let (_, _, wrong_signature) = proof_with_key(&archive, &SigningKey::from_bytes(&[9; 32]));
    let dir = tempfile::tempdir().unwrap();
    let _ = install(
        &dir,
        &archive,
        Some(&digest),
        Some(&wrong_signature),
        Some(&public),
    )
    .expect_err("wrong signer must be rejected");
    assert!(!dir.path().join("main").exists());
}

#[test]
fn substituted_bytes_with_original_proof_are_rejected() {
    let archive = build_targz_archive(b"legitimate");
    let (public, digest, signature) = proof(&archive);
    let changed = build_targz_archive(b"substituted");
    let dir = tempfile::tempdir().unwrap();
    let _ = install(
        &dir,
        &changed,
        Some(&digest),
        Some(&signature),
        Some(&public),
    )
    .expect_err("digest mismatch must fail closed");
    assert!(!dir.path().join("main").exists());
}

#[test]
fn signed_raw_binary_with_matching_hash_installs_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"#!/usr/bin/env python\nprint('raw')\n";
    let (public, digest, signature) = proof(payload);
    assert!(
        install_attested_bytes_into_dir(
            dir.path(),
            "https://example.invalid/raw-skill-binary",
            payload,
            RemoteBinaryAttestation {
                sha256: Some(&digest),
                signature: Some(&signature),
                package: "demo-skill",
                platform: "linux-x86_64",
                source_commit: None,
                reviewed_at: None,
                trusted_public_key: Some(&public),
            },
        )
        .unwrap()
    );
    assert_eq!(std::fs::read(dir.path().join("main")).unwrap(), payload);
}
