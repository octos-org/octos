//! Notebook, Source, Note, and Chat API handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::Utc;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::notebook::{
    Chunk, Note, NoteOrigin, Notebook, NotebookStore, Source, SourceStatus, SourceType,
};

// Re-export for multipart upload
use axum::extract::Multipart;

// ── Notebook CRUD ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateNotebookRequest {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub cover_image: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateNotebookRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub cover_image: Option<Option<String>>,
}

/// Serialized notebook for list responses (without inline sources/notes).
#[derive(Serialize)]
pub struct NotebookSummary {
    pub id: String,
    pub title: String,
    pub description: String,
    pub cover_image: Option<String>,
    pub source_count: usize,
    pub note_count: usize,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
    pub owner_id: String,
}

impl From<&Notebook> for NotebookSummary {
    fn from(nb: &Notebook) -> Self {
        Self {
            id: nb.id.clone(),
            title: nb.title.clone(),
            description: nb.description.clone(),
            cover_image: nb.cover_image.clone(),
            source_count: nb.source_count,
            note_count: nb.note_count,
            created_at: nb.created_at,
            updated_at: nb.updated_at,
            owner_id: nb.owner_id.clone(),
        }
    }
}

fn notebook_store(state: &AppState) -> Result<&Arc<NotebookStore>, (StatusCode, String)> {
    state.notebook_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "notebook store not configured".into(),
    ))
}

/// GET /api/notebooks
pub async fn list_notebooks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<NotebookSummary>>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let notebooks = store
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let summaries: Vec<NotebookSummary> = notebooks.iter().map(NotebookSummary::from).collect();
    Ok(Json(summaries))
}

/// POST /api/notebooks
pub async fn create_notebook(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateNotebookRequest>,
) -> Result<Json<Notebook>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let now = Utc::now();
    let nb = Notebook {
        id: uuid::Uuid::now_v7().to_string(),
        title: req.title,
        description: req.description,
        cover_image: req.cover_image,
        source_count: 0,
        note_count: 0,
        created_at: now,
        updated_at: now,
        owner_id: String::new(), // TODO: extract from auth identity
        sources: vec![],
        notes: vec![],
    };
    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(nb))
}

/// GET /api/notebooks/:id
pub async fn get_notebook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Notebook>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))
}

/// PUT /api/notebooks/:id
pub async fn update_notebook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateNotebookRequest>,
) -> Result<Json<Notebook>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    if let Some(title) = req.title {
        nb.title = title;
    }
    if let Some(desc) = req.description {
        nb.description = desc;
    }
    if let Some(cover) = req.cover_image {
        nb.cover_image = cover;
    }
    nb.updated_at = Utc::now();

    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(nb))
}

/// DELETE /api/notebooks/:id
pub async fn delete_notebook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let deleted = store
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !deleted {
        return Err((StatusCode::NOT_FOUND, format!("notebook {id} not found")));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Source CRUD ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AddSourceRequest {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub source_type: Option<SourceType>,
}

/// Split text into chunks of roughly `target_size` chars, breaking on paragraph boundaries.
fn split_into_chunks(text: &str, target_size: usize) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut offset = 0;

    // Split on double newlines (paragraphs) first
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut current = String::new();
    let mut current_start = 0;

    for para in paragraphs {
        if !current.is_empty() && current.len() + para.len() + 2 > target_size {
            // Flush current chunk
            let end = offset;
            chunks.push(Chunk {
                id: uuid::Uuid::now_v7().to_string(),
                content: current.clone(),
                start_offset: current_start,
                end_offset: end,
            });
            current.clear();
            current_start = offset;
        }
        if !current.is_empty() {
            current.push_str("\n\n");
            offset += 2;
        }
        current.push_str(para);
        offset += para.len();
    }

    // Flush remaining
    if !current.is_empty() {
        chunks.push(Chunk {
            id: uuid::Uuid::now_v7().to_string(),
            content: current,
            start_offset: current_start,
            end_offset: offset,
        });
    }

    // If any chunk is still too large, do a hard split
    let mut final_chunks = Vec::new();
    for chunk in chunks {
        if chunk.content.len() > target_size * 2 {
            let text = &chunk.content;
            let mut pos = 0;
            while pos < text.len() {
                let end = (pos + target_size).min(text.len());
                // Find a safe UTF-8 boundary
                let end = if end < text.len() {
                    let mut e = end;
                    while e > pos && !text.is_char_boundary(e) {
                        e -= 1;
                    }
                    e
                } else {
                    end
                };
                final_chunks.push(Chunk {
                    id: uuid::Uuid::now_v7().to_string(),
                    content: text[pos..end].to_string(),
                    start_offset: chunk.start_offset + pos,
                    end_offset: chunk.start_offset + end,
                });
                pos = end;
            }
        } else {
            final_chunks.push(chunk);
        }
    }

    final_chunks
}

