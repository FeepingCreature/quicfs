use std::sync::Arc;
use anyhow::Result;
use axum::{
    routing::{get, patch},
    Router,
    body::Body,
};
use tower::Service;
use http::Request;
use http_body_util::BodyExt;
use bytes::{Buf, Bytes};
use quinn::{Endpoint, crypto::rustls::QuicServerConfig};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use crate::fs::FileSystem;

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
            .route("/dir/", get(crate::routes::list_directory))
            .route("/dir/*path", get(crate::routes::list_directory))
            .route("/file/*path", get(crate::routes::read_file))
            .route("/file/*path", patch(crate::routes::write_file))
            .with_state(self.fs.clone());

        loop {
            println!("Waiting for QUIC connection...");
            let connection = self.endpoint.accept().await;
            let app = app.clone();
            
            match connection {
                Some(conn) => {
                    println!("New QUIC connection incoming, awaiting handshake...");
                    tokio::spawn(async move {
                        let result: Result<()> = async {
                        if let Ok(connection) = conn.await {
                            println!("QUIC connection established from {}", 
                                connection.remote_address());
                            
                            let h3_conn = h3_quinn::Connection::new(connection);
                            let mut h3: h3::server::Connection<_, Bytes> = h3::server::Connection::new(h3_conn).await?;
                            
                            while let Ok(Some((req, mut stream))) = h3.accept().await {
                                let path = req.uri().path().to_string();
                                let method = req.method().clone();
                                let headers = req.headers().clone();
                                println!("Received request: {} {} with headers: {:?}", method, path, headers);
                                
                                // Use app to route the request
                                let mut app = app.clone();
                                // Read the request body
                                let mut body = Vec::new();
                                while let Ok(Some(chunk)) = stream.recv_data().await {
                                    body.extend_from_slice(chunk.chunk());
                                }
                                let request = Request::from_parts(
                                    req.into_parts().0,
                                    Body::from(body)
                                );
                                println!("Routing request to axum app...");
                                let response = app.call(request).await;
                                match response {
                                    Ok(response) => {
                                        let (parts, mut body) = response.into_parts();
                                        println!("Response status: {}", parts.status);
                                        println!("Response headers: {:?}", parts.headers);
                                        let h3_response = http::Response::from_parts(parts, ());
                                        println!("Sending HTTP/3 response headers...");
                                        stream.send_response(h3_response).await?;
                                        
                                        // Convert axum body to bytes
                                        println!("Converting response body...");
                                        match body.frame().await {
                                            Some(Ok(frame)) => {
                                                println!("Got response frame: {:?}", frame);
                                                if let Ok(data) = frame.into_data() {
                                                    println!("Converted to data: {:?}", String::from_utf8_lossy(&data));
                                                    println!("Sending response data ({} bytes)...", data.len());
                                                    stream.send_data(data.into()).await?;
                                                    println!("Response data sent successfully");
                                                } else {
                                                    println!("Failed to convert frame to data");
                                                }
                                            }
                                            Some(Err(e)) => {
                                                println!("Error converting body frame: {}", e);
                                                println!("Error details: {:?}", e);
                                                return Err(anyhow::anyhow!("Body error: {}", e));
                                            }
                                            None => println!("No response body to send"),
                                        }
                                    },
                                    Err(e) => {
                                        eprintln!("Error handling request: {}", e);
                                        let error_body = format!("Internal server error: {}", e);
                                        println!("Error body: {}", error_body);
                                        let response = http::Response::builder()
                                            .status(500)
                                            .header("content-type", "text/plain")
                                            .header("content-length", error_body.len().to_string())
                                            .body(())?;
                                        stream.send_response(response).await?;
                                        stream.send_data(error_body.into()).await?;
                                    }
                                }
                                stream.finish().await?;
                            }
                        }
                            Ok(())
                        }
                        .await;
                        
                        if let Err(e) = result {
                            eprintln!("Connection error: {}", e);
                        }
                    });
                }
                None => println!("No QUIC connection available"),
            }
        }
    }
}
