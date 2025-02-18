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
    
    // Create endpoints
    let addr_v6 = "[::]:4433".parse::<SocketAddr>()?;
    let addr_v4 = "0.0.0.0:4433".parse::<SocketAddr>()?;
    
    let endpoint_v6 = Endpoint::server(server_config.clone(), addr_v6)?;
    let endpoint_v4 = Endpoint::server(server_config, addr_v4)?;
    
    println!("Listening on {} and {}", addr_v6, addr_v4);

    // Serve directory
    let serve_dir = PathBuf::from("served_files");
    if !serve_dir.exists() {
        fs::create_dir(&serve_dir).await?;
        println!("Created served_files directory");
    }

    loop {
        tokio::select! {
            connection = endpoint_v6.accept() => {
                handle_connection(connection).await;
            }
            connection = endpoint_v4.accept() => {
                handle_connection(connection).await;
            }
        }
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
            None => break,
        }
    }

    Ok(())
}
