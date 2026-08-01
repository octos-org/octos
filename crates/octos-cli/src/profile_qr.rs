//! Self-contained profile-in-QR export (`OCTOS1:` / `OCTOS1E:`).
//!
//! Format (EU-DCC pattern, sized for phone cameras — a full profile with
//! three provider keys lands around QR v15):
//!
//! ```text
//! OCTOS1:<base45(zlib(canonical JSON payload))>            — plain
//! OCTOS1E:<base45(zlib(salt ‖ nonce ‖ chacha20poly1305))>  — PIN-wrapped
//! ```
//!
//! The `OCTOS1E` variant derives its key from a 6-digit PIN via Argon2id
//! (the PIN is displayed BESIDE the QR, never inside it): a photographed
//! or logged QR alone is useless. Secrets are only included when the
//! caller explicitly asks; including them forces the encrypted variant
//! unless the caller explicitly opts out.
//!
//! base45 is RFC 9285 — its alphabet is exactly the QR alphanumeric-mode
//! charset, so the encoded string stays in the densest text mode.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

/// Plain-format prefix.
pub const PREFIX_PLAIN: &str = "OCTOS1:";
/// PIN-encrypted-format prefix.
pub const PREFIX_ENCRYPTED: &str = "OCTOS1E:";

/// RFC 9285 alphabet == QR alphanumeric charset.
const BASE45_ALPHABET: &[u8; 45] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";

/// Argon2id salt length prepended to the encrypted body.
const SALT_LEN: usize = 16;
/// ChaCha20-Poly1305 nonce length following the salt.
const NONCE_LEN: usize = 12;

/// The versioned wire payload. `BTreeMap` keeps secrets ordering (and the
/// whole encoding) deterministic for tests and diffing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileQrPayload {
    /// Format version — bump on breaking layout changes.
    pub v: u32,
    /// Discriminator for scanners (`octos-profile`).
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Serve host the mobile client should talk to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Bearer credential for the endpoint (a normal session/API token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// The profile's structured LLM contract, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<serde_json::Value>,
    /// Memory / embedding / voice blocks, verbatim (server-shaped JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_default: Option<String>,
    /// Provider API keys by env-var name. Only present when the caller
    /// explicitly included secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
}

impl ProfileQrPayload {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            v: 1,
            kind: "octos-profile".to_string(),
            id: id.into(),
            name: None,
            endpoint: None,
            auth_token: None,
            llm: None,
            memory: None,
            embedding: None,
            voice_default: None,
            secrets: BTreeMap::new(),
        }
    }

    /// True when the payload carries anything secret (provider keys or a
    /// bearer token) — the encoder uses this to demand the PIN wrap.
    pub fn has_secrets(&self) -> bool {
        !self.secrets.is_empty() || self.auth_token.is_some()
    }
}

// ---------------------------------------------------------------------------
// base45 (RFC 9285)
// ---------------------------------------------------------------------------

fn base45_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 3 / 2 + 3);
    for chunk in data.chunks(2) {
        if let [a, b] = chunk {
            let n = u32::from(*a) * 256 + u32::from(*b);
            out.push(BASE45_ALPHABET[(n % 45) as usize] as char);
            out.push(BASE45_ALPHABET[(n / 45 % 45) as usize] as char);
            out.push(BASE45_ALPHABET[(n / 45 / 45) as usize] as char);
        } else {
            let n = u32::from(chunk[0]);
            out.push(BASE45_ALPHABET[(n % 45) as usize] as char);
            out.push(BASE45_ALPHABET[(n / 45) as usize] as char);
        }
    }
    out
}

fn base45_value(c: u8) -> Result<u32> {
    match BASE45_ALPHABET.iter().position(|&a| a == c) {
        Some(v) => Ok(v as u32),
        None => bail!("invalid base45 character {:?}", c as char),
    }
}

