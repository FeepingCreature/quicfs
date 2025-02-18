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
    
    // Parse and validate Content-Range header
    let (offset, expected_len) = match headers.get("Content-Range").and_then(|v| v.to_str().ok()) {
        Some(range) => {
            println!("Parsing Content-Range header: {}", range);
            if let Some(range) = range.strip_prefix("bytes ") {
                let parts: Vec<&str> = range.split('/').collect();
                println!("Split parts: {:?}", parts);
                if parts.len() != 2 {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                        "error": "Invalid Content-Range format"
                    }))).into_response();
                }
                
                let range_parts: Vec<&str> = parts[0].split('-').collect();
                println!("Range parts: {:?}", range_parts);
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
                println!("Calculated expected_len={} from start={}, end={}", expected_len, start, end);
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

    println!("Writing {} bytes at offset: {}", expected_len, offset);
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
