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
use quinn::{Endpoint, ServerConfig as QuinnServerConfig};
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
    let priv_key = rustls::PrivateKey(cert.serialize_private_key_der());
    let cert_chain = vec![rustls::Certificate(cert_der)];

    // Create QUIC server config with ALPN protocols for HTTP/3
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain.clone(), priv_key.clone())?;
    server_crypto.alpn_protocols = vec![b"h3".to_vec()];
    
    let mut quic_config = QuinnServerConfig::with_crypto(Arc::new(server_crypto));
    let endpoint = Endpoint::server(quic_config, "0.0.0.0:4433".parse()?)?;
    
    println!("QUIC server config created");
    
    match endpoint.local_addr() {
        Ok(local_addr) => println!("QUIC server bound to {}", local_addr),
        Err(e) => eprintln!("Failed to get QUIC local address: {}", e),
    }

    // Create HTTPS server
    let https_addr = "0.0.0.0:8443".parse::<SocketAddr>()?;
    let app = Router::new()
        .route("/files/*path", get(handle_get).put(handle_put))
        .layer(CorsLayer::permissive())
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

async fn handle_connection(connection: Option<quinn::Connecting>) {
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
                        println!("Starting bi-directional stream acceptance for connection");
                        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                            println!("New QUIC stream established");
                            
                            // Basic HTTP/3 response
                            let response = "HTTP/3 200 OK\r\n\
                                         Content-Length: 13\r\n\
                                         Content-Type: text/plain\r\n\
                                         \r\n\
                                         Hello World!\n";
                            
                            match send.write_all(response.as_bytes()).await {
                                Ok(_) => {
                                    println!("Sent HTTP/3 response");
                                    send.finish().await.unwrap_or_else(|e| eprintln!("Failed to finish stream: {}", e));
                                },
                                Err(e) => eprintln!("Failed to send on stream: {}", e),
                            }
                        }
                        println!("Connection stream loop ended");
                    });
                }
                Err(e) => eprintln!("QUIC connection handshake failed: {}", e),
            }
        }
        None => println!("No QUIC connection available"),
    }
}