/// GET /api/notebooks/:id/sources
pub async fn list_sources(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Source>>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;
    Ok(Json(nb.sources))
}

/// POST /api/notebooks/:id/sources
pub async fn add_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AddSourceRequest>,
) -> Result<Json<Source>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let (content, source_type, filename) = if let Some(url) = &req.url {
        (url.clone(), SourceType::Url, url.clone())
    } else if let Some(text) = &req.text {
        (
            text.clone(),
            req.source_type.clone().unwrap_or(SourceType::Text),
            req.filename.clone().unwrap_or_else(|| "text-input".into()),
        )
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            "provide either 'url' or 'text'".into(),
        ));
    };

    let chunks = split_into_chunks(&content, 800);

    let source = Source {
        id: uuid::Uuid::now_v7().to_string(),
        notebook_id: id.clone(),
        source_type,
        filename,
        status: SourceStatus::Ready,
        error_message: None,
        chunks,
        created_at: Utc::now(),
    };

    nb.sources.push(source.clone());
    nb.source_count = nb.sources.len();
    nb.updated_at = Utc::now();

    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(source))
}

/// GET /api/notebooks/:id/sources/:sid
pub async fn get_source(
    State(state): State<Arc<AppState>>,
    Path((id, sid)): Path<(String, String)>,
) -> Result<Json<Source>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;
    nb.sources
        .into_iter()
        .find(|s| s.id == sid)
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("source {sid} not found")))
}

/// DELETE /api/notebooks/:id/sources/:sid
pub async fn delete_source(
    State(state): State<Arc<AppState>>,
    Path((id, sid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let before = nb.sources.len();
    nb.sources.retain(|s| s.id != sid);
    if nb.sources.len() == before {
        return Err((StatusCode::NOT_FOUND, format!("source {sid} not found")));
    }
    nb.source_count = nb.sources.len();
    nb.updated_at = Utc::now();

    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/notebooks/:id/sources/upload — multipart file upload for sources.
pub async fn upload_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<Source>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let mut filename = String::from("upload");
    let mut content = String::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            if let Some(fname) = field.file_name() {
                filename = fname.to_string();
            }
            content = field
                .text()
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("failed to read file: {e}")))?;
        }
    }

    if content.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no file content provided".into()));
    }

    // Detect source type from filename extension
    let source_type = if filename.ends_with(".pdf") {
        SourceType::Pdf
    } else if filename.ends_with(".docx") {
        SourceType::Docx
    } else if filename.ends_with(".pptx") {
        SourceType::Pptx
    } else {
        SourceType::Text
    };

    let chunks = split_into_chunks(&content, 800);

    let source = Source {
        id: uuid::Uuid::now_v7().to_string(),
        notebook_id: id.clone(),
        source_type,
        filename,
        status: SourceStatus::Ready,
        error_message: None,
        chunks,
        created_at: Utc::now(),
    };

    nb.sources.push(source.clone());
    nb.source_count = nb.sources.len();
    nb.updated_at = Utc::now();

    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(source))
}

// ── Note CRUD ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateNoteRequest {
    pub content: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
    #[serde(default = "default_note_origin")]
    pub created_from: NoteOrigin,
}

fn default_note_origin() -> NoteOrigin {
    NoteOrigin::Manual
}

#[derive(Deserialize)]
pub struct UpdateNoteRequest {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub source_refs: Option<Vec<String>>,
}

/// GET /api/notebooks/:id/notes
pub async fn list_notes(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Note>>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;
    Ok(Json(nb.notes))
}

/// POST /api/notebooks/:id/notes
pub async fn create_note(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CreateNoteRequest>,
) -> Result<Json<Note>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let now = Utc::now();
    let note = Note {
        id: uuid::Uuid::now_v7().to_string(),
        notebook_id: id.clone(),
        content: req.content,
        source_refs: req.source_refs,
        created_from: req.created_from,
        created_at: now,
        updated_at: now,
    };

    nb.notes.push(note.clone());
    nb.note_count = nb.notes.len();
    nb.updated_at = now;

    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(note))
}

/// PUT /api/notebooks/:id/notes/:nid
pub async fn update_note(
    State(state): State<Arc<AppState>>,
    Path((id, nid)): Path<(String, String)>,
    Json(req): Json<UpdateNoteRequest>,
) -> Result<Json<Note>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let note = nb
        .notes
        .iter_mut()
        .find(|n| n.id == nid)
        .ok_or((StatusCode::NOT_FOUND, format!("note {nid} not found")))?;

    if let Some(content) = req.content {
        note.content = content;
    }
    if let Some(refs) = req.source_refs {
        note.source_refs = refs;
    }
    note.updated_at = Utc::now();
    let updated = note.clone();

    nb.updated_at = Utc::now();
    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(updated))
}

