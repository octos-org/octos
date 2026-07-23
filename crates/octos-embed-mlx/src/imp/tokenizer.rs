//! Tokenization via the `tokenizers` crate loading the model's `tokenizer.json`.
//!
//! EmbeddingGemma uses task-specific prompt prefixes (prepended as plain text
//! BEFORE tokenization) from `config_sentence_transformers.json`:
//!   query    = "task: search result | query: "
//!   document = "title: none | text: "
//! The tokenizer's own post-processor adds `<bos>`(2) … `<eos>`(1); we pass
//! `add_special_tokens = true` so the ids match the Python oracle exactly.

use std::path::Path;

use eyre::{Result, eyre};
use tokenizers::Tokenizer;

pub const QUERY_PROMPT: &str = "task: search result | query: ";
pub const DOC_PROMPT: &str = "title: none | text: ";

pub struct GemmaTokenizer {
    inner: Tokenizer,
}

impl GemmaTokenizer {
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join("tokenizer.json");
        let inner = Tokenizer::from_file(&path).map_err(|e| eyre!("load tokenizer: {e}"))?;
        Ok(Self { inner })
    }

    /// Encode raw text (already prompt-prefixed) to i32 token ids, including the
    /// tokenizer's special tokens (BOS/EOS).
    pub fn encode(&self, text: &str) -> Result<Vec<i32>> {
        let enc = self
            .inner
            .encode(text, true)
            .map_err(|e| eyre!("encode: {e}"))?;
        Ok(enc.get_ids().iter().map(|&id| id as i32).collect())
    }

    pub fn encode_query(&self, text: &str) -> Result<Vec<i32>> {
        self.encode(&format!("{QUERY_PROMPT}{text}"))
    }

    pub fn encode_document(&self, text: &str) -> Result<Vec<i32>> {
        self.encode(&format!("{DOC_PROMPT}{text}"))
    }
}
