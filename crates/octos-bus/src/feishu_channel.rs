//! Feishu/Lark channel with WebSocket long connection or Webhook mode + REST API.
//!
//! Supports both Feishu (China, open.feishu.cn) and Larksuite (global, open.larksuite.com).
//! Set `region` to `"cn"` or `"global"` to select the platform.
//! Set `mode` to `"ws"` (default) for WebSocket long connection or `"webhook"` for HTTP webhook.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use octos_core::{InboundMessage, OutboundMessage};
use eyre::{Result, WrapErr};
use futures::StreamExt;
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

use crate::channel::Channel;
use crate::media::{download_media, is_image};

/// Token refresh interval (slightly under 2 hours).
const TOKEN_TTL_SECS: u64 = 7000;
/// Maximum message IDs to track for dedup.
const MAX_SEEN_IDS: usize = 1000;

fn base_url_for_region(region: &str) -> String {
    match region {
        "global" | "lark" => "https://open.larksuite.com/open-apis".to_string(),
        _ => "https://open.feishu.cn/open-apis".to_string(),
    }
}

/// AES-256-CBC decryption for Lark encrypted events.
fn decrypt_lark_event(encrypt_key: &str, ciphertext_b64: &str) -> Result<String> {
    let key_hash = {
        use std::io::Write;
        let mut hasher = Sha256Writer::default();
        hasher.write_all(encrypt_key.as_bytes()).unwrap();
        hasher.finish()
    };

    let buf = base64_decode(ciphertext_b64).wrap_err("base64 decode failed")?;
    if buf.len() < 16 {
        return Err(eyre::eyre!("ciphertext too short"));
    }

    let iv = &buf[..16];
    let data = &buf[16..];
    if data.len() % 16 != 0 {
        return Err(eyre::eyre!("ciphertext not aligned to block size"));
    }

    let mut plaintext = data.to_vec();
    aes256_cbc_decrypt(&key_hash, iv, &mut plaintext)?;

    // PKCS7 unpad
    if let Some(&pad_len) = plaintext.last() {
        let pad_len = pad_len as usize;
        if pad_len > 0 && pad_len <= 16 && plaintext.len() >= pad_len {
            plaintext.truncate(plaintext.len() - pad_len);
        }
    }

    String::from_utf8(plaintext).wrap_err("decrypted data not valid UTF-8")
}

/// Minimal SHA-256 implementation (no external dep).
#[derive(Default)]
struct Sha256Writer {
    data: Vec<u8>,
}

impl std::io::Write for Sha256Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sha256Writer {
    fn finish(self) -> [u8; 32] {
        sha256(&self.data)
    }
}

/// SHA-256 hash (pure Rust, minimal).
fn sha256(data: &[u8]) -> [u8; 32] {
    // Use reqwest's dependency chain — ring or similar is already linked.
    // Actually, we'll use a simple software implementation to avoid adding deps.
    sha256_impl(data)
}

fn sha256_impl(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pre-processing: padding
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit block
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut result = [0u8; 32];
    for (i, &val) in h.iter().enumerate() {
        result[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
    }
    result
}

/// Minimal base64 decode (standard alphabet with padding).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn val(c: u8) -> Result<u8> {
        if c == b'=' {
            return Ok(0);
        }
        TABLE
            .iter()
            .position(|&x| x == c)
            .map(|p| p as u8)
            .ok_or_else(|| eyre::eyre!("invalid base64 character"))
    }

    let input = input.trim();
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'\n' && b != b'\r')
        .collect();
    if bytes.len() % 4 != 0 {
        return Err(eyre::eyre!("invalid base64 length"));
    }

    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        let c = val(chunk[2])?;
        let d = val(chunk[3])?;

        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

/// AES-256-CBC decrypt in place (PKCS7 padding NOT removed).
fn aes256_cbc_decrypt(key: &[u8; 32], iv: &[u8], data: &mut [u8]) -> Result<()> {
    if data.len() % 16 != 0 {
        return Err(eyre::eyre!("data not aligned to 16 bytes"));
    }

    let round_keys = aes256_key_expansion(key);
    let mut prev_block = [0u8; 16];
    prev_block.copy_from_slice(iv);

    for i in (0..data.len()).step_by(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(&data[i..i + 16]);
        let cipher_block = block;

        aes256_decrypt_block(&round_keys, &mut block);

        for j in 0..16 {
            block[j] ^= prev_block[j];
        }
        prev_block = cipher_block;
        data[i..i + 16].copy_from_slice(&block);
    }
    Ok(())
}

