use std::sync::Arc;
use anyhow::Result;
use axum::{
    routing::{get, patch},
    Router,
};
use quinn::{Endpoint, crypto::rustls::QuicServerConfig};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use crate::{fs::FileSystem, routes};

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

        // Create router with routes
        let app = Router::new()
            .route("/dir/*path", get(routes::list_directory))
            .route("/file/*path", get(routes::read_file))
            .route("/file/*path", patch(routes::write_file))
            .with_state(self.fs.clone());

        loop {
            println!("Waiting for QUIC connection...");
            let connection = self.endpoint.accept().await;
            let app = app.clone();
            
            match connection {
                Some(conn) => {
                    println!("New QUIC connection incoming, awaiting handshake...");
                    tokio::spawn(async move {
                        if let Ok(connection) = conn.await {
                            println!("QUIC connection established from {}", 
                                connection.remote_address());
                            
                            let h3_conn = h3_quinn::Connection::new(connection);
                            let mut h3 = h3::server::Connection::new(h3_conn).await?;
                            
                            while let Ok(Some((req, mut send))) = h3.accept().await {
                                let path = req.uri().path().to_string();
                                let method = req.method().clone();
                                let headers = req.headers().clone();
                                
                                // Use app to route the request
                                let response = app.call(req).await;
                                match response {
                                    Ok(response) => {
                                        let (parts, body) = response.into_parts();
                                        let h3_response = http::Response::from_parts(parts, ());
                                        send.send_response(h3_response).await?;
                                        
                                        // Send body if any
                                        if let Some(body) = body.data().await {
                                            if let Ok(data) = body {
                                                send.send_data(data).await?;
                                            }
                                        }
                                    },
                                    Err(e) => {
                                        eprintln!("Error handling request: {}", e);
                                        let response = http::Response::builder()
                                            .status(500)
                                            .body(())?;
                                        send.send_response(response).await?;
                                    }
                                }
                                send.finish().await?;
                            }
                        }
                    });
                }
                None => println!("No QUIC connection available"),
            }
        }
    }
}
