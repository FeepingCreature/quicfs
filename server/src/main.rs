use anyhow::Result;
use quinn::{Endpoint, ServerConfig};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::fs;

#[tokio::main]
async fn main() -> Result<()> {
    // Configure TLS
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert_der = cert.serialize_der()?;
    let priv_key = cert.serialize_private_key_der();
    let priv_key = rustls::PrivateKey(priv_key);
    let cert_chain = vec![rustls::Certificate(cert_der)];

    // Create server config
    let server_config = ServerConfig::with_single_cert(cert_chain, priv_key)?;
    
    // Create endpoint with explicit socket configuration
    let addr = "[::]:4433".parse::<SocketAddr>()?;
    let mut server_config = ServerConfig::with_single_cert(cert_chain, priv_key)?;
    server_config.use_stateless_retry(true);
    
    let endpoint = Endpoint::server(server_config, addr)?;
    println!("Listening on {} (IPv4 and IPv6)", addr);
    
    // Verify we're actually listening
    if let Some(local_addr) = endpoint.local_addr() {
        println!("Server socket bound to {}", local_addr);
    } else {
        eprintln!("Failed to get local address!");
    }

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