/// AES-256 key expansion (15 round keys).
fn aes256_key_expansion(key: &[u8; 32]) -> [[u8; 16]; 15] {
    const SBOX: [u8; 256] = [
        0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab,
        0x76, 0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4,
        0x72, 0xc0, 0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71,
        0xd8, 0x31, 0x15, 0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2,
        0xeb, 0x27, 0xb2, 0x75, 0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6,
        0xb3, 0x29, 0xe3, 0x2f, 0x84, 0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb,
        0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf, 0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45,
        0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8, 0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5,
        0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2, 0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44,
        0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73, 0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a,
        0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb, 0xe0, 0x32, 0x3a, 0x0a, 0x49,
        0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79, 0xe7, 0xc8, 0x37, 0x6d,
        0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08, 0xba, 0x78, 0x25,
        0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a, 0x70, 0x3e,
        0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e, 0xe1,
        0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
        0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb,
        0x16,
    ];

    const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

    fn sub_word(w: [u8; 4]) -> [u8; 4] {
        [
            SBOX[w[0] as usize],
            SBOX[w[1] as usize],
            SBOX[w[2] as usize],
            SBOX[w[3] as usize],
        ]
    }

    fn rot_word(w: [u8; 4]) -> [u8; 4] {
        [w[1], w[2], w[3], w[0]]
    }

    // 60 words for AES-256
    let mut w = [[0u8; 4]; 60];
    for i in 0..8 {
        w[i] = [key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]];
    }

    for i in 8..60 {
        let mut temp = w[i - 1];
        if i % 8 == 0 {
            temp = sub_word(rot_word(temp));
            temp[0] ^= RCON[i / 8 - 1];
        } else if i % 8 == 4 {
            temp = sub_word(temp);
        }
        for j in 0..4 {
            w[i][j] = w[i - 8][j] ^ temp[j];
        }
    }

    let mut round_keys = [[0u8; 16]; 15];
    for r in 0..15 {
        for c in 0..4 {
            round_keys[r][c * 4..c * 4 + 4].copy_from_slice(&w[r * 4 + c]);
        }
    }
    round_keys
}

