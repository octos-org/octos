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
