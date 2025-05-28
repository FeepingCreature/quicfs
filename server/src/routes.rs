use std::sync::Arc;
use tracing::{info, warn, debug};
use axum::{
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use http::HeaderMap;
use http_body_util::BodyExt;
use crate::fs::FileSystem;

pub async fn list_directory(
    State(fs): State<Arc<FileSystem>>,
    path: Option<Path<String>>,
) -> impl IntoResponse {
    let dir_path = match path {
        Some(Path(p)) => {
            let decoded = urlencoding::decode(&p).unwrap_or_else(|_| p.clone().into());
            format!("/dir/{}", decoded)
        },
        None => "/dir/".to_string(),
    };
    info!("GET /dir/{}", dir_path);
    match fs.list_directory(&dir_path).await {
        Ok(dir_list) => {
            info!("Directory listing successful with {} entries", dir_list.entries.len());
            let json_response = serde_json::to_string(&dir_list).unwrap();
            debug!("Sending JSON response: {}", json_response);
            (StatusCode::OK, Json(dir_list)).into_response()
        },
        Err(err) => {
            warn!("Failed to list directory: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": err.to_string(),
                    "details": format!("{:?}", err)
                }))
            ).into_response()
        },
    }
}

pub async fn read_file(
    State(fs): State<Arc<FileSystem>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let decoded_path = urlencoding::decode(&path).unwrap_or_else(|_| path.clone().into());
    let file_path = format!("/file/{}", decoded_path);
    
    // Check for Range header first
    if let Some(range_header) = headers.get("Range").and_then(|v| v.to_str().ok()) {
        if let Some(range) = range_header.strip_prefix("bytes=") {
            let parts: Vec<&str> = range.split('-').collect();
            if parts.len() == 2 {
                let start: u64 = parts[0].parse().unwrap_or(0);
                
                // Get file size without reading the entire file
                let file_size = match fs.get_file_size(&file_path).await {
                    Ok(size) => size,
                    Err(err) => return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": err.to_string()
                        }))
                    ).into_response(),
                };
                
                let end: u64 = if parts[1].is_empty() {
                    file_size - 1
                } else {
                    parts[1].parse().unwrap_or(file_size - 1).min(file_size - 1)
                };
                
                if start <= end && start < file_size {
                    let length = end - start + 1;
                    // Use zero-copy mmap body for range requests
                    let range_body = tokio::task::spawn_blocking({
                        let fs = fs.clone();
                        let file_path = file_path.clone();
                        move || fs.read_file_range_mmap_body(&file_path, start, length)
                    }).await;
                    
                    match range_body {
                        Ok(Ok(mmap_body)) => {
                            return (
                                StatusCode::PARTIAL_CONTENT,
                                [
                                    ("Content-Range", format!("bytes {}-{}/{}", start, end, file_size)),
                                    ("Accept-Ranges", "bytes".to_string()),
                                    ("Content-Length", mmap_body.len().to_string()),
                                ],
                                Body::new(mmap_body)
                            ).into_response();
                        }
                        Ok(Err(err)) => return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({
                                "error": err.to_string()
                            }))
                        ).into_response(),
                        Err(_) => return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({
                                "error": "Task join error"
                            }))
                        ).into_response(),
                    }
                }
            }
        }
    }
    
    // Use zero-copy mmap body for full file requests
    let file_body = tokio::task::spawn_blocking({
        let fs = fs.clone();
        let file_path = file_path.clone();
        move || fs.read_file_mmap_body(&file_path)
    }).await;
    
    match file_body {
        Ok(Ok(mmap_body)) => {
            (
                StatusCode::OK,
                [
                    ("Accept-Ranges", "bytes".to_string()),
                    ("Content-Length", mmap_body.len().to_string()),
                ],
                Body::new(mmap_body)
            ).into_response()
        }
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": err.to_string()
            }))
        ).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "Task join error"
            }))
        ).into_response(),
    }
}

pub async fn write_file(
    State(fs): State<Arc<FileSystem>>,
    Path(path): Path<String>,
    headers: http::HeaderMap,
    body: axum::body::Body,
) -> impl IntoResponse {
    let decoded_path = urlencoding::decode(&path).unwrap_or_else(|_| path.clone().into());
    // info!("PATCH /file/{} with body", decoded_path);
    
    // Collect the body bytes efficiently
    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "error": format!("Failed to read request body: {}", e)
            }))).into_response();
        }
    };
    
    // Parse and validate Content-Range header
    let (offset, _expected_len) = match headers.get("Content-Range").and_then(|v| v.to_str().ok()) {
        Some(range) => {
            // info!("Content-Range: {}", range);
            if let Some(range) = range.strip_prefix("bytes ") {
                let parts: Vec<&str> = range.split('/').collect();
                if parts.len() != 2 {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                        "error": "Invalid Content-Range format"
                    }))).into_response();
                }

                // Handle special case for truncate: "bytes */size"
                if parts[0] == "*" {
                    let new_size: u64 = match parts[1].parse() {
                        Ok(v) => v,
                        Err(_) => {
                            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                                "error": "Invalid Content-Range size for truncate"
                            }))).into_response();
                        }
                    };
                    // For truncate operations, we expect no content
                    if !bytes.is_empty() {
                        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                            "error": "Truncate operation should not include content"
                        }))).into_response();
                    }
                    return match fs.truncate_file(&format!("/file/{}", decoded_path), new_size).await {
                        Ok(_) => StatusCode::OK.into_response(),
                        Err(err) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({
                                "error": err.to_string()
                            }))
                        ).into_response(),
                    };
                }
                
                let range_parts: Vec<&str> = parts[0].split('-').collect();
                if range_parts.len() != 2 {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                        "error": "Invalid Content-Range format"
                    }))).into_response();
                }

                let start: u64 = match range_parts[0].parse() {
                    Ok(v) => v,
                    Err(_) => {
                        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                            "error": "Invalid Content-Range start offset"
                        }))).into_response();
                    }
                };
                
                let end: u64 = match range_parts[1].parse() {
                    Ok(v) => v,
                    Err(_) => {
                        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                            "error": "Invalid Content-Range end offset"
                        }))).into_response();
                    }
                };
                
                let total: u64 = match parts[1].parse() {
                    Ok(v) => v,
                    Err(_) => {
                        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                            "error": "Invalid Content-Range total length"
                        }))).into_response();
                    }
                };

                // For a range like "0-0/0", we want length 1 only if total > 0
                let expected_len = if total == 0 {
                    0  // Special case for empty file creation
                } else if end >= start {
                    end - start + 1
                } else {
                    0
                };
                // info!("Writing {} bytes at offset {}", expected_len, start);
                if bytes.len() as u64 != expected_len {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                        "error": format!("Content length mismatch: expected {} bytes but got {} (range: {}-{}/{})", 
                                       expected_len, bytes.len(), start, end, total)
                    }))).into_response();
                }

                (start, expected_len)
            } else {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                    "error": "Invalid Content-Range format"
                }))).into_response();
            }
        }
        None => (0, bytes.len() as u64),
    };

    // info!("Writing {} bytes at offset: {}", expected_len, offset);
    match fs.write_file(&format!("/file/{}", decoded_path), offset, &bytes).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": err.to_string()
            }))
        ).into_response(),
    }
}