/// AES-256 decrypt a single 16-byte block.
fn aes256_decrypt_block(round_keys: &[[u8; 16]; 15], block: &mut [u8; 16]) {
    const INV_SBOX: [u8; 256] = [
        0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38, 0xbf, 0x40, 0xa3, 0x9e, 0x81, 0xf3, 0xd7,
        0xfb, 0x7c, 0xe3, 0x39, 0x82, 0x9b, 0x2f, 0xff, 0x87, 0x34, 0x8e, 0x43, 0x44, 0xc4, 0xde,
        0xe9, 0xcb, 0x54, 0x7b, 0x94, 0x32, 0xa6, 0xc2, 0x23, 0x3d, 0xee, 0x4c, 0x95, 0x0b, 0x42,
        0xfa, 0xc3, 0x4e, 0x08, 0x2e, 0xa1, 0x66, 0x28, 0xd9, 0x24, 0xb2, 0x76, 0x5b, 0xa2, 0x49,
        0x6d, 0x8b, 0xd1, 0x25, 0x72, 0xf8, 0xf6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xd4, 0xa4, 0x5c,
        0xcc, 0x5d, 0x65, 0xb6, 0x92, 0x6c, 0x70, 0x48, 0x50, 0xfd, 0xed, 0xb9, 0xda, 0x5e, 0x15,
        0x46, 0x57, 0xa7, 0x8d, 0x9d, 0x84, 0x90, 0xd8, 0xab, 0x00, 0x8c, 0xbc, 0xd3, 0x0a, 0xf7,
        0xe4, 0x58, 0x05, 0xb8, 0xb3, 0x45, 0x06, 0xd0, 0x2c, 0x1e, 0x8f, 0xca, 0x3f, 0x0f, 0x02,
        0xc1, 0xaf, 0xbd, 0x03, 0x01, 0x13, 0x8a, 0x6b, 0x3a, 0x91, 0x11, 0x41, 0x4f, 0x67, 0xdc,
        0xea, 0x97, 0xf2, 0xcf, 0xce, 0xf0, 0xb4, 0xe6, 0x73, 0x96, 0xac, 0x74, 0x22, 0xe7, 0xad,
        0x35, 0x85, 0xe2, 0xf9, 0x37, 0xe8, 0x1c, 0x75, 0xdf, 0x6e, 0x47, 0xf1, 0x1a, 0x71, 0x1d,
        0x29, 0xc5, 0x89, 0x6f, 0xb7, 0x62, 0x0e, 0xaa, 0x18, 0xbe, 0x1b, 0xfc, 0x56, 0x3e, 0x4b,
        0xc6, 0xd2, 0x79, 0x20, 0x9a, 0xdb, 0xc0, 0xfe, 0x78, 0xcd, 0x5a, 0xf4, 0x1f, 0xdd, 0xa8,
        0x33, 0x88, 0x07, 0xc7, 0x31, 0xb1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xec, 0x5f, 0x60, 0x51,
        0x7f, 0xa9, 0x19, 0xb5, 0x4a, 0x0d, 0x2d, 0xe5, 0x7a, 0x9f, 0x93, 0xc9, 0x9c, 0xef, 0xa0,
        0xe0, 0x3b, 0x4d, 0xae, 0x2a, 0xf5, 0xb0, 0xc8, 0xeb, 0xbb, 0x3c, 0x83, 0x53, 0x99, 0x61,
        0x17, 0x2b, 0x04, 0x7e, 0xba, 0x77, 0xd6, 0x26, 0xe1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0c,
        0x7d,
    ];

    fn gmul(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0u8;
        for _ in 0..8 {
            if b & 1 != 0 {
                p ^= a;
            }
            let hi = a & 0x80;
            a <<= 1;
            if hi != 0 {
                a ^= 0x1b;
            }
            b >>= 1;
        }
        p
    }

    fn add_round_key(state: &mut [u8; 16], rk: &[u8; 16]) {
        for i in 0..16 {
            state[i] ^= rk[i];
        }
    }

    fn inv_sub_bytes(state: &mut [u8; 16]) {
        for b in state.iter_mut() {
            *b = INV_SBOX[*b as usize];
        }
    }

    fn inv_shift_rows(state: &mut [u8; 16]) {
        // Row 1: shift right by 1
        let t = state[13];
        state[13] = state[9];
        state[9] = state[5];
        state[5] = state[1];
        state[1] = t;
        // Row 2: shift right by 2
        let (t0, t1) = (state[2], state[6]);
        state[2] = state[10];
        state[6] = state[14];
        state[10] = t0;
        state[14] = t1;
        // Row 3: shift right by 3
        let t = state[3];
        state[3] = state[7];
        state[7] = state[11];
        state[11] = state[15];
        state[15] = t;
    }

    fn inv_mix_columns(state: &mut [u8; 16]) {
        for c in 0..4 {
            let i = c * 4;
            let (s0, s1, s2, s3) = (state[i], state[i + 1], state[i + 2], state[i + 3]);
            state[i] = gmul(s0, 14) ^ gmul(s1, 11) ^ gmul(s2, 13) ^ gmul(s3, 9);
            state[i + 1] = gmul(s0, 9) ^ gmul(s1, 14) ^ gmul(s2, 11) ^ gmul(s3, 13);
            state[i + 2] = gmul(s0, 13) ^ gmul(s1, 9) ^ gmul(s2, 14) ^ gmul(s3, 11);
            state[i + 3] = gmul(s0, 11) ^ gmul(s1, 13) ^ gmul(s2, 9) ^ gmul(s3, 14);
        }
    }

    // Initial round key addition (last round key first for decryption)
    add_round_key(block, &round_keys[14]);

    // Rounds 13..1
    for round in (1..14).rev() {
        inv_shift_rows(block);
        inv_sub_bytes(block);
        add_round_key(block, &round_keys[round]);
        inv_mix_columns(block);
    }

    // Final round (no inv_mix_columns)
    inv_shift_rows(block);
    inv_sub_bytes(block);
    add_round_key(block, &round_keys[0]);
}