/// DELETE /api/notebooks/:id/notes/:nid
pub async fn delete_note(
    State(state): State<Arc<AppState>>,
    Path((id, nid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let before = nb.notes.len();
    nb.notes.retain(|n| n.id != nid);
    if nb.notes.len() == before {
        return Err((StatusCode::NOT_FOUND, format!("note {nid} not found")));
    }
    nb.note_count = nb.notes.len();
    nb.updated_at = Utc::now();

    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Notebook Chat (RAG + SSE) ────────────────────────────────────────

#[derive(Deserialize)]
pub struct NotebookChatRequest {
    pub message: String,
}

/// System prompt for RAG-based notebook chat with citation instructions.
const RAG_SYSTEM_PROMPT: &str = "\
You are a research assistant. Answer based ONLY on the provided sources.
Cite sources using [src:N] format where N is the source number.
If the sources don't contain relevant information, say so.";

/// Simple keyword-based relevance scoring for MVP.
fn score_chunk(chunk: &Chunk, query: &str) -> usize {
    let query_lower = query.to_lowercase();
    let content_lower = chunk.content.to_lowercase();
    query_lower
        .split_whitespace()
        .filter(|word| word.len() > 2 && content_lower.contains(word))
        .count()
}

/// POST /api/notebooks/:id/chat — RAG chat with SSE streaming.
pub async fn notebook_chat(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<NotebookChatRequest>,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, String),
> {
    let store = notebook_store(&state)?;
    let nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let agent = state.agent.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "No LLM provider configured".into(),
    ))?;

    // Collect all chunks with source labels
    let mut all_chunks: Vec<(usize, &Source, &Chunk)> = Vec::new();
    for (src_idx, source) in nb.sources.iter().enumerate() {
        for chunk in &source.chunks {
            all_chunks.push((src_idx + 1, source, chunk));
        }
    }

    // Score and rank chunks by keyword relevance
    let mut scored: Vec<_> = all_chunks
        .iter()
        .map(|(idx, src, chunk)| {
            let score = score_chunk(chunk, &req.message);
            (*idx, *src, *chunk, score)
        })
        .collect();
    scored.sort_by(|a, b| b.3.cmp(&a.3));

    // Take top chunks (max ~4000 chars of context)
    let mut context_text = String::new();
    let mut char_budget: usize = 4000;
    for (src_idx, _src, chunk, _score) in &scored {
        if char_budget == 0 {
            break;
        }
        let snippet = if chunk.content.len() > char_budget {
            &chunk.content[..char_budget]
        } else {
            &chunk.content
        };
        context_text.push_str(&format!("[Source {src_idx}]: {snippet}\n\n"));
        char_budget = char_budget.saturating_sub(chunk.content.len());
    }

    // Build messages
    let system_message = crew_core::Message::system(format!(
        "{RAG_SYSTEM_PROMPT}\n\n--- Sources ---\n{context_text}"
    ));
    let user_message = crew_core::Message::user(req.message);

    let llm = agent.llm_provider();
    let config = crew_llm::ChatConfig::default();

    // Stream from LLM
    let stream_result = llm
        .chat_stream(&[system_message, user_message], &[], &config)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let stream = stream_result.map(|event| {
        let json = match event {
            crew_llm::StreamEvent::TextDelta(text) => {
                serde_json::json!({ "type": "text", "content": text })
            }
            crew_llm::StreamEvent::Done(_reason) => {
                serde_json::json!({ "type": "done" })
            }
            crew_llm::StreamEvent::Error(msg) => {
                serde_json::json!({ "type": "error", "message": msg })
            }
            _ => serde_json::json!({ "type": "other" }),
        };
        Ok(Event::default().data(json.to_string()))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_split_text_into_chunks() {
        let text = "Hello world.\n\nThis is a test.\n\nAnother paragraph here.";
        let chunks = split_into_chunks(text, 30);
        assert!(chunks.len() >= 2);
        // All content should be covered
        let total: String = chunks.iter().map(|c| c.content.clone()).collect::<Vec<_>>().join("\n\n");
        assert_eq!(total, text);
    }

    #[test]
    fn should_handle_empty_text() {
        let chunks = split_into_chunks("", 800);
        assert!(chunks.is_empty());
    }

    #[test]
    fn should_score_chunks_by_keywords() {
        let chunk = Chunk {
            id: "1".into(),
            content: "Rust programming language is fast and safe".into(),
            start_offset: 0,
            end_offset: 43,
        };
        assert!(score_chunk(&chunk, "rust programming") > 0);
        assert_eq!(score_chunk(&chunk, "python javascript"), 0);
    }
}
