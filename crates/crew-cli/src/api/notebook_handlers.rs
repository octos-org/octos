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
    BookMeta, Chunk, Note, NoteOrigin, Notebook, NotebookStore, Share, ShareRole, Source,
    SourceStatus, SourceType,
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
    pub copyright_protected: bool,
    pub book_meta: Option<BookMeta>,
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
            copyright_protected: nb.copyright_protected,
            book_meta: nb.book_meta.clone(),
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
        shared_with: vec![],
        book_meta: None,
        copyright_protected: false,
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

/// Fetch URL content and strip HTML tags to plain text.
async fn fetch_url_content(url: &str) -> Result<String, String> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read body: {e}"))?;
    // Strip HTML tags with a simple regex
    let tag_re = regex::Regex::new(r"<[^>]+>").unwrap();
    let text = tag_re.replace_all(&body, " ");
    // Collapse whitespace
    let ws_re = regex::Regex::new(r"\s+").unwrap();
    let clean = ws_re.replace_all(&text, " ").trim().to_string();
    Ok(clean)
}

/// Extract text from a PDF file using `pdftotext` (poppler).
/// Falls back to storing the filename as a single chunk if pdftotext is unavailable.
fn extract_pdf_text(pdf_bytes: &[u8], filename: &str) -> String {
    // Write bytes to a temp file, shell out to pdftotext
    let tmp = match tempfile::NamedTempFile::new() {
        Ok(t) => t,
        Err(_) => return filename.to_string(),
    };
    if std::io::Write::write_all(&mut std::io::BufWriter::new(tmp.as_file()), pdf_bytes).is_err() {
        return filename.to_string();
    }
    let output = std::process::Command::new("pdftotext")
        .arg(tmp.path())
        .arg("-")
        .output();
    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).into_owned()
        }
        _ => {
            tracing::info!("pdftotext not available, using filename as fallback");
            filename.to_string()
        }
    }
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
        // Fetch URL content and strip HTML tags
        let fetched = fetch_url_content(url).await.unwrap_or_else(|e| {
            tracing::warn!(url = %url, error = %e, "failed to fetch URL, storing URL as content");
            url.clone()
        });
        (fetched, SourceType::Url, url.clone())
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
    let mut raw_bytes: Vec<u8> = Vec::new();

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
            raw_bytes = field
                .bytes()
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("failed to read file: {e}")))?
                .to_vec();
        }
    }

    if raw_bytes.is_empty() {
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

    // Extract text content based on source type
    let content = if source_type == SourceType::Pdf {
        extract_pdf_text(&raw_bytes, &filename)
    } else {
        String::from_utf8_lossy(&raw_bytes).into_owned()
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

// ── Sharing API ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ShareRequest {
    pub email: String,
    pub role: ShareRole,
}

/// POST /api/notebooks/:id/share
pub async fn share_notebook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ShareRequest>,
) -> Result<Json<Share>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let share = Share {
        id: uuid::Uuid::now_v7().to_string(),
        email: req.email,
        role: req.role,
        created_at: Utc::now(),
    };
    nb.shared_with.push(share.clone());
    nb.updated_at = Utc::now();
    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(share))
}

/// GET /api/notebooks/:id/share
pub async fn list_shares(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Share>>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;
    Ok(Json(nb.shared_with))
}