/// SHA-256 signature verification for Lark webhook events.
fn verify_signature(timestamp: &str, nonce: &str, encrypt_key: &str, body: &str) -> String {
    let content = format!("{timestamp}{nonce}{encrypt_key}{body}");
    let hash = sha256(content.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct FeishuChannel {
    app_id: String,
    app_secret: String,
    base_url: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    http: Client,
    media_dir: PathBuf,
    token_cache: Arc<tokio::sync::Mutex<Option<(String, Instant)>>>,
    seen_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    /// "ws" for WebSocket long connection, "webhook" for HTTP webhook mode.
    mode: String,
    /// Port for webhook HTTP server (only used in webhook mode).
    webhook_port: u16,
    /// Optional encrypt key for AES-256-CBC decryption.
    encrypt_key: Option<String>,
    /// Optional verification token for event validation.
    verification_token: Option<String>,
}

impl FeishuChannel {
    pub fn new(
        app_id: &str,
        app_secret: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        region: &str,
        media_dir: PathBuf,
    ) -> Self {
        Self {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            base_url: base_url_for_region(region),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            http: Client::new(),
            media_dir,
            token_cache: Arc::new(tokio::sync::Mutex::new(None)),
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            mode: "ws".to_string(),
            webhook_port: 9321,
            encrypt_key: None,
            verification_token: None,
        }
    }

    /// Set mode: "ws" for WebSocket, "webhook" for HTTP webhook.
    pub fn with_mode(mut self, mode: &str) -> Self {
        self.mode = mode.to_string();
        self
    }

    /// Set webhook port (default 9321).
    pub fn with_webhook_port(mut self, port: u16) -> Self {
        self.webhook_port = port;
        self
    }

    /// Set encrypt key for AES-256-CBC event decryption.
    pub fn with_encrypt_key(mut self, key: Option<String>) -> Self {
        self.encrypt_key = key;
        self
    }

    /// Set verification token for event validation.
    pub fn with_verification_token(mut self, token: Option<String>) -> Self {
        self.verification_token = token;
        self
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    /// Get or refresh tenant access token.
    async fn get_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((ref token, ref created)) = *cache {
            if created.elapsed().as_secs() < TOKEN_TTL_SECS {
                return Ok(token.clone());
            }
        }

        let resp: serde_json::Value = self
            .http
            .post(format!(
                "{}/auth/v3/tenant_access_token/internal",
                self.base_url
            ))
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .wrap_err("failed to get tenant token")?
            .json()
            .await?;

        let token = resp
            .get("tenant_access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let msg = resp
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eyre::eyre!("Feishu token error: {msg}")
            })?
            .to_string();

        *cache = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// Get WebSocket gateway URL from Feishu bot gateway endpoint.
    async fn get_ws_url(&self) -> Result<String> {
        let token = self.get_token().await?;
        let resp: serde_json::Value = self
            .http
            .post(format!("{}/callback/ws/endpoint", self.base_url))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .wrap_err("failed to get Feishu WS endpoint")?
            .json()
            .await?;

        let data = resp.get("data").ok_or_else(|| {
            let msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            eyre::eyre!("Feishu WS endpoint error: {msg}")
        })?;

        let url = data
            .get("URL")
            .or_else(|| data.get("url"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("no URL in Feishu WS endpoint response"))?;

        Ok(url.to_string())
    }

    /// Download a media resource from a Feishu message.
    async fn download_feishu_media(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
        ext: &str,
    ) -> Result<PathBuf> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/im/v1/messages/{}/resources/{}?type={}",
            self.base_url, message_id, file_key, resource_type
        );
        let filename = format!("feishu_{}{}", Utc::now().timestamp_millis(), ext);
        download_media(
            &self.http,
            &url,
            &[("Authorization", &format!("Bearer {token}"))],
            &self.media_dir,
            &filename,
        )
        .await
    }

    /// Upload an image and return the image_key.
    async fn upload_image(&self, file_path: &str) -> Result<String> {
        let token = self.get_token().await?;
        let data = std::fs::read(file_path).wrap_err("failed to read image file")?;
        let filename = std::path::Path::new(file_path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.png".to_string());

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename)
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);

        let resp: serde_json::Value = self
            .http
            .post(format!("{}/im/v1/images", self.base_url))
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .wrap_err("failed to upload image to Feishu")?
            .json()
            .await?;

        resp.get("data")
            .and_then(|d| d.get("image_key"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                let msg = resp
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eyre::eyre!("Feishu image upload error: {msg}")
            })
    }

    /// Upload a file and return the file_key.
    async fn upload_file(&self, file_path: &str) -> Result<String> {
        let token = self.get_token().await?;
        let data = std::fs::read(file_path).wrap_err("failed to read file")?;
        let filename = std::path::Path::new(file_path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.clone())
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("file_type", "stream")
            .text("file_name", filename)
            .part("file", part);

        let resp: serde_json::Value = self
            .http
            .post(format!("{}/im/v1/files", self.base_url))
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .wrap_err("failed to upload file to Feishu")?
            .json()
            .await?;

        resp.get("data")
            .and_then(|d| d.get("file_key"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                let msg = resp
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eyre::eyre!("Feishu file upload error: {msg}")
            })
    }

    /// Send a typed message via Feishu REST API.
    async fn send_message(&self, chat_id: &str, msg_type: &str, content: &str) -> Result<()> {
        self.send_message_returning_id(chat_id, msg_type, content)
            .await?;
        Ok(())
    }

    /// Send a message and return its message_id from the API response.
    async fn send_message_returning_id(
        &self,
        chat_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<Option<String>> {
        let token = self.get_token().await?;
        let id_type = Self::receive_id_type(chat_id);

        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": msg_type,
            "content": content,
        });

        let resp: serde_json::Value = self
            .http
            .post(format!(
                "{}/im/v1/messages?receive_id_type={id_type}",
                self.base_url
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send Feishu message")?
            .json()
            .await?;

        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let err_msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("Feishu send error: {err_msg}");
            return Ok(None);
        }

        // Extract message_id from response: { "data": { "message_id": "om_..." } }
        let message_id = resp
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(message_id)
    }

    /// Check if a message ID has been seen; add if not. Trims when over capacity.
    fn dedup_check(&self, msg_id: &str) -> bool {
        let mut seen = self.seen_ids.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains(msg_id) {
            return true;
        }
        if seen.len() >= MAX_SEEN_IDS {
            seen.clear();
        }
        seen.insert(msg_id.to_string());
        false
    }

    /// Determine receive_id_type from chat_id prefix.
    fn receive_id_type(chat_id: &str) -> &'static str {
        if chat_id.starts_with("oc_") {
            "chat_id"
        } else {
            "open_id"
        }
    }

    /// Parse event JSON (shared between WS and webhook modes).
    /// Returns Some(InboundMessage) if the event is valid and should be dispatched.
    async fn parse_event(&self, envelope: &serde_json::Value) -> Option<InboundMessage> {
        let event_type = envelope
            .get("header")
            .and_then(|h| h.get("event_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if event_type != "im.message.receive_v1" {
            debug!(event_type, "Feishu: ignoring non-message event");
            return None;
        }

        let event = envelope.get("event")?;
        let message = event.get("message")?;

        let message_id = message
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if message_id.is_empty() || self.dedup_check(message_id) {
            debug!(message_id, "Feishu: dedup filtered message");
            return None;
        }

        let sender_id = event
            .get("sender")
            .and_then(|s| s.get("sender_id"))
            .and_then(|s| s.get("open_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let chat_id = message
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if sender_id.is_empty() || chat_id.is_empty() {
            return None;
        }

        if !self.check_allowed(sender_id) {
            return None;
        }

        let msg_type = message
            .get("message_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let content_json: Option<serde_json::Value> = message
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok());

        let mut content = String::new();
        let mut media = Vec::new();

        match msg_type {
            "text" => {
                content = content_json
                    .as_ref()
                    .and_then(|v| v.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
            }
            "image" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("image_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "image", ".png")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu image: {e}"),
                    }
                }
            }
            "file" => {
                let file_key = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let file_name = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !file_key.is_empty() {
                    let ext = std::path::Path::new(file_name)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    match self
                        .download_feishu_media(message_id, file_key, "file", &ext)
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu file: {e}"),
                    }
                }
            }
            "audio" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "file", ".ogg")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu audio: {e}"),
                    }
                }
            }
            "media" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "file", ".mp4")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu video: {e}"),
                    }
                }
            }
            "sticker" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "file", ".png")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu sticker: {e}"),
                    }
                }
            }
            _ => {
                content = format!("[{msg_type} message]");
            }
        }

        if content.is_empty() && media.is_empty() {
            debug!(
                message_id,
                msg_type, "Feishu: empty content and media, skipping"
            );
            return None;
        }

        info!(
            message_id,
            msg_type,
            media_count = media.len(),
            "Feishu: parsed event"
        );

        Some(InboundMessage {
            channel: "feishu".into(),
            sender_id: sender_id.to_string(),
            chat_id: chat_id.to_string(),
            content,
            timestamp: Utc::now(),
            media,
            metadata: serde_json::json!({
                "feishu": {
                    "message_id": message_id,
                    "message_type": msg_type,
                }
            }),
            message_id: None,
        })
    }

    /// Run WebSocket long connection mode.
    async fn start_ws(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let ws_url = match self.get_ws_url().await {
                Ok(url) => url,
                Err(e) => {
                    error!("Failed to get Feishu WS URL: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let (ws_stream, _) = match connect_async(&ws_url).await {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to connect Feishu WebSocket: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            info!("Feishu WebSocket connected");
            let (_ws_tx, mut ws_rx) = ws_stream.split();

            while let Some(frame) = ws_rx.next().await {
                if self.shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let data = match frame {
                    Ok(WsMessage::Text(text)) => text,
                    Ok(WsMessage::Close(_)) => {
                        info!("Feishu WebSocket closed by server");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        warn!("Feishu WebSocket error: {e}");
                        break;
                    }
                };

                let envelope: serde_json::Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("Failed to parse Feishu envelope: {e}");
                        continue;
                    }
                };

                if let Some(inbound) = self.parse_event(&envelope).await {
                    if inbound_tx.send(inbound).await.is_err() {
                        return Ok(());
                    }
                }
            }

            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            warn!("Feishu WebSocket disconnected, reconnecting in 2s...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        Ok(())
    }

    /// Run webhook HTTP server mode.
    async fn start_webhook(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        use axum::{
            Router, extract::State, http::HeaderMap, response::IntoResponse, routing::post,
        };

        #[derive(Clone)]
        struct WebhookState {
            encrypt_key: Option<String>,
            verification_token: Option<String>,
            inbound_tx: mpsc::Sender<serde_json::Value>,
        }

        async fn handle_webhook(
            State(state): State<WebhookState>,
            headers: HeaderMap,
            body: String,
        ) -> impl IntoResponse {
            // Try to parse the body as JSON
            let body_json: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Feishu webhook: invalid JSON body: {e}");
                    return axum::http::Response::builder()
                        .status(400)
                        .header("Content-Type", "application/json")
                        .body(serde_json::json!({"error": "invalid json"}).to_string())
                        .unwrap();
                }
            };

            // Signature verification if encrypt_key is set
            if let Some(ref ek) = state.encrypt_key {
                let timestamp = headers
                    .get("X-Lark-Request-Timestamp")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let nonce = headers
                    .get("X-Lark-Request-Nonce")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let expected_sig = headers
                    .get("X-Lark-Signature")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");

                if !timestamp.is_empty() && !nonce.is_empty() && !expected_sig.is_empty() {
                    let computed = verify_signature(timestamp, nonce, ek, &body);
                    if computed != expected_sig {
                        warn!("Feishu webhook: signature mismatch");
                        return axum::http::Response::builder()
                            .status(403)
                            .header("Content-Type", "application/json")
                            .body(serde_json::json!({"error": "signature mismatch"}).to_string())
                            .unwrap();
                    }
                }
            }

            // Decrypt if encrypted
            let event_json = if let Some(encrypt_str) =
                body_json.get("encrypt").and_then(|v| v.as_str())
            {
                if let Some(ref ek) = state.encrypt_key {
                    match decrypt_lark_event(ek, encrypt_str) {
                        Ok(decrypted) => match serde_json::from_str(&decrypted) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("Feishu webhook: failed to parse decrypted event: {e}");
                                return axum::http::Response::builder()
                                    .status(400)
                                    .header("Content-Type", "application/json")
                                    .body(
                                        serde_json::json!({"error": "decrypt parse error"})
                                            .to_string(),
                                    )
                                    .unwrap();
                            }
                        },
                        Err(e) => {
                            warn!("Feishu webhook: decryption failed: {e}");
                            return axum::http::Response::builder()
                                .status(400)
                                .header("Content-Type", "application/json")
                                .body(serde_json::json!({"error": "decryption failed"}).to_string())
                                .unwrap();
                        }
                    }
                } else {
                    warn!("Feishu webhook: received encrypted event but no encrypt_key configured");
                    return axum::http::Response::builder()
                        .status(400)
                        .header("Content-Type", "application/json")
                        .body(serde_json::json!({"error": "no encrypt key configured"}).to_string())
                        .unwrap();
                }
            } else {
                body_json
            };

            // Handle url_verification challenge
            if event_json.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
                let challenge = event_json
                    .get("challenge")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                info!("Feishu webhook: url_verification challenge received");
                return axum::http::Response::builder()
                    .status(200)
                    .header("Content-Type", "application/json")
                    .body(serde_json::json!({"challenge": challenge}).to_string())
                    .unwrap();
            }

            // Verification token check (for non-encrypted plaintext events)
            if let Some(ref vt) = state.verification_token {
                let event_token = event_json
                    .get("token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !event_token.is_empty() && event_token != vt {
                    warn!("Feishu webhook: verification token mismatch");
                    return axum::http::Response::builder()
                        .status(403)
                        .header("Content-Type", "application/json")
                        .body(serde_json::json!({"error": "token mismatch"}).to_string())
                        .unwrap();
                }
            }

            // Forward event to the channel for processing
            let _ = state.inbound_tx.send(event_json).await;

            axum::http::Response::builder()
                .status(200)
                .body("ok".to_string())
                .unwrap()
        }

        // Internal channel for passing parsed events
        let (event_tx, mut event_rx) = mpsc::channel::<serde_json::Value>(100);

        let state = WebhookState {
            encrypt_key: self.encrypt_key.clone(),
            verification_token: self.verification_token.clone(),
            inbound_tx: event_tx,
        };

        let app = Router::new()
            .route("/webhook/event", post(handle_webhook))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .wrap_err_with(|| format!("failed to bind webhook server to {addr}"))?;
        info!(port = self.webhook_port, "Feishu webhook server listening");

        let shutdown = self.shutdown.clone();

        // Spawn the HTTP server
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    while !server_shutdown.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                })
                .await
                .ok();
        });

        // Process incoming events
        while let Some(envelope) = event_rx.recv().await {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            if let Some(inbound) = self.parse_event(&envelope).await {
                info!(sender = %inbound.sender_id, chat = %inbound.chat_id, "Feishu: sending to inbound bus");
                if inbound_tx.send(inbound).await.is_err() {
                    error!("Feishu: inbound_tx send failed (receiver dropped)");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!(base_url = %self.base_url, mode = %self.mode, "Starting Feishu/Lark channel");

        match self.mode.as_str() {
            "webhook" => self.start_webhook(inbound_tx).await?,
            _ => self.start_ws(inbound_tx).await?,
        }

        info!("Feishu channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        // Send text content as interactive card with markdown
        if !msg.content.is_empty() {
            let card = serde_json::json!({
                "elements": [
                    {
                        "tag": "markdown",
                        "content": msg.content,
                    }
                ]
            });
            self.send_message(&msg.chat_id, "interactive", &card.to_string())
                .await?;
        }

        // Send media files
        for path in &msg.media {
            if is_image(path) {
                match self.upload_image(path).await {
                    Ok(image_key) => {
                        let content = serde_json::json!({"image_key": image_key}).to_string();
                        self.send_message(&msg.chat_id, "image", &content).await?;
                    }
                    Err(e) => warn!("failed to upload Feishu image: {e}"),
                }
            } else {
                match self.upload_file(path).await {
                    Ok(file_key) => {
                        let filename = std::path::Path::new(path)
                            .file_name()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_else(|| "file".to_string());
                        let content =
                            serde_json::json!({"file_key": file_key, "file_name": filename})
                                .to_string();
                        self.send_message(&msg.chat_id, "file", &content).await?;
                    }
                    Err(e) => warn!("failed to upload Feishu file: {e}"),
                }
            }
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        if msg.content.is_empty() {
            return Ok(None);
        }
        let card = serde_json::json!({
            "elements": [
                {
                    "tag": "markdown",
                    "content": msg.content,
                }
            ]
        });
        self.send_message_returning_id(&msg.chat_id, "interactive", &card.to_string())
            .await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        new_content: &str,
    ) -> Result<()> {
        let token = self.get_token().await?;
        let card = serde_json::json!({
            "elements": [
                {
                    "tag": "markdown",
                    "content": new_content,
                }
            ]
        });
        let body = serde_json::json!({
            "msg_type": "interactive",
            "content": card.to_string(),
        });

        let resp: serde_json::Value = self
            .http
            .patch(format!("{}/im/v1/messages/{}", self.base_url, message_id))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to edit Feishu message")?
            .json()
            .await?;

        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let err_msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("Feishu edit error: {err_msg}");
        }
        Ok(())
    }

    async fn delete_message(&self, _chat_id: &str, message_id: &str) -> Result<()> {
        let token = self.get_token().await?;

        let resp: serde_json::Value = self
            .http
            .delete(format!("{}/im/v1/messages/{}", self.base_url, message_id))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .wrap_err("failed to delete Feishu message")?
            .json()
            .await?;

        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let err_msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("Feishu delete error: {err_msg}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<&str>) -> FeishuChannel {
        make_channel_with_region(allowed, "cn")
    }

    fn make_channel_with_region(allowed: Vec<&str>, region: &str) -> FeishuChannel {
        FeishuChannel {
            app_id: "test_id".into(),
            app_secret: "test_secret".into(),
            base_url: base_url_for_region(region),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            http: Client::new(),
            media_dir: PathBuf::from("/tmp/test-feishu-media"),
            token_cache: Arc::new(tokio::sync::Mutex::new(None)),
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            mode: "ws".into(),
            webhook_port: 9321,
            encrypt_key: None,
            verification_token: None,
        }
    }

    #[test]
    fn test_base_url_cn() {
        let ch = make_channel_with_region(vec![], "cn");
        assert_eq!(ch.base_url, "https://open.feishu.cn/open-apis");
    }

    #[test]
    fn test_base_url_global() {
        let ch = make_channel_with_region(vec![], "global");
        assert_eq!(ch.base_url, "https://open.larksuite.com/open-apis");
    }

    #[test]
    fn test_base_url_lark_alias() {
        let ch = make_channel_with_region(vec![], "lark");
        assert_eq!(ch.base_url, "https://open.larksuite.com/open-apis");
    }

    #[test]
    fn test_base_url_default_cn() {
        let ch = make_channel_with_region(vec![], "anything_else");
        assert_eq!(ch.base_url, "https://open.feishu.cn/open-apis");
    }

    #[test]
    fn test_is_allowed_empty_list() {
        let ch = make_channel(vec![]);
        assert!(ch.is_allowed("anyone"));
    }

    #[test]
    fn test_is_allowed_matching() {
        let ch = make_channel(vec!["ou_123", "ou_456"]);
        assert!(ch.is_allowed("ou_123"));
        assert!(!ch.is_allowed("ou_789"));
    }

    #[test]
    fn test_receive_id_type() {
        assert_eq!(FeishuChannel::receive_id_type("oc_abc123"), "chat_id");
        assert_eq!(FeishuChannel::receive_id_type("ou_xyz789"), "open_id");
        assert_eq!(FeishuChannel::receive_id_type("other"), "open_id");
    }

    #[test]
    fn test_dedup() {
        let ch = make_channel(vec![]);
        assert!(!ch.dedup_check("msg1"));
        assert!(ch.dedup_check("msg1")); // duplicate
        assert!(!ch.dedup_check("msg2"));
    }

    #[test]
    fn test_dedup_overflow_clears() {
        let ch = make_channel(vec![]);
        for i in 0..MAX_SEEN_IDS {
            ch.dedup_check(&format!("msg_{i}"));
        }
        assert!(!ch.dedup_check("new_msg"));
        assert!(!ch.dedup_check("msg_0"));
    }

    #[test]
    fn test_message_content_text() {
        let content_str = r#"{"text":"Hello world"}"#;
        let parsed: serde_json::Value = serde_json::from_str(content_str).unwrap();
        let text = parsed.get("text").and_then(|t| t.as_str()).unwrap();
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn test_message_content_image() {
        let content_str = r#"{"image_key":"img_abc123"}"#;
        let parsed: serde_json::Value = serde_json::from_str(content_str).unwrap();
        let key = parsed.get("image_key").and_then(|t| t.as_str()).unwrap();
        assert_eq!(key, "img_abc123");
    }

    #[test]
    fn test_message_content_file() {
        let content_str = r#"{"file_key":"file_xyz","file_name":"report.pdf"}"#;
        let parsed: serde_json::Value = serde_json::from_str(content_str).unwrap();
        let key = parsed.get("file_key").and_then(|t| t.as_str()).unwrap();
        let name = parsed.get("file_name").and_then(|t| t.as_str()).unwrap();
        assert_eq!(key, "file_xyz");
        assert_eq!(name, "report.pdf");
    }

    #[test]
    fn test_sha256_basic() {
        let hash = sha256(b"test key");
        // Known SHA-256 of "test key"
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "fa2bdca424f01f01ffb48df93acc35d439c7fd331a1a7fba6ac2fd83aa9ab31a"
        );
    }

    #[test]
    fn test_base64_decode() {
        let decoded = base64_decode("aGVsbG8gd29ybGQ=").unwrap();
        assert_eq!(decoded, b"hello world");
    }

    #[test]
    fn test_decrypt_lark_event() {
        // Official test vector from Lark docs: encrypt key="test key", plaintext="hello world"
        let result =
            decrypt_lark_event("test key", "P37w+VZImNgPEO1RBhJ6RtKl7n6zymIbEG1pReEzghk=").unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_verify_signature() {
        let sig = verify_signature("ts123", "nonce456", "mykey", r#"{"test":"body"}"#);
        // Should be a 64-char hex string
        assert_eq!(sig.len(), 64);
        // Deterministic
        let sig2 = verify_signature("ts123", "nonce456", "mykey", r#"{"test":"body"}"#);
        assert_eq!(sig, sig2);
    }

    #[test]
    fn test_with_mode() {
        let ch = FeishuChannel::new(
            "id",
            "secret",
            vec![],
            Arc::new(AtomicBool::new(false)),
            "global",
            PathBuf::from("/tmp"),
        )
        .with_mode("webhook")
        .with_webhook_port(8080);
        assert_eq!(ch.mode, "webhook");
        assert_eq!(ch.webhook_port, 8080);
    }
}
