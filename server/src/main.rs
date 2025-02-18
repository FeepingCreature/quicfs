use anyhow::Result;
use axum::{
    routing::get,
    Router,
    http::HeaderValue,
    response::Response,
    middleware::{self, Next},
};
use axum::body::Body;
use axum::http::Request;
use quinn::{Endpoint, crypto::rustls::QuicServerConfig};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tower_http::cors::CorsLayer;

#[tokio::main]
async fn main() -> Result<()> {
    // Generate TLS certificate
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert_pem = cert.serialize_pem()?;
    let key_pem = cert.serialize_private_key_pem();
    
    // For QUIC server (needs DER format)
    let cert_der = cert.serialize_der()?;
    let priv_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der()));
    let cert_chain = vec![CertificateDer::from(cert_der)];

    // Create QUIC server config with ALPN protocols for HTTP/3
    let mut server_crypto = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain.clone(), priv_key)?;
    server_crypto.alpn_protocols = vec![b"h3".to_vec()];

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));
    let endpoint = Endpoint::server(server_config, "0.0.0.0:4433".parse()?)?;
    
    println!("QUIC server config created");
    
    match endpoint.local_addr() {
        Ok(local_addr) => println!("QUIC server bound to {}", local_addr),
        Err(e) => eprintln!("Failed to get QUIC local address: {}", e),
    }

    // Create HTTPS server
    let https_addr = "0.0.0.0:8443".parse::<SocketAddr>()?;
    let app = Router::new()
        .route("/files/*path", get(handle_get).put(handle_put))
        .layer(middleware::from_fn(add_alt_svc_header));

    println!("HTTPS server listening on {}", https_addr);

    // Run both servers
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
        cert_pem.as_bytes().to_vec(),
        key_pem.as_bytes().to_vec(),
    )
    .await
    .expect("Failed to create HTTPS config");

    tokio::spawn(async move {
        axum_server::bind_rustls(https_addr, rustls_config)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    // Serve directory
    let serve_dir = PathBuf::from("served_files");
    if !serve_dir.exists() {
        fs::create_dir(&serve_dir).await?;
        println!("Created served_files directory");
    }

    println!("Starting QUIC connection acceptance loop");
    loop {
        println!("Waiting for QUIC connection...");
        let connection = endpoint.accept().await;
        println!("QUIC connection attempt received: {:?}", connection.is_some());
        handle_connection(connection).await;
    }
}

async fn handle_get(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Result<Vec<u8>, String> {
    let path = PathBuf::from("served_files").join(path);
    match fs::read(&path).await {
        Ok(data) => Ok(data),
        Err(e) => Err(format!("Failed to read file: {}", e))
    }
}

async fn handle_put(
    axum::extract::Path(path): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> Result<(), String> {
    let path = PathBuf::from("served_files").join(path);
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent).await {
            return Err(format!("Failed to create directory: {}", e));
        }
    }
    match fs::write(&path, body).await {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to write file: {}", e))
    }
}

async fn add_alt_svc_header(
    request: Request<Body>,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let alt_svc = HeaderValue::from_static("h3=\":4433\"; ma=3600");
    response.headers_mut().insert("alt-svc", alt_svc.clone());
    println!("Added Alt-Svc header: {:?}", alt_svc);
    response
}

async fn handle_connection(connection: Option<quinn::Incoming>) {
    match connection {
        Some(conn) => {
            println!("New QUIC connection incoming, awaiting handshake...");
            let connecting = conn.await;
            match connecting {
                Ok(connection) => {
                    println!("QUIC connection established from {}", 
                        connection.remote_address());
                    // Handle connection in a new task
                    tokio::spawn(async move {
                        println!("Starting HTTP/3 connection handling");
                        
                        let h3_conn = h3_quinn::Connection::new(connection);
                        let mut h3 = h3::server::Connection::new(h3_conn).await.unwrap();
                        
                        loop {
                            match h3.accept().await {
                                Ok(Some((req, mut stream))) => {
                                    println!("Received request: {:?}", req);
                                    
                                    println!("Processing request path: {}", req.uri().path());
                                    
                                    // Handle directory listing
                                    if req.uri().path().starts_with("/dir") {
                                        let dir_list = quicfs_common::types::DirList {
                                            entries: vec![
                                                quicfs_common::types::DirEntry {
                                                    name: "test.txt".to_string(),
                                                    type_: "file".to_string(),
                                                    size: 42,
                                                    mode: 0o644,
                                                    mtime: "2024-02-18T15:04:05Z".to_string(),
                                                    atime: "2024-02-18T15:04:05Z".to_string(),
                                                    ctime: "2024-02-18T15:04:05Z".to_string(),
                                                }
                                            ]
                                        };
                                        
                                        let json = serde_json::to_string(&dir_list).unwrap();
                                        
                                        let response = http::Response::builder()
                                            .status(200)
                                            .header("content-type", "application/json")
                                            .body(())
                                            .unwrap();
                                        
                                        stream.send_response(response).await.unwrap();
                                        stream.send_data(bytes::Bytes::from(json)).await.unwrap();
                                    } else {
                                        // Default response for other paths
                                        let response = http::Response::builder()
                                            .status(404)
                                            .header("content-type", "text/plain")
                                            .body(())
                                            .unwrap();
                                        
                                        stream.send_response(response).await.unwrap();
                                        stream.send_data(bytes::Bytes::from("Not Found")).await.unwrap();
                                    }
                                    stream.finish().await.unwrap();
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
                    });
                }
                Err(e) => eprintln!("QUIC connection handshake failed: {}", e),
            }
        }
        None => println!("No QUIC connection available"),
    }
}
