use anyhow::Result;
use std::path::PathBuf;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use clap::Parser;

use quicfs_server::{fs::FileSystem, http::HttpServer};

#[derive(Parser)]
#[command(name = "quicfs-server")]
#[command(about = "A QUIC-based filesystem server")]
struct Opts {
    /// Directory to serve as the filesystem root
    #[arg(short, long, default_value = "served_files")]
    root: PathBuf,
    
    /// Port to listen on
    #[arg(short, long, default_value = "4433")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging with debug level
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    rustls::crypto::aws_lc_rs::default_provider().install_default().unwrap();

    let opts = Opts::parse();

    // Generate TLS certificate
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    
    // For QUIC server (needs DER format)
    let cert_der = cert.cert.der().clone();
    let priv_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let cert_chain = vec![CertificateDer::from(cert_der)];

    // Initialize filesystem with custom root directory
    let fs = FileSystem::new(opts.root.clone())?;
    fs.ensure_root_exists().await?;

    println!("Serving directory: {:?}", opts.root);
    println!("Listening on port: {}", opts.port);

    // Create and run HTTP/3 server
    let server = HttpServer::new(cert_chain, priv_key, fs, opts.port)?;
    server.run().await?;

    Ok(())
}
