use anyhow::Result;
use std::path::PathBuf;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use quicfs_server::{fs::FileSystem, http::HttpServer};
use http::HttpServer;

#[tokio::main]
async fn main() -> Result<()> {
    // Generate TLS certificate
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    
    // For QUIC server (needs DER format)
    let cert_der = cert.serialize_der()?;
    let priv_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der()));
    let cert_chain = vec![CertificateDer::from(cert_der)];

    // Initialize filesystem
    let fs = FileSystem::new(PathBuf::from("served_files"))?;
    fs.ensure_root_exists().await?;

    // Create and run HTTP/3 server
    let server = HttpServer::new(cert_chain, priv_key, fs)?;
    server.run().await?;

    Ok(())
}
