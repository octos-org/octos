//! Remote skill-binary provenance and registry-review checks.
//!
//! The trust root is deliberately out-of-band: registry JSON and download
//! bytes may be replaced together, so a public key embedded in the same
//! registry would prove nothing. Callers pass the operator-configured
//! `OCTOS_HUB_TRUSTED_ED25519_KEY` (base64-encoded 32-byte Ed25519 public key).
//! A signature covers package, platform, digest AND (for registry artifacts)
//! the reviewed source commit/review epoch. The digest then binds those signed
//! semantics to the exact downloaded archive bytes.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use eyre::{Result, WrapErr, bail};
use sha2::{Digest, Sha256};

/// Environment variable containing the out-of-band hub trust root.
pub const HUB_TRUST_PUBLIC_KEY_ENV: &str = "OCTOS_HUB_TRUSTED_ED25519_KEY";
const SIGNING_DOMAIN: &str = "octos-hub-binary-v1";
const REGISTRY_REVIEW_MAX_AGE: Duration = Duration::days(30);
const REGISTRY_REVIEW_FUTURE_TOLERANCE: Duration = Duration::minutes(5);

/// Proof and identity context for one remote binary. `source_commit` and
/// `reviewed_at` are both `None` only for direct manifest downloads; registry
/// artifacts must carry both and have them covered by the signature.
#[derive(Clone, Copy)]
pub struct RemoteBinaryProof<'a> {
    pub sha256: Option<&'a str>,
    pub signature: Option<&'a str>,
    pub package: &'a str,
    pub platform: &'a str,
    pub source_commit: Option<&'a str>,
    pub reviewed_at: Option<&'a str>,
    pub trusted_public_key: Option<&'a str>,
}

/// Canonical bytes a hub publisher signs for one downloadable artifact.
/// The URL is intentionally excluded so a reviewed artifact may move to a
/// mirror. Direct manifest downloads use `-` for the two registry-only fields.
pub fn remote_binary_signing_message(
    package: &str,
    platform: &str,
    sha256: &str,
    source_commit: Option<&str>,
    reviewed_at: Option<&str>,
) -> Vec<u8> {
    format!(
        "{SIGNING_DOMAIN}\0{package}\0{platform}\0{}\0{}\0{}",
        sha256.to_lowercase(),
        source_commit.unwrap_or("-"),
        reviewed_at.unwrap_or("-"),
    )
    .into_bytes()
}

/// Verify signature, digest, signed review provenance and trust root BEFORE
/// any downloaded bytes are written. Missing/malformed proof is an error,
/// never a warning/fallback.
pub fn verify_remote_binary(bytes: &[u8], proof: RemoteBinaryProof<'_>) -> Result<()> {
    let expected = proof.sha256.ok_or_else(|| {
        eyre::eyre!("remote binary has no SHA-256 digest; refusing unsigned download")
    })?;
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("remote binary has an invalid SHA-256 digest");
    }
    match (proof.source_commit, proof.reviewed_at) {
        (None, None) => {}
        (Some(commit), Some(reviewed)) => {
            validate_registry_binary_provenance(Some(commit), Some(reviewed))?;
        }
        _ => bail!("remote binary registry provenance is incomplete"),
    }
    let signature = proof.signature.ok_or_else(|| {
        eyre::eyre!("remote binary has no Ed25519 signature; refusing unsigned download")
    })?;
    let trusted_public_key = proof.trusted_public_key.ok_or_else(|| {
        eyre::eyre!("remote binary trust root is not configured; set {HUB_TRUST_PUBLIC_KEY_ENV}")
    })?;

    let key_bytes = BASE64
        .decode(trusted_public_key)
        .map_err(|_| eyre::eyre!("remote binary trust root is not valid base64"))?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| eyre::eyre!("remote binary trust root must be a 32-byte Ed25519 key"))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| eyre::eyre!("remote binary trust root is not a valid Ed25519 key"))?;
    let signature_bytes = BASE64
        .decode(signature)
        .map_err(|_| eyre::eyre!("remote binary signature is not valid base64"))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| eyre::eyre!("remote binary signature must be 64 bytes"))?;
    let message = remote_binary_signing_message(
        proof.package,
        proof.platform,
        expected,
        proof.source_commit,
        proof.reviewed_at,
    );
    key.verify_strict(&message, &signature)
        .map_err(|_| eyre::eyre!("remote binary signature verification failed"))?;

    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected.to_lowercase() {
        bail!("remote binary SHA-256 mismatch");
    }
    Ok(())
}

/// Require binary-bearing registry entries to name the reviewed upstream
/// commit and carry a recent review timestamp. Source-only entries are not
/// executable downloads and do not call this gate.
pub fn validate_registry_binary_provenance(
    source_commit: Option<&str>,
    reviewed_at: Option<&str>,
) -> Result<()> {
    validate_registry_binary_provenance_at(source_commit, reviewed_at, Utc::now())
}

