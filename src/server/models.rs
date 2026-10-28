use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;

use super::error::ApiError;
use super::AppState;
use crate::openai::{ModelList, ModelObject};

pub async fn list(State(state): State<Arc<AppState>>) -> Json<ModelList> {
    let created = state.catalog.created();
    let data = state
        .catalog
        .entries()
        .await
        .into_iter()
        .map(|e| ModelObject {
            id: e.id,
            object: "model",
            created,
            owned_by: e.owned_by.to_string(),
        })
        .collect();
    Json(ModelList {
        object: "list",
        data,
    })
}

pub async fn get_one(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ModelObject>, ApiError> {
    let created = state.catalog.created();
    if let Some(e) = state
        .catalog
        .entries()
        .await
        .into_iter()
        .find(|e| e.id == id)
    {
        return Ok(Json(ModelObject {
            id: e.id,
            object: "model",
            created,
            owned_by: e.owned_by.to_string(),
        }));
    }
    // Anything the router can resolve is also a valid model id.
    match crate::routing::resolve(&state.cfg, &id) {
        Ok(r) => Ok(Json(ModelObject {
            id,
            object: "model",
            created,
            owned_by: r.harness.owned_by().to_string(),
        })),
        Err(e) => Err(e.into()),
    }
}
