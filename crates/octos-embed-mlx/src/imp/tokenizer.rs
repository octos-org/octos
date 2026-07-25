//! Tokenization via the `tokenizers` crate loading the model's `tokenizer.json`.
//!
//! EmbeddingGemma uses task-specific prompt prefixes (prepended as plain text
//! BEFORE tokenization) from `config_sentence_transformers.json`:
//!   query    = "task: search result | query: "
//!   document = "title: none | text: "
//! The tokenizer's own post-processor adds `<bos>`(2) … `<eos>`(1); we pass
//! `add_special_tokens = true` so the ids match the Python oracle exactly.
//!
//! Inputs longer than the model's `max_seq_length` (2048 for EmbeddingGemma,
//! read from `sentence_bert_config.json`) are truncated, preserving the closing
//! `<eos>`. Without this a single oversized memory episode would run RoPE far
//! past the trained range and allocate an O(L²) attention matrix.

use std::path::Path;

use eyre::{Result, eyre};
use serde::Deserialize;
use tokenizers::Tokenizer;

pub const QUERY_PROMPT: &str = "task: search result | query: ";
pub const DOC_PROMPT: &str = "title: none | text: ";

/// Fallback when `sentence_bert_config.json` is absent or unparseable.
const DEFAULT_MAX_SEQ_LEN: usize = 2048;

/// Fallback `<eos>` id for EmbeddingGemma when the token is not in the
/// tokenizer's vocab under that name.
const EOS_FALLBACK: u32 = 1;

#[derive(Deserialize)]
struct SbertConfig {
    max_seq_length: Option<usize>,
}

pub struct GemmaTokenizer {
    inner: Tokenizer,
    max_len: usize,
    eos_id: u32,
}

impl GemmaTokenizer {
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join("tokenizer.json");
        let inner = Tokenizer::from_file(&path).map_err(|e| eyre!("load tokenizer: {e}"))?;

        let max_len = std::fs::read_to_string(dir.join("sentence_bert_config.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<SbertConfig>(&s).ok())
            .and_then(|c| c.max_seq_length)
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_MAX_SEQ_LEN);
        let eos_id = inner.token_to_id("<eos>").unwrap_or(EOS_FALLBACK);

        Ok(Self {
            inner,
            max_len,
            eos_id,
        })
    }

    /// The active truncation limit, in tokens (prompt prefix included).
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Encode raw text (already prompt-prefixed) to i32 token ids, including the
    /// tokenizer's special tokens (BOS/EOS), truncated to [`Self::max_len`].
    pub fn encode(&self, text: &str) -> Result<Vec<i32>> {
        let enc = self
            .inner
            .encode(text, true)
            .map_err(|e| eyre!("encode: {e}"))?;
        let ids = enc.get_ids();
        let mut out: Vec<i32> = ids.iter().take(self.max_len).map(|&id| id as i32).collect();
        // Keep the sequence `<eos>`-terminated after a hard cut.
        if ids.len() > self.max_len {
            if let Some(last) = out.last_mut() {
                *last = self.eos_id as i32;
            }
        }
        Ok(out)
    }

    pub fn encode_query(&self, text: &str) -> Result<Vec<i32>> {
        self.encode(&format!("{QUERY_PROMPT}{text}"))
    }

    pub fn encode_document(&self, text: &str) -> Result<Vec<i32>> {
        self.encode(&format!("{DOC_PROMPT}{text}"))
    }
}
