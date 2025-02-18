use std::sync::Arc;
use anyhow::Result;
use quinn::{Endpoint, crypto::rustls::QuicServerConfig};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use crate::fs::FileSystem;
use serde_json;

pub struct HttpServer {
    endpoint: Endpoint,
    fs: Arc<FileSystem>,
}

impl HttpServer {
    pub fn new(cert_chain: Vec<CertificateDer<'static>>, private_key: PrivateKeyDer<'static>, fs: FileSystem) -> Result<Self> {
        // Create QUIC server config with ALPN protocols for HTTP/3
        let mut server_crypto = quinn::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)?;
        server_crypto.alpn_protocols = vec![b"h3".to_vec()];

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));
        let endpoint = Endpoint::server(server_config, "0.0.0.0:4433".parse()?)?;

        Ok(HttpServer {
            endpoint,
            fs: Arc::new(fs),
        })
    }

    pub async fn run(&self) -> Result<()> {
        println!("QUIC server listening on {}", self.endpoint.local_addr()?);

        loop {
            println!("Waiting for QUIC connection...");
            let connection = self.endpoint.accept().await;
            let fs = self.fs.clone();
            
            match connection {
                Some(conn) => {
                    println!("New QUIC connection incoming, awaiting handshake...");
                    tokio::spawn(async move {
                        if let Ok(connection) = conn.await {
                            println!("QUIC connection established from {}", 
                                connection.remote_address());
                            
                            if let Err(e) = handle_connection(connection, fs).await {
                                eprintln!("Connection error: {}", e);
                            }
                        }
                    });
                }
                None => println!("No QUIC connection available"),
            }
        }
    }
}

async fn handle_connection(connection: quinn::Connection, fs: Arc<FileSystem>) -> Result<()> {
    println!("Starting HTTP/3 connection handling");
    
    let h3_conn = h3_quinn::Connection::new(connection);
    let mut h3 = h3::server::Connection::new(h3_conn).await?;
    
    loop {
        match h3.accept().await {
            Ok(Some((req, mut stream))) => {
                println!("Received request: {:?}", req);
                println!("Processing request path: {}", req.uri().path());
                
                // Handle directory listing
                println!("Full request URI: {}", req.uri());
                println!("Request headers: {:?}", req.headers());
                
                if req.uri().path().starts_with("/dir") {
                    println!("Processing directory listing request");
                    match fs.list_directory(req.uri().path()).await {
                        Ok(dir_list) => {
                            let json = serde_json::to_string(&dir_list)?;
                            let response = http::Response::builder()
                                .status(200)
                                .header("content-type", "application/json")
                                .body(())?;
                            
                            stream.send_response(response).await?;
                            stream.send_data(bytes::Bytes::from(json)).await?;
                        },
                        Err(e) => {
                            let response = http::Response::builder()
                                .status(500)
                                .header("content-type", "application/json")
                                .body(())?;
                            
                            let error_json = serde_json::json!({
                                "error": e.to_string()
                            }).to_string();
                            
                            stream.send_response(response).await?;
                            stream.send_data(bytes::Bytes::from(error_json)).await?;
                        }
                    }
                } else if req.uri().path().starts_with("/file") {
                    // Handle file operations
                    if req.method() == "PATCH" {
                        // Handle write request
                        let mut body = Vec::new();
                        while let Some(chunk) = stream.recv_data().await? {
                            body.extend_from_slice(chunk.chunk());
                        }
                        
                        // Parse Content-Range header
                        let range = req.headers().get("Content-Range")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| {
                                let parts: Vec<&str> = s.split(' ').collect();
                                if parts.len() != 2 { return None; }
                                let range_parts: Vec<&str> = parts[1].split('-').collect();
                                if range_parts.len() != 2 { return None; }
                                Some((
                                    range_parts[0].parse::<u64>().ok()?,
                                    range_parts[1].split('/').next()?.parse::<u64>().ok()?
                                ))
                            })
                            .ok_or_else(|| anyhow::anyhow!("Invalid Content-Range header"))?;

                        match fs.write_file(req.uri().path(), range.0, &body).await {
                            Ok(_) => {
                                let response = http::Response::builder()
                                    .status(200)
                                    .body(())?;
                                stream.send_response(response).await?;
                            },
                            Err(e) => {
                                let response = http::Response::builder()
                                    .status(500)
                                    .header("content-type", "application/json")
                                    .body(())?;
                                
                                let error_json = serde_json::json!({
                                    "error": e.to_string()
                                }).to_string();
                                
                                stream.send_response(response).await?;
                                stream.send_data(bytes::Bytes::from(error_json)).await?;
                            }
                        }
                    } else {
                        // Handle read request
                        match fs.read_file(req.uri().path()).await {
                            Ok(data) => {
                                let response = http::Response::builder()
                                    .status(200)
                                    .header("content-type", "application/octet-stream")
                                    .body(())?;
                                
                                stream.send_response(response).await?;
                                stream.send_data(bytes::Bytes::from(data)).await?;
                            },
                            Err(e) => {
                                let response = http::Response::builder()
                                    .status(500)
                                    .header("content-type", "application/json")
                                    .body(())?;
                                
                                let error_json = serde_json::json!({
                                    "error": e.to_string()
                                }).to_string();
                                
                                stream.send_response(response).await?;
                                stream.send_data(bytes::Bytes::from(error_json)).await?;
                            }
                        }
                    }
                } else {
                    // Default response for other paths
                    let response = http::Response::builder()
                        .status(404)
                        .header("content-type", "text/plain")
                        .body(())?;
                    
                    stream.send_response(response).await?;
                    stream.send_data(bytes::Bytes::from("Not Found")).await?;
                }
                stream.finish().await?;
            }
            Ok(None) => {
                println!("Connection closed");
                break;
            }
            Err(e) => {
                eprintln!("Error accepting request: {}", e);
                break;
            }
        }
    }
    Ok(())
}
