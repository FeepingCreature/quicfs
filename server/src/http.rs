use std::sync::Arc;
use anyhow::Result;
use bytes::{Bytes, Buf};
use quinn::{Endpoint, crypto::rustls::QuicServerConfig, VarInt};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use crate::fs::{FileSystem, MmapBody};
use tracing::{info, warn, error};
use http::HeaderMap;

pub struct HttpServer {
    endpoint: Endpoint,
    fs: Arc<FileSystem>,
}

impl HttpServer {
    pub fn new(cert_chain: Vec<CertificateDer<'static>>, private_key: PrivateKeyDer<'static>, fs: FileSystem, port: u16) -> Result<Self> {
        // Create QUIC server config with ALPN protocols for HTTP/3
        let mut server_crypto = quinn::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)?;
        server_crypto.alpn_protocols = vec![b"h3".to_vec()];

        // Add QUIC transport configuration for performance
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.max_concurrent_bidi_streams(100u32.into());
        transport_config.max_concurrent_uni_streams(100u32.into());
        transport_config.send_window(8 * 1024 * 1024); // 8MB
        transport_config.receive_window(VarInt::from_u32(8 * 1024 * 1024)); // 8MB
        transport_config.stream_receive_window(VarInt::from_u32(2 * 1024 * 1024)); // 2MB per stream

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));
        server_config.transport_config(Arc::new(transport_config));
        
        let bind_addr = format!("0.0.0.0:{}", port).parse()?;
        let endpoint = Endpoint::server(server_config, bind_addr)?;

        Ok(HttpServer {
            endpoint,
            fs: Arc::new(fs),
        })
    }

    pub async fn run(&self) -> Result<()> {
        info!("QUIC server listening on {}", self.endpoint.local_addr()?);

        loop {
            info!("Waiting for QUIC connection...");
            let connection = self.endpoint.accept().await;
            
            match connection {
                Some(conn) => {
                    info!("New QUIC connection incoming, awaiting handshake...");
                    let fs = self.fs.clone();
                    tokio::spawn(async move {
                        let result: Result<()> = async {
                            if let Ok(connection) = conn.await {
                                info!("QUIC connection established from {}", 
                                    connection.remote_address());
                                
                                let h3_conn = h3_quinn::Connection::new(connection);
                                let mut h3: h3::server::Connection<_, Bytes> = h3::server::Connection::new(h3_conn).await?;
                                
                                while let Ok(Some(req_resolver)) = h3.accept().await {
                                    let fs = fs.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = handle_request(req_resolver, fs).await {
                                            error!("Request handling error: {}", e);
                                        }
                                    });
                                }
                            }
                            Ok(())
                        }.await;
                        
                        if let Err(e) = result {
                            error!("Connection error: {}", e);
                        }
                    });
                }
                None => warn!("No QUIC connection available"),
            }
        }
    }
}

async fn handle_request(
    req_resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    fs: Arc<FileSystem>,
) -> Result<()> {
    let (req, stream) = req_resolver.resolve_request().await?;
    
    let path = req.uri().path();
    let method = req.method();
    
    // Only log non-file requests (avoid spammy GET and PATCH for files)
    if !((method == "GET" || method == "PATCH") && path.starts_with("/file/")) {
        info!("Received {} request for {}", method, path);
    }
    
    match (method.as_str(), path) {
        ("GET", path) if path.starts_with("/file/") => {
            handle_file_read(req, stream, fs).await
        }
        ("PATCH", path) if path.starts_with("/file/") => {
            handle_file_write(req, stream, fs).await
        }
        ("GET", path) if path.starts_with("/dir/") => {
            handle_dir_list(req, stream, fs).await
        }
        _ => {
            let mut stream = stream;
            let response = http::Response::builder()
                .status(404)
                .header("content-type", "application/json")
                .body(())?;
            stream.send_response(response).await?;
            let error_body = serde_json::json!({
                "error": "Not found",
                "path": path
            });
            stream.send_data(Bytes::from(error_body.to_string())).await?;
            stream.finish().await?;
            Ok(())
        }
    }
}

