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
    path: Option<Path<String>>,
) -> impl IntoResponse {
    let dir_path = match path {
        Some(Path(p)) => format!("/dir/{}", p),
        None => "/dir/".to_string(),
    };
    println!("Handling directory listing request for path: {}", dir_path);
    println!("Attempting to list directory at path: {:?}", dir_path);
    match fs.list_directory(&dir_path).await {
        Ok(dir_list) => {
            println!("Directory listing successful, found {} entries", dir_list.entries.len());
            let response = (StatusCode::OK, Json(dir_list)).into_response();
            println!("Sending directory listing response: {:?}", response);
            response
        },
        Err(err) => {
            println!("Error listing directory: {}", err);
            println!("Error details: {:?}", err);
            let error_response = (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": err.to_string(),
                    "details": format!("{:?}", err)
                }))
            ).into_response();
            println!("Sending error response: {:?}", error_response);
            error_response
        },
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
    headers: http::HeaderMap,
    bytes: Bytes,
) -> impl IntoResponse {
    println!("Write request received for path: /file/{}", path);
    println!("Received {} bytes of data", bytes.len());
    println!("Headers: {:?}", headers);
    
    // Parse Content-Range header if present
    let offset = headers
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            if let Some(range) = v.strip_prefix("bytes ") {
                range.split('-').next().and_then(|s| s.parse::<u64>().ok())
            } else {
                None
            }
        })
        .unwrap_or(0);

    println!("Writing at offset: {}", offset);
    match fs.write_file(&format!("/file/{}", path), offset, &bytes).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": err.to_string()
            }))
        ).into_response(),
    }
}
