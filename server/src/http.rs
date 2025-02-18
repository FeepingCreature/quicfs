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
                if req.uri().path().starts_with("/dir") {
                    let dir_list = fs.list_directory(req.uri().path()).await?;
                    let json = serde_json::to_string(&dir_list)?;
                    
                    let response = http::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(())?;
                    
                    stream.send_response(response).await?;
                    stream.send_data(bytes::Bytes::from(json)).await?;
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