fn validate_registry_binary_provenance_at(
    source_commit: Option<&str>,
    reviewed_at: Option<&str>,
    now: DateTime<Utc>,
) -> Result<()> {
    let commit = source_commit
        .ok_or_else(|| eyre::eyre!("registry binary entry has no reviewed source commit"))?;
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("registry binary entry has an invalid source commit");
    }
    let reviewed_at =
        reviewed_at.ok_or_else(|| eyre::eyre!("registry binary entry has no review timestamp"))?;
    let reviewed = DateTime::parse_from_rfc3339(reviewed_at)
        .wrap_err("registry binary entry has an invalid review timestamp")?
        .with_timezone(&Utc);
    if reviewed > now + REGISTRY_REVIEW_FUTURE_TOLERANCE {
        bail!("registry binary entry has a future review timestamp");
    }
    if now - reviewed > REGISTRY_REVIEW_MAX_AGE {
        bail!("registry binary review is stale (older than 30 days)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn fixture(bytes: &[u8]) -> (String, String, String) {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = BASE64.encode(key.verifying_key().to_bytes());
        let digest = format!("{:x}", Sha256::digest(bytes));
        let signature = BASE64.encode(
            key.sign(&remote_binary_signing_message(
                "demo",
                "linux-x86_64",
                &digest,
                None,
                None,
            ))
            .to_bytes(),
        );
        (public, digest, signature)
    }

    fn verify(
        bytes: &[u8],
        sha256: Option<&str>,
        signature: Option<&str>,
        trusted_public_key: Option<&str>,
    ) -> Result<()> {
        verify_remote_binary(
            bytes,
            RemoteBinaryProof {
                sha256,
                signature,
                package: "demo",
                platform: "linux-x86_64",
                source_commit: None,
                reviewed_at: None,
                trusted_public_key,
            },
        )
    }

    #[test]
    fn signed_matching_binary_is_accepted() {
        let (public, digest, signature) = fixture(b"payload");
        verify(b"payload", Some(&digest), Some(&signature), Some(&public)).unwrap();
    }

    #[test]
    fn missing_or_bad_proof_is_fail_closed() {
        let (public, digest, signature) = fixture(b"payload");
        for (hash, sig, key) in [
            (None, Some(signature.as_str()), Some(public.as_str())),
            (Some(digest.as_str()), None, Some(public.as_str())),
            (Some(digest.as_str()), Some(signature.as_str()), None),
            (
                Some("not-hex"),
                Some(signature.as_str()),
                Some(public.as_str()),
            ),
            (
                Some(digest.as_str()),
                Some("not-base64"),
                Some(public.as_str()),
            ),
        ] {
            let _ = verify(b"payload", hash, sig, key)
                .expect_err("missing/malformed proof must be rejected");
        }
    }

    #[test]
    fn bad_signature_or_hash_is_rejected() {
        let (public, digest, signature) = fixture(b"payload");
        let other = SigningKey::from_bytes(&[9; 32]);
        let bad_sig = BASE64.encode(
            other
                .sign(&remote_binary_signing_message(
                    "demo",
                    "linux-x86_64",
                    &digest,
                    None,
                    None,
                ))
                .to_bytes(),
        );
        let _ = verify(b"payload", Some(&digest), Some(&bad_sig), Some(&public))
            .expect_err("wrong signer");
        let _ = verify(b"changed", Some(&digest), Some(&signature), Some(&public))
            .expect_err("content substitution");
    }

    #[test]
    fn registry_commit_and_review_epoch_are_signed() {
        let bytes = b"payload";
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = BASE64.encode(key.verifying_key().to_bytes());
        let digest = format!("{:x}", Sha256::digest(bytes));
        let commit = "a".repeat(40);
        let reviewed = Utc::now().to_rfc3339();
        let signature = BASE64.encode(
            key.sign(&remote_binary_signing_message(
                "demo",
                "linux-x86_64",
                &digest,
                Some(&commit),
                Some(&reviewed),
            ))
            .to_bytes(),
        );
        let proof = |source_commit, reviewed_at| RemoteBinaryProof {
            sha256: Some(&digest),
            signature: Some(&signature),
            package: "demo",
            platform: "linux-x86_64",
            source_commit,
            reviewed_at,
            trusted_public_key: Some(&public),
        };
        verify_remote_binary(bytes, proof(Some(&commit), Some(&reviewed))).unwrap();
        let wrong_commit = "b".repeat(40);
        let _ = verify_remote_binary(bytes, proof(Some(&wrong_commit), Some(&reviewed)))
            .expect_err("source-commit substitution must invalidate the signature");
        let redated = Utc::now().to_rfc3339();
        let _ = verify_remote_binary(bytes, proof(Some(&commit), Some(&redated)))
            .expect_err("re-dating an old review must invalidate the signature");
    }

    #[test]
    fn registry_stale_corrupt_and_future_reviews_are_rejected() {
        let now = DateTime::parse_from_rfc3339("2026-11-06T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let commit = "a".repeat(40);
        validate_registry_binary_provenance_at(Some(&commit), Some("2026-11-01T00:00:00Z"), now)
            .unwrap();
        for (source, review) in [
            (None, Some("2026-11-01T00:00:00Z")),
            (Some("bad"), Some("2026-11-01T00:00:00Z")),
            (Some(commit.as_str()), None),
            (Some(commit.as_str()), Some("not-a-date")),
            (Some(commit.as_str()), Some("2026-10-01T00:00:00Z")),
            (Some(commit.as_str()), Some("2026-12-01T00:00:00Z")),
        ] {
            let _ = validate_registry_binary_provenance_at(source, review, now)
                .expect_err("invalid/stale registry metadata must be rejected");
        }
    }
}
