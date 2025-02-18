use std::sync::Arc;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use bytes::Bytes;
use crate::fs::FileSystem;

pub async fn list_directory(
    State(fs): State<Arc<FileSystem>>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    match fs.list_directory(&format!("/dir/{}", path)).await {
        Ok(dir_list) => (StatusCode::OK, Json(dir_list)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": err.to_string()
            }))
        ).into_response(),
    }
}

pub async fn read_file(
    State(fs): State<Arc<FileSystem>>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    match fs.read_file(&format!("/file/{}", path)).await {
        Ok(data) => (StatusCode::OK, Bytes::from(data)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": err.to_string()
            }))
        ).into_response(),
    }
}

pub async fn write_file(
    State(fs): State<Arc<FileSystem>>,
    Path(path): Path<String>,
    bytes: Bytes,
) -> impl IntoResponse {
    match fs.write_file(&format!("/file/{}", path), &bytes).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": err.to_string()
            }))
        ).into_response(),
    }
}