fn base45_decode(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 3 == 1 {
        bail!("invalid base45 length {}", bytes.len());
    }
    let mut out = Vec::with_capacity(bytes.len() / 3 * 2 + 1);
    for chunk in bytes.chunks(3) {
        match chunk {
            [a, b, c] => {
                let n = base45_value(*a)? + base45_value(*b)? * 45 + base45_value(*c)? * 45 * 45;
                if n > 0xFFFF {
                    bail!("base45 triple out of range");
                }
                out.push((n / 256) as u8);
                out.push((n % 256) as u8);
            }
            [a, b] => {
                let n = base45_value(*a)? + base45_value(*b)? * 45;
                if n > 0xFF {
                    bail!("base45 pair out of range");
                }
                out.push(n as u8);
            }
            _ => unreachable!("length checked above"),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// zlib
// ---------------------------------------------------------------------------

fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data).wrap_err("compress payload")?;
    enc.finish().wrap_err("finish compression")
}

fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    // A profile payload is small; 1 MiB is an absurd upper bound that still
    // caps decompression-bomb inputs from a hostile QR. Read one byte past
    // the cap so oversized streams are REJECTED, not silently truncated
    // into something that might still parse (codex P3).
    const MAX: u64 = 1024 * 1024;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(MAX + 1)
        .read_to_end(&mut out)
        .wrap_err("decompress payload")?;
    if out.len() as u64 > MAX {
        bail!("decompressed payload exceeds {MAX} bytes");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// PIN wrap (Argon2id → ChaCha20-Poly1305)
// ---------------------------------------------------------------------------

fn derive_key(pin: &str, salt: &[u8]) -> Result<[u8; 32]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    // Deliberately heavier than the argon2 crate defaults (64 MiB, t=3):
    // the QR is an OFFLINE artifact, so the KDF cost is the only thing
    // rate-limiting a brute force of the transfer secret. Combined with
    // the 40-bit generated secret this puts exhaustive search in the
    // thousands-of-years range on commodity hardware (codex P1).
    let params =
        Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| eyre::eyre!("argon2 params: {e}"))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(pin.as_bytes(), salt, &mut key)
        .map_err(|e| eyre::eyre!("key derivation failed: {e}"))?;
    Ok(key)
}

fn encrypt(plaintext: &[u8], pin: &str) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
    use chacha20poly1305::{AeadCore, ChaCha20Poly1305};

    let mut salt = [0u8; SALT_LEN];
    use chacha20poly1305::aead::rand_core::RngCore;
    OsRng.fill_bytes(&mut salt);
    let key = derive_key(pin, &salt)?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| eyre::eyre!("encryption failed: {e}"))?;

    let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt(body: &[u8], pin: &str) -> Result<Vec<u8>> {
    use chacha20poly1305::ChaCha20Poly1305;
    use chacha20poly1305::aead::{Aead, KeyInit};

    if body.len() < SALT_LEN + NONCE_LEN + 16 {
        bail!("encrypted payload too short");
    }
    let (salt, rest) = body.split_at(SALT_LEN);
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let key = derive_key(pin, salt)?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    cipher
        .decrypt(nonce.into(), ciphertext)
        .map_err(|_| eyre::eyre!("decryption failed — wrong PIN or corrupted payload"))
}

// ---------------------------------------------------------------------------
// Public encode / decode
// ---------------------------------------------------------------------------

/// Encode a payload as the plain `OCTOS1:` string.
///
/// Refuses secret-bearing payloads unless `allow_plain_secrets` — the QR
/// is a bearer artifact; anyone who photographs it owns its contents.
pub fn encode_plain(payload: &ProfileQrPayload, allow_plain_secrets: bool) -> Result<String> {
    if payload.has_secrets() && !allow_plain_secrets {
        bail!(
            "payload carries secrets; use a PIN (encrypted OCTOS1E) or pass \
             the explicit plain-secrets override"
        );
    }
    let json = serde_json::to_vec(payload).wrap_err("serialize payload")?;
    Ok(format!(
        "{PREFIX_PLAIN}{}",
        base45_encode(&compress(&json)?)
    ))
}

/// Encode a payload as the PIN-wrapped `OCTOS1E:` string.
pub fn encode_encrypted(payload: &ProfileQrPayload, pin: &str) -> Result<String> {
    if pin.len() < 6 {
        bail!("transfer secret must be at least 6 characters");
    }
    let json = serde_json::to_vec(payload).wrap_err("serialize payload")?;
    let sealed = encrypt(&compress(&json)?, pin)?;
    Ok(format!("{PREFIX_ENCRYPTED}{}", base45_encode(&sealed)))
}