async fn handle_file_read(
    req: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    fs: Arc<FileSystem>,
) -> Result<()> {
    let path = req.uri().path().trim_start_matches("/file/");
    let decoded_path = urlencoding::decode(path).unwrap_or_else(|_| path.into());
    let file_path = format!("/file/{}", decoded_path);
    
    // Check for Range header
    if let Some(range_header) = req.headers().get("Range").and_then(|v| v.to_str().ok()) {
        if let Some(range) = range_header.strip_prefix("bytes=") {
            let parts: Vec<&str> = range.split('-').collect();
            if parts.len() == 2 {
                let start: u64 = parts[0].parse().unwrap_or(0);
                
                // Get file size
                let file_size = match fs.get_file_size(&file_path).await {
                    Ok(size) => size,
                    Err(err) => {
                        let response = http::Response::builder()
                            .status(500)
                            .header("content-type", "application/json")
                            .body(())?;
                        stream.send_response(response).await?;
                        let error_body = serde_json::json!({
                            "error": err.to_string()
                        });
                        stream.send_data(Bytes::from(error_body.to_string())).await?;
                        stream.finish().await?;
                        return Ok(());
                    }
                };
                
                let end: u64 = if parts[1].is_empty() {
                    file_size - 1
                } else {
                    parts[1].parse().unwrap_or(file_size - 1).min(file_size - 1)
                };
                
                if start <= end && start < file_size {
                    let length = end - start + 1;
                    
                    // Use zero-copy mmap body for range requests
                    let mmap_body = tokio::task::spawn_blocking({
                        let fs = fs.clone();
                        let file_path = file_path.clone();
                        move || fs.read_file_range_mmap_body(&file_path, start, length)
                    }).await;
                    
                    match mmap_body {
                        Ok(Ok(mmap_body)) => {
                            let response = http::Response::builder()
                                .status(206)
                                .header("Content-Range", format!("bytes {}-{}/{}", start, end, file_size))
                                .header("Accept-Ranges", "bytes")
                                .header("Content-Length", mmap_body.len().to_string())
                                .body(())?;
                            
                            stream.send_response(response).await?;
                            stream_mmap_body(stream, mmap_body).await?;
                            return Ok(());
                        }
                        Ok(Err(err)) => {
                            let response = http::Response::builder()
                                .status(500)
                                .header("content-type", "application/json")
                                .body(())?;
                            stream.send_response(response).await?;
                            let error_body = serde_json::json!({
                                "error": err.to_string()
                            });
                            stream.send_data(Bytes::from(error_body.to_string())).await?;
                            stream.finish().await?;
                            return Ok(());
                        }
                        Err(_) => {
                            let response = http::Response::builder()
                                .status(500)
                                .header("content-type", "application/json")
                                .body(())?;
                            stream.send_response(response).await?;
                            let error_body = serde_json::json!({
                                "error": "Task join error"
                            });
                            stream.send_data(Bytes::from(error_body.to_string())).await?;
                            stream.finish().await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    
    // Full file read - stream directly from mmap
    let mmap_body = tokio::task::spawn_blocking({
        let fs = fs.clone();
        let file_path = file_path.clone();
        move || fs.read_file_mmap_body(&file_path)
    }).await;
    
    match mmap_body {
        Ok(Ok(mmap_body)) => {
            let response = http::Response::builder()
                .status(200)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", mmap_body.len().to_string())
                .body(())?;
            
            stream.send_response(response).await?;
            stream_mmap_body(stream, mmap_body).await?;
        }
        Ok(Err(err)) => {
            let response = http::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(())?;
            stream.send_response(response).await?;
            let error_body = serde_json::json!({
                "error": err.to_string()
            });
            stream.send_data(Bytes::from(error_body.to_string())).await?;
            stream.finish().await?;
        }
        Err(_) => {
            let response = http::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(())?;
            stream.send_response(response).await?;
            let error_body = serde_json::json!({
                "error": "Task join error"
            });
            stream.send_data(Bytes::from(error_body.to_string())).await?;
            stream.finish().await?;
        }
    }
    
    Ok(())
}

async fn stream_mmap_body(
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    mmap_body: MmapBody,
) -> Result<()> {
    // Stream chunks directly without collecting
    const CHUNK_SIZE: usize = 64 * 1024; // 64KB chunks
    let mut position = mmap_body.start();
    let end = mmap_body.end();
    
    while position < end {
        let remaining = end - position;
        let chunk_size = std::cmp::min(remaining, CHUNK_SIZE);
        
        let chunk = mmap_body.get_chunk(position, chunk_size);
        stream.send_data(Bytes::copy_from_slice(chunk)).await?;
        
        position += chunk_size;
    }
    
    stream.finish().await?;
    Ok(())
}

async fn handle_file_write(
    req: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    fs: Arc<FileSystem>,
) -> Result<()> {
    let path = req.uri().path().trim_start_matches("/file/");
    let decoded_path = urlencoding::decode(path).unwrap_or_else(|_| path.into());
    
    // Parse and validate Content-Range header
    let (offset, expected_len) = match parse_content_range(req.headers()) {
        Ok(result) => result,
        Err(err) => {
            let response = http::Response::builder()
                .status(400)
                .header("content-type", "application/json")
                .body(())?;
            stream.send_response(response).await?;
            let error_body = serde_json::json!({
                "error": err
            });
            stream.send_data(Bytes::from(error_body.to_string())).await?;
            stream.finish().await?;
            return Ok(());
        }
    };
    
    // Handle truncate operation
    if expected_len.is_none() {
        // This is a truncate operation (Content-Range: bytes */size)
        if let Some(new_size) = offset {
            match fs.truncate_file(&format!("/file/{}", decoded_path), new_size).await {
                Ok(_) => {
                    let response = http::Response::builder()
                        .status(200)
                        .body(())?;
                    stream.send_response(response).await?;
                    stream.finish().await?;
                    return Ok(());
                }
                Err(err) => {
                    let response = http::Response::builder()
                        .status(500)
                        .header("content-type", "application/json")
                        .body(())?;
                    stream.send_response(response).await?;
                    let error_body = serde_json::json!({
                        "error": err.to_string()
                    });
                    stream.send_data(Bytes::from(error_body.to_string())).await?;
                    stream.finish().await?;
                    return Ok(());
                }
            }
        }
    }
    
    let write_offset = offset.unwrap_or(0);
    let expected_length = expected_len.unwrap_or(0);
    
    // Stream body directly to filesystem without intermediate buffer
    let mut total_received = 0u64;
    let mut write_buffer = Vec::new();
    const WRITE_CHUNK_SIZE: usize = 64 * 1024; // 64KB write chunks
    
    while let Ok(Some(chunk)) = stream.recv_data().await {
        write_buffer.extend_from_slice(chunk.chunk());
        total_received += chunk.chunk().len() as u64;
        
        // Write in chunks to avoid large memory usage
        while write_buffer.len() >= WRITE_CHUNK_SIZE {
            let chunk_to_write = write_buffer.drain(..WRITE_CHUNK_SIZE).collect::<Vec<u8>>();
            let current_offset = write_offset + (total_received - write_buffer.len() as u64 - chunk_to_write.len() as u64);
            
            if let Err(err) = fs.write_file(&format!("/file/{}", decoded_path), current_offset, &chunk_to_write).await {
                let response = http::Response::builder()
                    .status(500)
                    .header("content-type", "application/json")
                    .body(())?;
                stream.send_response(response).await?;
                let error_body = serde_json::json!({
                    "error": err.to_string()
                });
                stream.send_data(Bytes::from(error_body.to_string())).await?;
                stream.finish().await?;
                return Ok(());
            }
        }
    }
    
    // Write remaining data
    if !write_buffer.is_empty() {
        let current_offset = write_offset + (total_received - write_buffer.len() as u64);
        if let Err(err) = fs.write_file(&format!("/file/{}", decoded_path), current_offset, &write_buffer).await {
            let response = http::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(())?;
            stream.send_response(response).await?;
            let error_body = serde_json::json!({
                "error": err.to_string()
            });
            stream.send_data(Bytes::from(error_body.to_string())).await?;
            stream.finish().await?;
            return Ok(());
        }
    }
    
    // Validate received length
    if expected_length > 0 && total_received != expected_length {
        let response = http::Response::builder()
            .status(400)
            .header("content-type", "application/json")
            .body(())?;
        stream.send_response(response).await?;
        let error_body = serde_json::json!({
            "error": format!("Content length mismatch: expected {} bytes but got {}", expected_length, total_received)
        });
        stream.send_data(Bytes::from(error_body.to_string())).await?;
        stream.finish().await?;
        return Ok(());
    }
    
    let response = http::Response::builder()
        .status(200)
        .body(())?;
    stream.send_response(response).await?;
    stream.finish().await?;
    Ok(())
}

async fn handle_dir_list(
    req: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    fs: Arc<FileSystem>,
) -> Result<()> {
    let path = req.uri().path();
    let dir_path = if path == "/dir/" {
        "/dir/".to_string()
    } else {
        let clean_path = path.trim_start_matches("/dir/");
        let decoded = urlencoding::decode(clean_path).unwrap_or_else(|_| clean_path.into());
        format!("/dir/{}", decoded)
    };
    
    info!("GET {}", dir_path);
    
    match fs.list_directory(&dir_path).await {
        Ok(dir_list) => {
            info!("Directory listing successful with {} entries", dir_list.entries.len());
            let json_response = serde_json::to_string(&dir_list)?;
            
            let response = http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("content-length", json_response.len().to_string())
                .body(())?;
            
            stream.send_response(response).await?;
            stream.send_data(Bytes::from(json_response)).await?;
            stream.finish().await?;
        }
        Err(err) => {
            warn!("Failed to list directory: {}", err);
            let response = http::Response::builder()
                .status(500)
                .header("content-type", "application/json")
                .body(())?;
            stream.send_response(response).await?;
            let error_body = serde_json::json!({
                "error": err.to_string(),
                "details": format!("{:?}", err)
            });
            stream.send_data(Bytes::from(error_body.to_string())).await?;
            stream.finish().await?;
        }
    }
    
    Ok(())
}

fn parse_content_range(headers: &HeaderMap) -> Result<(Option<u64>, Option<u64>), String> {
    match headers.get("Content-Range").and_then(|v| v.to_str().ok()) {
        Some(range) => {
            if let Some(range) = range.strip_prefix("bytes ") {
                let parts: Vec<&str> = range.split('/').collect();
                if parts.len() != 2 {
                    return Err("Invalid Content-Range format".to_string());
                }

                // Handle special case for truncate: "bytes */size"
                if parts[0] == "*" {
                    let new_size: u64 = parts[1].parse()
                        .map_err(|_| "Invalid Content-Range size for truncate".to_string())?;
                    return Ok((Some(new_size), None)); // Signal truncate operation
                }
                
                let range_parts: Vec<&str> = parts[0].split('-').collect();
                if range_parts.len() != 2 {
                    return Err("Invalid Content-Range format".to_string());
                }

                let start: u64 = range_parts[0].parse()
                    .map_err(|_| "Invalid Content-Range start offset".to_string())?;
                
                let end: u64 = range_parts[1].parse()
                    .map_err(|_| "Invalid Content-Range end offset".to_string())?;
                
                let total: u64 = parts[1].parse()
                    .map_err(|_| "Invalid Content-Range total length".to_string())?;

                let expected_len = if total == 0 {
                    0  // Special case for empty file creation
                } else if end >= start {
                    end - start + 1
                } else {
                    0
                };

                Ok((Some(start), Some(expected_len)))
            } else {
                Err("Invalid Content-Range format".to_string())
            }
        }
        None => Ok((Some(0), None)), // Default to offset 0, no expected length
    }
}
