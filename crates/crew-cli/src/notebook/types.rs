//! Notebook data model types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A notebook containing sources and notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notebook {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_image: Option<String>,
    #[serde(default)]
    pub source_count: usize,
    #[serde(default)]
    pub note_count: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub owner_id: String,
    /// Inline sources (persisted in the same JSON file).
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Inline notes (persisted in the same JSON file).
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Users this notebook is shared with.
    #[serde(default)]
    pub shared_with: Vec<Share>,
    /// Book metadata for library feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_meta: Option<BookMeta>,
    /// When true, raw source content is hidden; only summaries are served.
    #[serde(default)]
    pub copyright_protected: bool,
}

/// A share grant on a notebook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub id: String,
    pub email: String,
    pub role: ShareRole,
    pub created_at: DateTime<Utc>,
}

/// Role for a notebook share.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShareRole {
    Viewer,
    Editor,
}

/// Book metadata for library integration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BookMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marc_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
}

/// Type of source material.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    Pdf,
    Url,
    Text,
    Docx,
    Pptx,
    Image,
}

/// Processing status of a source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceStatus {
    Uploading,
    Parsing,
    Indexing,
    Ready,
    Error,
}

/// A source document attached to a notebook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub notebook_id: String,
    pub source_type: SourceType,
    pub filename: String,
    pub status: SourceStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default)]
    pub chunks: Vec<Chunk>,
    pub created_at: DateTime<Utc>,
}

/// A chunk of text from a source document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub content: String,
    pub start_offset: usize,
    pub end_offset: usize,
}

/// A note inside a notebook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub notebook_id: String,
    pub content: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
    pub created_from: NoteOrigin,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// How a note was created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NoteOrigin {
    Manual,
    ChatReply,
}
