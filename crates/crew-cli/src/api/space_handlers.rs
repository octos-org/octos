//! Space (class/course) API handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use serde::Deserialize;

use super::AppState;
use crate::space::{Space, SpaceStore};

fn space_store(state: &AppState) -> Result<&Arc<SpaceStore>, (StatusCode, String)> {
    state.space_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "space store not configured".into(),
    ))
}

#[derive(Deserialize)]
pub struct CreateSpaceRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
pub struct UpdateSpaceRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct AddMemberRequest {
    pub user_id: String,
}

#[derive(Deserialize)]
pub struct LinkNotebookRequest {
    pub notebook_id: String,
}

/// GET /api/spaces
pub async fn list_spaces(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Space>>, (StatusCode, String)> {
    let store = space_store(&state)?;
    let spaces = store
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(spaces))
}

/// POST /api/spaces
pub async fn create_space(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSpaceRequest>,
) -> Result<Json<Space>, (StatusCode, String)> {
    let store = space_store(&state)?;
    let space = Space {
        id: uuid::Uuid::now_v7().to_string(),
        name: req.name,
        description: req.description,
        owner_id: String::new(), // TODO: extract from auth
        member_ids: vec![],
        notebook_ids: vec![],
        created_at: Utc::now(),
    };
    store
        .save(&space)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(space))
}

/// GET /api/spaces/:id
pub async fn get_space(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Space>, (StatusCode, String)> {
    let store = space_store(&state)?;
    store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("space {id} not found")))
}

/// PUT /api/spaces/:id
pub async fn update_space(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateSpaceRequest>,
) -> Result<Json<Space>, (StatusCode, String)> {
    let store = space_store(&state)?;
    let mut space = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("space {id} not found")))?;

    if let Some(name) = req.name {
        space.name = name;
    }
    if let Some(desc) = req.description {
        space.description = desc;
    }
    store
        .save(&space)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(space))
}

/// DELETE /api/spaces/:id
pub async fn delete_space(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = space_store(&state)?;
    let deleted = store
        .delete(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !deleted {
        return Err((StatusCode::NOT_FOUND, format!("space {id} not found")));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/spaces/:id/members
pub async fn add_member(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AddMemberRequest>,
) -> Result<Json<Space>, (StatusCode, String)> {
    let store = space_store(&state)?;
    let mut space = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("space {id} not found")))?;

    if !space.member_ids.contains(&req.user_id) {
        space.member_ids.push(req.user_id);
    }
    store
        .save(&space)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(space))
}

/// DELETE /api/spaces/:id/members/:uid
pub async fn remove_member(
    State(state): State<Arc<AppState>>,
    Path((id, uid)): Path<(String, String)>,
) -> Result<Json<Space>, (StatusCode, String)> {
    let store = space_store(&state)?;
    let mut space = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("space {id} not found")))?;

    space.member_ids.retain(|m| m != &uid);
    store
        .save(&space)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(space))
}

/// POST /api/spaces/:id/notebooks
pub async fn link_notebook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<LinkNotebookRequest>,
) -> Result<Json<Space>, (StatusCode, String)> {
    let store = space_store(&state)?;
    let mut space = store
        .get(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("space {id} not found")))?;

    if !space.notebook_ids.contains(&req.notebook_id) {
        space.notebook_ids.push(req.notebook_id);
    }
    store
        .save(&space)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(space))
}