/// Decode either format. `pin` is required for `OCTOS1E:`.
pub fn decode(s: &str, pin: Option<&str>) -> Result<ProfileQrPayload> {
    let s = s.trim();
    if let Some(body) = s.strip_prefix(PREFIX_PLAIN) {
        let json = decompress(&base45_decode(body)?)?;
        return serde_json::from_slice(&json).wrap_err("parse payload JSON");
    }
    if let Some(body) = s.strip_prefix(PREFIX_ENCRYPTED) {
        let Some(pin) = pin else {
            bail!("this profile QR is PIN-protected — supply the PIN shown beside it");
        };
        let sealed = base45_decode(body)?;
        let json = decompress(&decrypt(&sealed, pin)?)?;
        return serde_json::from_slice(&json).wrap_err("parse payload JSON");
    }
    bail!("not an octos profile QR payload (missing OCTOS1/OCTOS1E prefix)")
}

/// Render the encoded string as a terminal QR (Unicode half-blocks).
pub fn render_terminal(encoded: &str) -> Result<String> {
    let code = qrcode::QrCode::new(encoded.as_bytes()).wrap_err("QR encode")?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .build())
}

/// Crockford base32 (no I/L/O/U): unambiguous to read off a terminal
/// and type on a phone keyboard.
const SECRET_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generate a random transfer secret: 8 Crockford-base32 chars grouped
/// as `XXXX-XXXX` (40 bits of entropy). A 6-digit PIN (~20 bits) is
/// offline-brute-forceable against a photographed QR even under a slow
/// KDF; 40 bits under the 64 MiB Argon2id profile is not (codex P1).
pub fn generate_pin() -> String {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut rng = chacha20poly1305::aead::OsRng;
    let mut chars = Vec::with_capacity(9);
    for i in 0..8 {
        if i == 4 {
            chars.push(b'-');
        }
        // Rejection sampling for a bias-free draw from the 32-char set.
        chars.push(SECRET_ALPHABET[(rng.next_u32() % 32) as usize]);
    }
    String::from_utf8(chars).expect("ascii alphabet")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HISTORICAL_V1_FIXTURE: &str = r#"{
        "v": 1,
        "kind": "octos-profile",
        "id": "legacy-profile",
        "name": "Legacy Profile",
        "endpoint": "https://octos.example.test",
        "llm": {
            "primary": {
                "family_id": "deepseek",
                "model_id": "deepseek-chat",
                "route": {
                    "api_key_env": "DEEPSEEK_API_KEY"
                }
            },
            "fallbacks": []
        },
        "memory": {
            "max_inject_tokens": 2500
        },
        "embedding": {
            "provider": "openai",
            "model": "text-embedding-3-small"
        },
        "voice_default": "alloy"
    }"#;

    const EXTENSION_BEARING_V1_FIXTURE: &str = r#"{
        "v": 1,
        "kind": "octos-profile",
        "id": "extended-profile",
        "llm": {
            "primary": {
                "family_id": "proxy",
                "model_id": "future-model",
                "route": {
                    "api_key_env": "PROXY_API_KEY",
                    "custom_headers": {
                        "x-route-preview": "enabled"
                    }
                },
                "vendor_capabilities": ["reasoning", "tools-v2"]
            },
            "fallbacks": [],
            "routing_extension": {
                "strategy": "latency-aware"
            }
        },
        "memory": {
            "max_inject_tokens": 4096,
            "retention_extension": {
                "strategy": "semantic"
            }
        },
        "embedding": {
            "provider": "future-provider",
            "model": "future-embedding",
            "batch_extension": {
                "size": 128
            }
        }
    }"#;

    fn sample(with_secrets: bool) -> ProfileQrPayload {
        let mut p = ProfileQrPayload::new("dspfac");
        p.name = Some("DSP Factory".into());
        p.endpoint = Some("https://ada.crew.ominix.io".into());
        p.llm = Some(serde_json::json!({
            "primary": {"family_id": "deepseek", "model_id": "deepseek-v4-pro",
                         "route": {"api_key_env": "DEEPSEEK_API_KEY"}}
        }));
        p.memory = Some(serde_json::json!({"max_inject_tokens": 2500}));
        if with_secrets {
            p.auth_token = Some(format!("octs_{}", "A".repeat(43)));
            p.secrets
                .insert("DEEPSEEK_API_KEY".into(), format!("sk-{}", "0".repeat(32)));
        }
        p
    }

    fn assert_v1_fixture_round_trips_verbatim(fixture: &str) {
        let expected: serde_json::Value =
            serde_json::from_str(fixture).expect("literal v1 fixture");
        let payload: ProfileQrPayload =
            serde_json::from_str(fixture).expect("decode literal v1 fixture");
        assert_eq!(payload.v, 1);

        let encoded = encode_plain(&payload, false).expect("encode literal v1 fixture");
        let decoded = decode(&encoded, None).expect("decode encoded v1 fixture");
        assert_eq!(
            serde_json::to_value(decoded).expect("serialize decoded v1 fixture"),
            expected
        );
    }

    #[test]
    fn historical_v1_payload_remains_decodable_and_lossless() {
        assert_v1_fixture_round_trips_verbatim(HISTORICAL_V1_FIXTURE);
    }

    #[test]
    fn extension_bearing_v1_blocks_round_trip_without_field_loss() {
        assert_v1_fixture_round_trips_verbatim(EXTENSION_BEARING_V1_FIXTURE);
    }

    #[test]
    fn base45_round_trips_including_odd_lengths() {
        for len in [0usize, 1, 2, 3, 63, 64, 400] {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
            let enc = base45_encode(&data);
            assert!(
                enc.bytes().all(|b| BASE45_ALPHABET.contains(&b)),
                "alphabet violation"
            );
            assert_eq!(base45_decode(&enc).unwrap(), data, "len {len}");
        }
    }

    #[test]
    fn base45_rejects_garbage() {
        assert!(base45_decode("ab#").is_err());
        assert!(base45_decode("A").is_err(), "length ≡ 1 mod 3 invalid");
        // A triple decoding above 0xFFFF must be rejected.
        assert!(base45_decode(":::").is_err());
    }

    #[test]
    fn plain_round_trip() {
        let p = sample(false);
        let enc = encode_plain(&p, false).unwrap();
        assert!(enc.starts_with(PREFIX_PLAIN));
        // stays comfortably inside a scannable QR
        assert!(enc.len() < 1000, "unexpectedly large: {}", enc.len());
        let back = decode(&enc, None).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn plain_refuses_secrets_without_override() {
        let p = sample(true);
        assert!(encode_plain(&p, false).is_err());
        // explicit override allowed (Wi-Fi-QR-style bearer semantics)
        let enc = encode_plain(&p, true).unwrap();
        assert_eq!(decode(&enc, None).unwrap(), p);
    }

    #[test]
    fn encrypted_round_trip_and_wrong_pin() {
        let p = sample(true);
        let enc = encode_encrypted(&p, "483920").unwrap();
        assert!(enc.starts_with(PREFIX_ENCRYPTED));
        assert_eq!(decode(&enc, Some("483920")).unwrap(), p);

        let err = decode(&enc, Some("000000")).unwrap_err();
        assert!(err.to_string().contains("wrong PIN"), "{err}");
        assert!(
            decode(&enc, None)
                .unwrap_err()
                .to_string()
                .contains("PIN-protected")
        );
    }

    #[test]
    fn rejects_foreign_strings() {
        assert!(decode("WIFI:T:WPA;S:home;;", None).is_err());
        assert!(decode("OCTOS1:%%%%", None).is_err());
    }

    #[test]
    fn terminal_render_produces_blocks() {
        let enc = encode_plain(&sample(false), false).unwrap();
        let art = render_terminal(&enc).unwrap();
        assert!(art.lines().count() > 20);
    }

    #[test]
    fn generated_secret_is_grouped_crockford_base32() {
        for _ in 0..10 {
            let pin = generate_pin();
            assert_eq!(pin.len(), 9);
            let (a, b) = pin.split_once('-').expect("XXXX-XXXX shape");
            for part in [a, b] {
                assert_eq!(part.len(), 4);
                assert!(
                    part.bytes().all(|c| SECRET_ALPHABET.contains(&c)),
                    "unexpected char in {pin}"
                );
            }
        }
    }

    #[test]
    fn encrypt_rejects_short_secrets() {
        let err = encode_encrypted(&sample(true), "12345").unwrap_err();
        assert!(err.to_string().contains("at least 6"), "{err}");
    }

    #[test]
    fn oversized_decompression_is_rejected_not_truncated() {
        // 2 MiB of zeros zlib-compresses to a few KiB; the decoder must
        // refuse it outright instead of truncating to the first MiB.
        let bomb = compress(&vec![0u8; 2 * 1024 * 1024]).unwrap();
        let err = decompress(&bomb).unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }
}