/// DELETE /api/notebooks/:id/share/:share_id
pub async fn revoke_share(
    State(state): State<Arc<AppState>>,
    Path((id, share_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let mut nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let before = nb.shared_with.len();
    nb.shared_with.retain(|s| s.id != share_id);
    if nb.shared_with.len() == before {
        return Err((StatusCode::NOT_FOUND, format!("share {share_id} not found")));
    }
    nb.updated_at = Utc::now();
    store
        .save(&nb)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── Copyright-aware source content ──────────────────────────────────

/// GET /api/notebooks/:id/sources/:sid/content — returns chunk content,
/// but redacts raw text when the notebook is copyright-protected.
pub async fn get_source_content(
    State(state): State<Arc<AppState>>,
    Path((id, sid)): Path<(String, String)>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let store = notebook_store(&state)?;
    let nb = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("notebook {id} not found")))?;

    let source = nb
        .sources
        .iter()
        .find(|s| s.id == sid)
        .ok_or((StatusCode::NOT_FOUND, format!("source {sid} not found")))?;

    let chunks: Vec<serde_json::Value> = source
        .chunks
        .iter()
        .map(|c| {
            if nb.copyright_protected {
                // Only return a summary (first 100 chars + ellipsis)
                let summary = if c.content.len() > 100 {
                    format!("{}...", &c.content[..100])
                } else {
                    c.content.clone()
                };
                serde_json::json!({
                    "id": c.id,
                    "summary": summary,
                    "start_offset": c.start_offset,
                    "end_offset": c.end_offset,
                })
            } else {
                serde_json::json!({
                    "id": c.id,
                    "content": c.content,
                    "start_offset": c.start_offset,
                    "end_offset": c.end_offset,
                })
            }
        })
        .collect();
    Ok(Json(chunks))
}

// ── Library: Batch import ────────────────────────────────────────────

/// POST /api/library/import — multipart CSV with columns: title,author,isbn,classification,file_path
pub async fn library_import(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Json<Vec<NotebookSummary>>, (StatusCode, String)> {
    let store = notebook_store(&state)?;

    let mut csv_content = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            csv_content = field
                .text()
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("failed to read CSV: {e}")))?;
        }
    }

    if csv_content.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no CSV content provided".into()));
    }

    let mut created = Vec::new();
    let now = Utc::now();

    for (i, line) in csv_content.lines().enumerate() {
        // Skip header
        if i == 0 {
            let lower = line.to_lowercase();
            if lower.contains("title") && lower.contains("author") {
                continue;
            }
        }
        let cols: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if cols.len() < 4 {
            continue;
        }
        let title = cols[0].to_string();
        let author = cols.get(1).map(|s| s.to_string());
        let isbn = cols.get(2).map(|s| s.to_string());
        let classification = cols.get(3).map(|s| s.to_string());
        let file_path = cols.get(4).map(|s| s.to_string());

        let book_meta = BookMeta {
            isbn: isbn.clone().filter(|s| !s.is_empty()),
            author: author.filter(|s| !s.is_empty()),
            classification: classification.filter(|s| !s.is_empty()),
            ..Default::default()
        };

        let mut nb = Notebook {
            id: uuid::Uuid::now_v7().to_string(),
            title,
            description: String::new(),
            cover_image: None,
            source_count: 0,
            note_count: 0,
            created_at: now,
            updated_at: now,
            owner_id: String::new(),
            sources: vec![],
            notes: vec![],
            shared_with: vec![],
            book_meta: Some(book_meta),
            copyright_protected: false,
        };

        // If file_path provided and it's a PDF, try to add as source
        if let Some(fp) = file_path.filter(|s| !s.is_empty()) {
            let content = if fp.ends_with(".pdf") {
                match std::fs::read(&fp) {
                    Ok(bytes) => extract_pdf_text(&bytes, &fp),
                    Err(_) => fp.clone(),
                }
            } else {
                std::fs::read_to_string(&fp).unwrap_or_else(|_| fp.clone())
            };
            let chunks = split_into_chunks(&content, 800);
            let source = Source {
                id: uuid::Uuid::now_v7().to_string(),
                notebook_id: nb.id.clone(),
                source_type: if fp.ends_with(".pdf") {
                    SourceType::Pdf
                } else {
                    SourceType::Text
                },
                filename: fp,
                status: SourceStatus::Ready,
                error_message: None,
                chunks,
                created_at: now,
            };
            nb.sources.push(source);
            nb.source_count = nb.sources.len();
        }

        store
            .save(&nb)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        created.push(NotebookSummary::from(&nb));
    }

    Ok(Json(created))
}

// ── Library: ISBN lookup ─────────────────────────────────────────────

/// GET /api/library/isbn/:isbn — lookup book info from Open Library.
pub async fn isbn_lookup(
    Path(isbn): Path<String>,
) -> Result<Json<BookMeta>, (StatusCode, String)> {
    let url = format!(
        "https://openlibrary.org/api/books?bibkeys=ISBN:{isbn}&format=json&jscmd=data"
    );
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Open Library request failed: {e}")))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("failed to parse response: {e}")))?;

    let key = format!("ISBN:{isbn}");
    let data = body
        .get(&key)
        .ok_or((StatusCode::NOT_FOUND, format!("ISBN {isbn} not found")))?;

    let meta = BookMeta {
        isbn: Some(isbn),
        author: data
            .get("authors")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from),
        publisher: data
            .get("publishers")
            .and_then(|p| p.as_array())
            .and_then(|p| p.first())
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from),
        publish_year: data
            .get("publish_date")
            .and_then(|d| d.as_str())
            .and_then(|d| {
                // Try to extract a 4-digit year
                d.chars()
                    .collect::<String>()
                    .split_whitespace()
                    .find_map(|w| w.parse::<u16>().ok().filter(|y| *y > 1000))
            }),
        subject: data
            .get("subjects")
            .and_then(|s| s.as_array())
            .and_then(|s| s.first())
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .map(String::from),
        cover_url: data
            .get("cover")
            .and_then(|c| c.get("large"))
            .and_then(|u| u.as_str())
            .map(String::from),
        ..Default::default()
    };
    Ok(Json(meta))
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
