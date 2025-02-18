use anyhow::Result;
use axum::{
    routing::{get, put},
    Router,
};
use quinn::{Endpoint, ServerConfig as QuinnServerConfig};
use std::{net::SocketAddr, sync::Arc};
use std::path::PathBuf;
use tokio::fs;
use tower_http::cors::CorsLayer;

#[tokio::main]
async fn main() -> Result<()> {
    // Generate TLS certificate
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert_der = cert.serialize_der()?;
    let priv_key = cert.serialize_private_key_der();
    let priv_key = rustls::PrivateKey(priv_key);
    let cert_chain = vec![rustls::Certificate(cert_der)];

    // Create QUIC server config
    let quic_config = QuinnServerConfig::with_single_cert(cert_chain.clone(), priv_key.clone())?;
    
    // Create QUIC endpoint
    let quic_addr = "0.0.0.0:4433".parse::<SocketAddr>()?;
    let endpoint = Endpoint::server(quic_config, quic_addr)?;
    
    match endpoint.local_addr() {
        Ok(local_addr) => println!("QUIC server bound to {}", local_addr),
        Err(e) => eprintln!("Failed to get QUIC local address: {}", e),
    }

    // Create HTTPS server
    let https_addr = "0.0.0.0:8443".parse::<SocketAddr>()?;
    let app = Router::new()
        .route("/files/*path", get(handle_get).put(handle_put))
        .layer(CorsLayer::permissive());

    println!("HTTPS server listening on {}", https_addr);

    // Run both servers
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
        cert_chain[0].0.clone(),
        priv_key.0.clone(),
    )
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

    loop {
        let connection = endpoint.accept().await;
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

async fn handle_connection(connection: Option<quinn::Connecting>) {
    match connection {
        Some(conn) => {
            let connecting = conn.await;
            match connecting {
                Ok(connection) => {
                    println!("Connection established from {}", connection.remote_address());
                    // Handle connection in a new task
                    tokio::spawn(async move {
                        while let Ok((mut send, _recv)) = connection.accept_bi().await {
                            println!("New stream established");
                            // Here you would implement the actual file serving logic
                            // This is just a placeholder that acknowledges the stream
                            let _ = send.write_all(b"Hello from Quinn server!").await;
                        }
                    });
                }
                Err(e) => eprintln!("Connection failed: {}", e),
            }
        }
        None => (),
    }
}
