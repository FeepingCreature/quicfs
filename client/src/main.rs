use anyhow::Result;
use clap::Parser;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEntry, Request as FuseRequest, TimeOrNow, ReplyOpen, KernelConfig, ReplyDirectoryPlus,
};
use libc::ENOENT;
use std::ffi::OsStr;
use std::time::{Duration, SystemTime};
use tracing::{info, warn, error};
use std::collections::HashMap;
use std::sync::Arc;
use quicfs_common::types::DirList;
use futures::future;
use http::Request;
use bytes::Buf;
use quinn::rustls::{self, pki_types, client::danger};
use quinn::VarInt;

const TTL: Duration = Duration::from_secs(1);

#[derive(Parser)]
#[command(name = "quicfs-client")]
#[command(about = "A QUIC-based filesystem client")]
struct Opts {
    /// Mount point for the filesystem
    #[arg(short, long)]
    mountpoint: String,

    /// Server URL (e.g., https://localhost:4433)
    #[arg(short, long)]
    server: String,
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::aws_lc_rs::default_provider())))
    }
}

impl danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &pki_types::CertificateDer<'_>,
        _intermediates: &[pki_types::CertificateDer<'_>],
        _server_name: &pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: pki_types::UnixTime,
    ) -> Result<danger::ServerCertVerified, rustls::Error> {
        Ok(danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

const ROOT_INODE: u64 = 1;

struct QuicFS {
    send_request: Option<h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>>,
    inodes: HashMap<u64, FileAttr>,
    paths: HashMap<u64, String>,
    next_inode: u64,
    server_url: String,
}

impl QuicFS {
    async fn write_file(&mut self, path: &str, offset: u64, contents: &[u8]) -> Result<()> {
        let server_uri = self.server_url.parse::<http::Uri>()?;
        let host = server_uri.host().unwrap_or("localhost");
        let port = server_uri.port_u16().unwrap_or(4433);
        let encoded_path = urlencoding::encode(path.trim_start_matches('/'));
        let request_path = format!("/file/{}", encoded_path);
        let contents = contents.to_vec();

        for attempt in 0..2 {
            self.ensure_connected().await?;

            let req = Request::builder()
                .method("PATCH")
                .uri(&request_path)
                .header("host", format!("{}:{}", host, port))
                .header("Content-Range", format!("bytes {}-{}/{}",
                    offset,
                    offset + (contents.len() as u64).saturating_sub(1),
                    offset + contents.len() as u64))
                .body(())?;

            let result = async {
                let mut stream = self.send_request.as_mut().unwrap().send_request(req).await?;
                stream.send_data(bytes::Bytes::from(contents.clone())).await?;
                stream.finish().await?;

                let resp = stream.recv_response().await?;

                if !resp.status().is_success() {
                    let mut body = Vec::new();
                    while let Some(chunk) = stream.recv_data().await? {
                        body.extend_from_slice(chunk.chunk());
                    }
                    let error: serde_json::Value = serde_json::from_slice(&body)?;
                    return Err(anyhow::anyhow!("Server error: {}",
                        error.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error")));
                }

                Ok(())
            }.await;

            match result {
                Ok(result) => return Ok(result),
                Err(e) if attempt == 0 => {
                    warn!("Request failed, retrying with new connection: {}", e);
                    // Force reconnection on next attempt
                    self.send_request = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn read_file(&mut self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>> {
        let server_uri = self.server_url.parse::<http::Uri>()?;
        let host = server_uri.host().unwrap_or("localhost");
        let port = server_uri.port_u16().unwrap_or(4433);
        let encoded_path = urlencoding::encode(path.trim_start_matches('/'));
        let request_path = format!("/file/{}", encoded_path);

        for attempt in 0..2 {
            self.ensure_connected().await?;

            let req = Request::builder()
                .method("GET")
                .uri(&request_path)
                .header("host", format!("{}:{}", host, port))
                .header("Range", format!("bytes={}-{}", offset, offset + size as u64 - 1))
                .body(())?;

            let result = async {
                let mut stream = self.send_request.as_mut().unwrap().send_request(req).await?;
                stream.finish().await?;

                let resp = stream.recv_response().await?;

                let mut body = Vec::new();
                while let Some(chunk) = stream.recv_data().await? {
                    body.extend_from_slice(chunk.chunk());
                }

                if !resp.status().is_success() {
                    let error: serde_json::Value = serde_json::from_slice(&body)?;
                    return Err(anyhow::anyhow!("Server error: {}",
                        error.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error")));
                }

                Ok(body)
            }.await;

            match result {
                Ok(result) => return Ok(result),
                Err(e) if attempt == 0 => {
                    warn!("Request failed, retrying with new connection: {}", e);
                    // Force reconnection on next attempt
                    self.send_request = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn head_file(&mut self, path: &str) -> Result<FileAttr> {
        let server_uri = self.server_url.parse::<http::Uri>()?;
        let host = server_uri.host().unwrap_or("localhost");
        let port = server_uri.port_u16().unwrap_or(4433);
        let encoded_path = urlencoding::encode(path.trim_start_matches('/'));
        let request_path = format!("/file/{}", encoded_path);

        for attempt in 0..2 {
            self.ensure_connected().await?;

            let req = Request::builder()
                .method("HEAD")
                .uri(&request_path)
                .header("host", format!("{}:{}", host, port))
                .body(())?;

            let result = async {
                let mut stream = self.send_request.as_mut().unwrap().send_request(req).await?;
                stream.finish().await?;

                let resp = stream.recv_response().await?;

                if !resp.status().is_success() {
                    return Err(anyhow::anyhow!("File not found"));
                }

                // Extract metadata from headers
                let size = resp.headers().get("Content-Length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);

                let file_type = resp.headers().get("X-File-Type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("file");

                let mode = resp.headers().get("X-File-Mode")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0o644);

                let mtime = resp.headers().get("X-Access-Time")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);

                let file_type_enum = match file_type {
                    "dir" => FileType::Directory,
                    _ => FileType::RegularFile,
                };

                let attr = FileAttr {
                    ino: 0, // Will be set by caller
                    size,
                    blocks: (size + 511) / 512,
                    atime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime),
                    mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime),
                    ctime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime),
                    crtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime),
                    kind: file_type_enum,
                    perm: mode as u16,
                    nlink: if file_type_enum == FileType::Directory { 2 } else { 1 },
                    uid: 1000,
                    gid: 1000,
                    rdev: 0,
                    flags: 0,
                    blksize: 512,
                };

                Ok(attr)
            }.await;

            match result {
                Ok(result) => return Ok(result),
                Err(e) if attempt == 0 => {
                    warn!("Request failed, retrying with new connection: {}", e);
                    self.send_request = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn read_file_concurrent(&mut self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>> {
        const CHUNK_SIZE: u32 = 256 * 1024; // 256KB per stream

        if size <= CHUNK_SIZE {
            return self.read_file(path, offset, size).await;
        }

        let mut tasks = Vec::new();
        let mut current_offset = offset;
        let mut remaining = size;

        while remaining > 0 {
            let chunk_size = std::cmp::min(remaining, CHUNK_SIZE);
            let chunk_offset = current_offset;
            let path = path.to_string();
            let server_url = self.server_url.clone();

            // Create new connection for concurrent use
            let task = tokio::spawn(async move {
                let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;

                let mut crypto_config = rustls::ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(SkipServerVerification::new())
                    .with_no_client_auth();

                crypto_config.alpn_protocols = vec![b"h3".to_vec()];
                crypto_config.enable_early_data = true;

                let mut transport_config = quinn::TransportConfig::default();
                transport_config.max_concurrent_bidi_streams(100u32.into());
                transport_config.max_concurrent_uni_streams(100u32.into());
                transport_config.send_window(8 * 1024 * 1024);
                transport_config.receive_window(VarInt::from_u32(8 * 1024 * 1024));
                transport_config.stream_receive_window(VarInt::from_u32(2 * 1024 * 1024));

                let mut client_config = quinn::ClientConfig::new(Arc::new(
                    quinn::crypto::rustls::QuicClientConfig::try_from(crypto_config)?
                ));
                client_config.transport_config(Arc::new(transport_config));
                endpoint.set_default_client_config(client_config);

                let server_uri = server_url.parse::<http::Uri>()?;
                let host = server_uri.host().unwrap_or("localhost");
                let port = server_uri.port_u16().unwrap_or(4433);
                let addr = format!("{}:{}", host, port).parse()?;
                let connection = endpoint.connect(addr, host)?.await?;

                let h3_conn = h3_quinn::Connection::new(connection);
                let (mut driver, mut send_request) = h3::client::new(h3_conn).await?;

                tokio::spawn(async move {
                    future::poll_fn(|cx| driver.poll_close(cx)).await;
                });

                let encoded_path = urlencoding::encode(path.trim_start_matches('/'));
                let request_path = format!("/file/{}", encoded_path);

                let req = Request::builder()
                    .method("GET")
                    .uri(&request_path)
                    .header("host", format!("{}:{}", host, port))
                    .header("Range", format!("bytes={}-{}", chunk_offset, chunk_offset + chunk_size as u64 - 1))
                    .body(())?;

                let mut stream = send_request.send_request(req).await?;
                stream.finish().await?;

                let resp = stream.recv_response().await?;

                let mut body = Vec::new();
                while let Some(chunk) = stream.recv_data().await? {
                    body.extend_from_slice(chunk.chunk());
                }

                if !resp.status().is_success() {
                    let error: serde_json::Value = serde_json::from_slice(&body)?;
                    return Err(anyhow::anyhow!("Server error: {}",
                        error.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error")));
                }

                Ok::<(u64, Vec<u8>), anyhow::Error>((chunk_offset, body))
            });

            tasks.push(task);
            current_offset += chunk_size as u64;
            remaining -= chunk_size;
        }

        // Collect results and reassemble
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await??);
        }

        results.sort_by_key(|(offset, _)| *offset);
        let mut final_data = Vec::with_capacity(size as usize);
        for (_, data) in results {
            final_data.extend(data);
        }

        Ok(final_data)
    }

    async fn list_directory(&mut self, path: &str) -> Result<DirList> {
        let server_uri = self.server_url.parse::<http::Uri>()?;
        let host = server_uri.host().unwrap_or("localhost");
        let port = server_uri.port_u16().unwrap_or(4433);
        let encoded_path = if path == "/" {
            "/".to_string()
        } else {
            urlencoding::encode(path.trim_start_matches('/')).to_string()
        };
        let request_path = format!("/dir/{}", encoded_path.trim_start_matches('/'));

        for attempt in 0..2 {
            self.ensure_connected().await?;

            let req = Request::builder()
                .method("GET")
                .uri(&request_path)
                .header("host", format!("{}:{}", host, port))
                .body(())?;

            let result = async {
                let mut stream = self.send_request.as_mut().unwrap().send_request(req).await?;
                stream.finish().await?;

                let resp = stream.recv_response().await?;
                let status = resp.status();

                let mut body = Vec::new();
                while let Some(chunk) = stream.recv_data().await? {
                    body.extend_from_slice(chunk.chunk());
                }

                if !status.is_success() {
                    let error: serde_json::Value = serde_json::from_slice(&body)?;
                    return Err(anyhow::anyhow!("Server error: {}",
                        error.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error")));
                }

                match serde_json::from_slice::<DirList>(&body) {
                    Ok(dir_list) => Ok(dir_list),
                    Err(e) => {
                        error!("Failed to parse directory listing: {}", e);
                        Err(anyhow::anyhow!("Failed to parse directory listing: {}", e))
                    }
                }
            }.await;

            match result {
                Ok(result) => return Ok(result),
                Err(e) if attempt == 0 => {
                    warn!("Request failed, retrying with new connection: {}", e);
                    // Force reconnection on next attempt
                    self.send_request = None;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn setup_connection(&self) -> Result<h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>> {
        info!("Setting up QUIC connection...");

        // Parse server URL and connect
        let url = self.server_url.parse::<http::Uri>()?;
        let host = url.host().unwrap_or("localhost");
        let port = url.port_u16().unwrap_or(4433);
        info!("Connecting to {}:{}", host, port);

        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;

        let mut crypto_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth();

        crypto_config.alpn_protocols = vec![b"h3".to_vec()];
        crypto_config.enable_early_data = true;

        // Add QUIC transport configuration for performance
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.max_concurrent_bidi_streams(100u32.into());
        transport_config.max_concurrent_uni_streams(100u32.into());
        transport_config.send_window(8 * 1024 * 1024); // 8MB
        transport_config.receive_window(VarInt::from_u32(8 * 1024 * 1024)); // 8MB
        transport_config.stream_receive_window(VarInt::from_u32(2 * 1024 * 1024)); // 2MB per stream

        let mut client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto_config)?
        ));
        client_config.transport_config(Arc::new(transport_config));
        endpoint.set_default_client_config(client_config);

        // Force IPv4 lookup
        let addr = format!("{}:{}", host, port).parse()?;
        let connection = endpoint.connect(addr, host)?.await?;

        let h3_conn = h3_quinn::Connection::new(connection);
        let (mut driver, send_request) = h3::client::new(h3_conn).await?;

        // Spawn the connection driver
        tokio::spawn(async move {
            future::poll_fn(|cx| driver.poll_close(cx)).await;
        });

        Ok(send_request)
    }

    async fn connect(&self) -> Result<h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>> {
        self.setup_connection().await
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        // Always try to use existing connection first
        if let Some(ref mut _send_request) = self.send_request {
            // Test the connection with a simple ping-like request
            // If it fails, we'll recreate the connection below
            return Ok(());
        }

        // Create new connection
        info!("Creating new QUIC connection...");
        self.send_request = Some(self.connect().await?);
        Ok(())
    }

    async fn new(server_url: String) -> Result<Self> {
        info!("Creating new QuicFS instance");

        let mut fs = QuicFS {
            send_request: None,
            inodes: HashMap::new(),
            paths: HashMap::new(),
            next_inode: 2,  // 1 is reserved for root
            server_url,
        };

        // Initial connection
        fs.ensure_connected().await?;

        // Create root directory
        let root = FileAttr {
            ino: ROOT_INODE,
            size: 0,
            blocks: 0,
            atime: SystemTime::now(),
            mtime: SystemTime::now(),
            ctime: SystemTime::now(),
            crtime: SystemTime::now(),
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            flags: 0,
            blksize: 512,
        };

        fs.inodes.insert(ROOT_INODE, root);
        fs.paths.insert(ROOT_INODE, "/".to_string());
        Ok(fs)
    }

    fn create_file_attr_from_entry(&mut self, entry: &quicfs_common::types::DirEntry, path: &str) -> FileAttr {
        let file_type = match entry.type_.as_str() {
            "file" => FileType::RegularFile,
            "dir" => FileType::Directory,
            _ => FileType::RegularFile, // Default fallback
        };

        let ino = self.next_inode;
        self.next_inode += 1;

        let attr = FileAttr {
            ino,
            size: entry.size,
            blocks: (entry.size + 511) / 512,
            atime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.atime.parse().unwrap_or(0)),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.mtime.parse().unwrap_or(0)),
            ctime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.ctime.parse().unwrap_or(0)),
            crtime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.ctime.parse().unwrap_or(0)),
            kind: file_type,
            perm: entry.mode as u16,
            nlink: if file_type == FileType::Directory { 2 } else { 1 },
            uid: 1000,
            gid: 1000,
            rdev: 0,
            flags: 0,
            blksize: 512,
        };

        // Store both the attributes and the path
        self.paths.insert(ino, path.to_string());
        self.inodes.insert(ino, attr.clone());

        attr
    }
}

impl Filesystem for QuicFS {
    fn init(&mut self, _req: &FuseRequest, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        info!("Initializing filesystem with advanced FUSE capabilities");

        // Combine multiple performance and functionality flags
        let capabilities = fuser::consts::FUSE_WRITEBACK_CACHE    // Writeback cache for better write performance
            | fuser::consts::FUSE_BIG_WRITES                      // Support writes larger than 4KB
            | fuser::consts::FUSE_ASYNC_READ                      // Asynchronous read requests
            | fuser::consts::FUSE_ASYNC_DIO                       // Asynchronous direct I/O
            | fuser::consts::FUSE_PARALLEL_DIROPS                 // Parallel directory operations
            | fuser::consts::FUSE_AUTO_INVAL_DATA                 // Automatic cache invalidation
            | fuser::consts::FUSE_DO_READDIRPLUS                  // READDIR+LOOKUP optimization
            | fuser::consts::FUSE_CACHE_SYMLINKS                  // Cache symlink responses
            | fuser::consts::FUSE_ATOMIC_O_TRUNC                  // Handle O_TRUNC atomically
            | fuser::consts::FUSE_DONT_MASK;                      // Don't apply umask to file modes

        match config.add_capabilities(capabilities) {
            Ok(()) => {
                info!("Successfully enabled advanced FUSE capabilities: 0x{:x}", capabilities);
            }
            Err(unsupported) => {
                warn!("Some FUSE capabilities not supported: 0x{:x}", unsupported);
                info!("Enabled capabilities: 0x{:x}", capabilities & !unsupported);
            }
        }

        // Set performance parameters
        let _ = config.set_max_write(1024 * 1024); // 1MB writes
        let _ = config.set_max_readahead(1024 * 1024); // 1MB readahead

        Ok(())
    }

    fn open(&mut self, _req: &FuseRequest, ino: u64, flags: i32, reply: ReplyOpen) {
        info!("open: inode {} with flags {}", ino, flags);

        // Check if file exists
        if !self.inodes.contains_key(&ino) {
            reply.error(ENOENT);
            return;
        }

        // Return file handle with cache flags for better performance
        let fh = ino; // Use inode as file handle for simplicity
        let open_flags = fuser::consts::FOPEN_KEEP_CACHE;

        reply.opened(fh, open_flags);
    }

    fn create(
        &mut self,
        _req: &FuseRequest,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        info!("create: {} in {} with mode {:o}", name.to_string_lossy(), parent, mode);

        // Get parent directory path
        let parent_path = if parent == ROOT_INODE {
            "/".to_string()
        } else {
            match self.paths.get(&parent) {
                Some(path) => path.clone(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        // Construct the full path for the new file
        let entry_path = if parent_path == "/" {
            format!("/{}", name.to_string_lossy())
        } else {
            format!("{}/{}", parent_path, name.to_string_lossy())
        };

        // Check if file already exists using HEAD request
        let head_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.head_file(&entry_path).await
            })
        });

        match head_result {
            Ok(mut attr) => {
                // File exists, return existing inode if we have it
                if let Some((&existing_ino, _)) = self.paths.iter().find(|(_, p)| **p == entry_path) {
                    if let Some(existing_attr) = self.inodes.get(&existing_ino) {
                        reply.created(&TTL, existing_attr, 0, 0, flags as u32);
                        return;
                    }
                }

                // File exists but we don't have an inode for it, create one
                let ino = self.next_inode;
                self.next_inode += 1;
                attr.ino = ino;

                self.inodes.insert(ino, attr);
                self.paths.insert(ino, entry_path.clone());
                reply.created(&TTL, &attr, 0, 0, flags as u32);
            }
            Err(_) => {
                // File doesn't exist, create a new inode
                let ino = self.next_inode;
                self.next_inode += 1;

                let attr = FileAttr {
                    ino,
                    size: 0,
                    blocks: 0,
                    atime: SystemTime::now(),
                    mtime: SystemTime::now(),
                    ctime: SystemTime::now(),
                    crtime: SystemTime::now(),
                    kind: FileType::RegularFile,
                    perm: mode as u16,
                    nlink: 1,
                    uid: 1000,
                    gid: 1000,
                    rdev: 0,
                    flags: flags as u32,
                    blksize: 512,
                };

                // Store the inode and path
                self.inodes.insert(ino, attr);
                info!("Created inode {} with path {}", ino, entry_path);
                self.paths.insert(ino, entry_path.clone());

                // Create empty file on server
                let create_result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        self.write_file(&entry_path, 0, &[]).await
                    })
                });

                match create_result {
                    Ok(_) => {
                        reply.created(&TTL, &attr, 0, 0, flags as u32);
                    }
                    Err(e) => {
                        warn!("Failed to create file: {}", e);
                        self.inodes.remove(&ino);
                        reply.error(libc::EIO);
                    }
                }
            }
        }
    }

    fn lookup(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: ReplyEntry) {
        info!("lookup: {} in {} (pid: {})", name.to_string_lossy(), parent, _req.pid());

        // Ignore special directories
        if name.to_string_lossy().starts_with('.') {
            reply.error(ENOENT);
            return;
        }

        // Get parent directory path
        let parent_path = if parent == ROOT_INODE {
            "/".to_string()
        } else {
            match self.paths.get(&parent) {
                Some(path) => path.clone(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        // Construct the full path for the requested entry
        let entry_path = if parent_path == "/" {
            format!("/{}", name.to_string_lossy())
        } else {
            format!("{}/{}", parent_path, name.to_string_lossy())
        };

        // Check if we already have this path mapped
        if let Some((&existing_ino, _)) = self.paths.iter().find(|(_, p)| **p == entry_path) {
            if let Some(attr) = self.inodes.get(&existing_ino) {
                reply.entry(&TTL, attr, 0);
                return;
            }
        }

        // Use HEAD request to check if file exists and get metadata
        let head_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.head_file(&entry_path).await
            })
        });

        match head_result {
            Ok(mut attr) => {
                let ino = self.next_inode;
                self.next_inode += 1;
                attr.ino = ino;
                
                self.paths.insert(ino, entry_path.clone());
                self.inodes.insert(ino, attr.clone());
                info!("Created inode {} for path {}", ino, entry_path);
                reply.entry(&TTL, &attr, 0);
            }
            Err(_) => {
                reply.error(ENOENT);
            }
        }
    }

    fn getattr(&mut self, _req: &FuseRequest, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        info!("getattr: {}", ino);

        // For root directory, just return cached attributes
        if ino == ROOT_INODE {
            if let Some(attr) = self.inodes.get(&ino) {
                reply.attr(&TTL, attr);
                return;
            }
        }

        // For files, use HEAD request to get current attributes
        let attr_result: Result<FileAttr, anyhow::Error> = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Get the stored path for this inode
                let path = self.paths.get(&ino)
                    .ok_or_else(|| anyhow::anyhow!("Path not found for inode {}", ino))?
                    .clone();

                // Use HEAD request to get fresh metadata
                let mut attr = self.head_file(&path).await?;
                attr.ino = ino; // Preserve the inode number

                // Update the cache
                self.inodes.insert(ino, attr.clone());

                Ok(attr)
            })
        });

        match attr_result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => {
                warn!("Failed to get attributes: {}", e);
                // Fall back to cached attributes if available
                match self.inodes.get(&ino) {
                    Some(attr) => reply.attr(&TTL, attr),
                    None => reply.error(ENOENT),
                }
            }
        }
    }

    fn read(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        // Look up the inode
        let attr = match self.inodes.get(&ino) {
            Some(attr) => attr,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        // Only allow reading regular files
        if attr.kind != FileType::RegularFile {
            reply.error(libc::EISDIR);
            return;
        }

        // Make request to server
        let read_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Get the stored path for this inode
                let path = self.paths.get(&ino)
                    .ok_or_else(|| anyhow::anyhow!("Path not found for inode {}", ino))?
                    .clone();

                // Use concurrent reads for large requests
                if size > 512 * 1024 { // 512KB threshold for concurrent reads
                    self.read_file_concurrent(&path, offset as u64, size).await
                } else {
                    self.read_file(&path, offset as u64, size).await
                }
            })
        });

        match read_result {
            Ok(data) => reply.data(&data),
            Err(e) => {
                warn!("Failed to read file: {}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        info!("readdir: {} at offset {}", ino, offset);

        // Get the path for this inode
        let dir_path = if ino == ROOT_INODE {
            "/".to_string()
        } else {
            match self.paths.get(&ino) {
                Some(path) => path.clone(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        // Create entries vector with . and ..
        let mut entries = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ino, FileType::Directory, "..".to_string()),
        ];

        // Fetch directory contents from server
        let server_entries = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.list_directory(&dir_path).await
            })
        });

        match server_entries {
            Ok(dir_list) => {
                // Process each entry and ensure we have inodes for them
                for entry in dir_list.entries {
                    let entry_path = if dir_path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", dir_path, entry.name)
                    };

                    // Check if we already have an inode for this path
                    let entry_ino = if let Some((&existing_ino, _)) = self.paths.iter().find(|(_, p)| **p == entry_path) {
                        existing_ino
                    } else {
                        // Create new inode
                        let new_ino = self.next_inode;
                        self.next_inode += 1;
                        
                        let file_type = match entry.type_.as_str() {
                            "file" => FileType::RegularFile,
                            "dir" => FileType::Directory,
                            _ => FileType::RegularFile,
                        };

                        let attr = FileAttr {
                            ino: new_ino,
                            size: entry.size,
                            blocks: (entry.size + 511) / 512,
                            atime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.atime.parse().unwrap_or(0)),
                            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.mtime.parse().unwrap_or(0)),
                            ctime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.ctime.parse().unwrap_or(0)),
                            crtime: SystemTime::UNIX_EPOCH + Duration::from_secs(entry.ctime.parse().unwrap_or(0)),
                            kind: file_type,
                            perm: entry.mode as u16,
                            nlink: if file_type == FileType::Directory { 2 } else { 1 },
                            uid: 1000,
                            gid: 1000,
                            rdev: 0,
                            flags: 0,
                            blksize: 512,
                        };

                        self.paths.insert(new_ino, entry_path);
                        self.inodes.insert(new_ino, attr);
                        new_ino
                    };

                    let file_type = match entry.type_.as_str() {
                        "file" => FileType::RegularFile,
                        "dir" => FileType::Directory,
                        _ => FileType::RegularFile,
                    };

                    entries.push((entry_ino, file_type, entry.name));
                }

                // Handle offset and add entries
                for (i, (ino, file_type, name)) in entries.iter().enumerate().skip(offset as usize) {
                    if reply.add(*ino, (i + 1) as i64, *file_type, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => {
                error!("readdir: Failed to list directory: {:?}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn readdirplus(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        info!("readdirplus: {} at offset {}", ino, offset);

        // Get the path for this inode
        let dir_path = if ino == ROOT_INODE {
            "/".to_string()
        } else {
            match self.paths.get(&ino) {
                Some(path) => path.clone(),
                None => {
                    reply.error(ENOENT);
                    return;
                }
            }
        };

        // Fetch directory contents from server
        let server_entries = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.list_directory(&dir_path).await
            })
        });

        match server_entries {
            Ok(dir_list) => {
                // Create entries with full attributes (readdir + lookup combined)
                let mut entries = Vec::new();

                // Add . and .. entries
                if let Some(dir_attr) = self.inodes.get(&ino) {
                    entries.push((ino, FileType::Directory, ".".to_string(), dir_attr.clone()));
                    entries.push((ino, FileType::Directory, "..".to_string(), dir_attr.clone()));
                }

                // Process server entries and create/update inodes with full attributes
                for entry in dir_list.entries {
                    let entry_path = if dir_path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", dir_path, entry.name)
                    };
                    
                    // Check if we already have this path mapped to an inode
                    let (entry_ino, attr) = if let Some((&existing_ino, _)) = self.paths.iter().find(|(_, p)| **p == entry_path) {
                        // Update existing inode with fresh data from server
                        if let Some(mut attr) = self.inodes.get(&existing_ino).cloned() {
                            attr.size = entry.size;
                            attr.blocks = (entry.size + 511) / 512;
                            attr.atime = SystemTime::UNIX_EPOCH + Duration::from_secs(entry.atime.parse().unwrap_or(0));
                            attr.mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(entry.mtime.parse().unwrap_or(0));
                            attr.ctime = SystemTime::UNIX_EPOCH + Duration::from_secs(entry.ctime.parse().unwrap_or(0));
                            
                            // Update the cache
                            self.inodes.insert(existing_ino, attr.clone());
                            (existing_ino, attr)
                        } else {
                            // Existing inode not found in cache, create new one
                            let attr = self.create_file_attr_from_entry(&entry, &entry_path);
                            (attr.ino, attr)
                        }
                    } else {
                        // Create new inode
                        let attr = self.create_file_attr_from_entry(&entry, &entry_path);
                        (attr.ino, attr)
                    };

                    let file_type = match entry.type_.as_str() {
                        "file" => FileType::RegularFile,
                        "dir" => FileType::Directory,
                        _ => FileType::RegularFile,
                    };

                    entries.push((entry_ino, file_type, entry.name, attr));
                }

                // Handle offset and add entries to reply
                for (i, (ino, _file_type, name, attr)) in entries.iter().enumerate().skip(offset as usize) {
                    // Reply is full, break the loop
                    if reply.add(*ino, (i + 1) as i64, name, &TTL, attr, 0) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => {
                error!("readdirplus: Failed to list directory: {:?}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn setattr(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        info!("setattr: {} mode={:?} uid={:?} gid={:?} size={:?}", ino, mode, uid, gid, size);

        // Get the current attributes
        let mut attr = match self.inodes.get(&ino).cloned() {
            Some(attr) => attr,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        // Update the attributes
        if let Some(mode) = mode {
            attr.perm = mode as u16;
        }
        if let Some(uid) = uid {
            attr.uid = uid;
        }
        if let Some(gid) = gid {
            attr.gid = gid;
        }
        if let Some(size) = size {
            // Handle truncate by making a request to the server
            let truncate_result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let path = self.paths.get(&ino)
                        .ok_or_else(|| anyhow::anyhow!("Path not found for inode {}", ino))?
                        .clone();

                    // Parse the server URL to extract host and port for the header
                    let server_uri = self.server_url.parse::<http::Uri>()?;
                    let host = server_uri.host().unwrap_or("localhost");
                    let port = server_uri.port_u16().unwrap_or(4433);

                    // Build the encoded request path
                    let encoded_path = urlencoding::encode(path.trim_start_matches('/')).to_string();
                    let request_path = format!("/file/{}", encoded_path);

                    for attempt in 0..2 {
                        self.ensure_connected().await?;

                        let req = Request::builder()
                            .method("PATCH")
                            .uri(&request_path)
                            .header("host", format!("{}:{}", host, port))
                            .header("Content-Range", format!("bytes */{}", size))
                            .body(())?;

                        let result = async {
                            let mut stream = self.send_request.as_mut().unwrap().send_request(req).await?;
                            stream.finish().await?;

                            let resp = stream.recv_response().await?;
                            if !resp.status().is_success() {
                                let mut body = Vec::new();
                                while let Some(chunk) = stream.recv_data().await? {
                                    body.extend_from_slice(chunk.chunk());
                                }
                                let error: serde_json::Value = serde_json::from_slice(&body)?;
                                return Err(anyhow::anyhow!("Server error: {}",
                                    error.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error")));
                            }

                            Ok(())
                        }.await;

                        match result {
                            Ok(result) => return Ok(result),
                            Err(e) if attempt == 0 => {
                                warn!("Request failed, retrying with new connection: {}", e);
                                // Force reconnection on next attempt
                                self.send_request = None;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    unreachable!()
                })
            });

            match truncate_result {
                Ok(_) => {
                    attr.size = size;
                }
                Err(e) => {
                    warn!("Failed to truncate file: {}", e);
                    reply.error(libc::EIO);
                    return;
                }
            }
        }
        if let Some(atime) = atime {
            attr.atime = match atime {
                TimeOrNow::Now => SystemTime::now(),
                TimeOrNow::SpecificTime(time) => time,
            };
        }
        if let Some(mtime) = mtime {
            attr.mtime = match mtime {
                TimeOrNow::Now => SystemTime::now(),
                TimeOrNow::SpecificTime(time) => time,
            };
        }
        if let Some(flags) = flags {
            attr.flags = flags;
        }

        // Store updated attributes
        self.inodes.insert(ino, attr);
        reply.attr(&TTL, &attr);
    }

    fn write(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        contents: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyWrite,
    ) {
        // Look up the inode
        let attr = match self.inodes.get(&ino) {
            Some(attr) => attr,
            None => {
                reply.error(ENOENT);
                return;
            }
        };

        // Only allow writing to regular files
        if attr.kind != FileType::RegularFile {
            reply.error(libc::EISDIR);
            return;
        }

        // Make request to server
        let write_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Get the stored path for this inode
                let path = self.paths.get(&ino)
                    .ok_or_else(|| anyhow::anyhow!("Path not found for inode {}", ino))?
                    .clone();

                let result = self.write_file(&path, offset as u64, contents).await;
                result
            })
        });

        match write_result {
            Ok(_) => reply.written(contents.len() as u32),
            Err(e) => {
                warn!("Failed to write file: {}", e);
                reply.error(libc::EIO);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    rustls::crypto::ring::default_provider().install_default().unwrap();
    let opts = Opts::parse();

    info!("Mounting QuicFS at {} with server {}", opts.mountpoint, opts.server);

    // Ensure mount point exists
    if !std::path::Path::new(&opts.mountpoint).exists() {
        info!("Creating mount point directory");
        std::fs::create_dir_all(&opts.mountpoint)?;
    }

    info!("Initializing QuicFS...");
    let fs = QuicFS::new(opts.server).await?;
    info!("QuicFS initialized successfully");

    info!("Attempting to mount filesystem...");
    match fuser::mount2(
        fs,
        &opts.mountpoint,
        &[
            MountOption::FSName("quicfs".to_string()),
            MountOption::AutoUnmount,
            MountOption::AllowOther,
            MountOption::DefaultPermissions,
            MountOption::Exec,
            MountOption::RW,
        ],
    ) {
        Ok(_) => {
            info!("Filesystem mounted successfully");
            // The mount call is blocking, so we'll only get here after unmounting
            info!("Filesystem unmounted");
            Ok(())
        }
        Err(e) => {
            tracing::error!("Failed to mount filesystem: {}", e);
            Err(e.into())
        }
    }
}
