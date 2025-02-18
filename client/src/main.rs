use anyhow::Result;
use clap::Parser;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, 
    ReplyEntry, Request as FuseRequest, TimeOrNow,
};
use libc::ENOENT;
use std::ffi::OsStr;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use quicfs_common::types::DirList;
use futures::future;
use http::Request;
use bytes::Buf;
use quinn::rustls::{self, pki_types, client::danger};

const TTL: Duration = Duration::from_secs(1);

#[derive(Parser)]
struct Opts {
    /// Mount point for the filesystem
    #[clap(short, long)]
    mountpoint: String,
    
    /// Server URL (e.g., https://localhost:4433)
    #[clap(short, long)]
    server: String,
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
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
        let req = Request::builder()
            .method("PATCH")
            .uri(format!("{}/file{}", self.server_url, path))
            .header("Content-Range", format!("bytes {}-{}/{}", 
                offset, 
                offset + (contents.len() as u64).saturating_sub(1),
                offset + contents.len() as u64))
            .body(())?;

        self.ensure_connected().await?;
        let mut stream = self.send_request.as_mut().unwrap().send_request(req).await?;
        stream.send_data(bytes::Bytes::copy_from_slice(contents)).await?;
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
    }

    async fn read_file(&mut self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>> {
        let req = Request::builder()
            .method("GET")
            .uri(format!("{}/file{}", self.server_url, path))
            .header("Range", format!("bytes={}-{}", offset, offset + size as u64 - 1))
            .body(())?;

        self.ensure_connected().await?;
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
    }

    async fn list_directory(&mut self, path: &str) -> Result<DirList> {
        let req = Request::builder()
            .method("GET")
            .uri(format!("{}/dir{}", self.server_url, path))
            .body(())?;

        self.ensure_connected().await?;
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

        let dir_list: DirList = serde_json::from_slice(&body)?;
        Ok(dir_list)
    }

    async fn connect(&self) -> Result<h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>> {
        info!("Connecting to server...");
        
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

        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto_config)?
        ));
        endpoint.set_default_client_config(client_config);

        // Force IPv4 lookup
        let addr = format!("{}:{}", host, port).parse()?;
        let connection = endpoint.connect(addr, host)?.await?;
            
        let h3_conn = h3_quinn::Connection::new(connection);
        let (mut driver, _send_request) = h3::client::new(h3_conn).await?;

        // Spawn the connection driver
        tokio::spawn(async move {
            if let Err(e) = future::poll_fn(|cx| driver.poll_close(cx)).await {
                tracing::error!("Connection driver error: {}", e);
            }
        });

        Ok(send_request)
    }

    async fn ensure_connected(&mut self) -> Result<()> {
        if self.send_request.is_none() {
            self.send_request = Some(self.connect().await?);
        }
        Ok(())
    }

    async fn new(server_url: String) -> Result<Self> {
        info!("Creating new QuicFS instance");
        
        // Parse server URL and connect
        let url = server_url.parse::<http::Uri>()?;
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

        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto_config)?
        ));
        endpoint.set_default_client_config(client_config);

        // Force IPv4 lookup
        let addr = format!("{}:{}", host, port).parse()?;
        let connection = endpoint.connect(addr, host)?.await?;
            
        let h3_conn = h3_quinn::Connection::new(connection);
        let (mut driver, send_request) = h3::client::new(h3_conn).await?;

        // Spawn the connection driver
        tokio::spawn(async move {
            if let Err(e) = future::poll_fn(|cx| driver.poll_close(cx)).await {
                tracing::error!("Connection driver error: {}", e);
            }
        });

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
        Ok(fs)
    }
}

impl Filesystem for QuicFS {
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

        // Create a new inode for the file
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
        let path = format!("/{}", name.to_string_lossy());
        self.paths.insert(ino, path.clone());
        info!("Created inode {} with path {}", ino, path);

        // Create empty file on server
        let create_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let path = format!("/{}", name.to_string_lossy());
                self.write_file(&path, 0, &[]).await
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

    fn lookup(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: ReplyEntry) {
        info!("lookup: {} in {} (pid: {})", name.to_string_lossy(), parent, _req.pid());
        
        // For now, only handle root directory
        if parent != ROOT_INODE {
            info!("lookup: rejecting non-root parent inode {}", parent);
            reply.error(ENOENT);
            return;
        }
        
        // Ignore special directories for now
        if name.to_string_lossy().starts_with('.') {
            reply.error(ENOENT);
            return;
        }
        
        // Make a request to the server to look up the file
        let lookup_result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.list_directory("/").await
            })
        });

        match lookup_result {
            Ok(dir_list) => {
                // Look for the file in the directory listing
                if let Some(entry) = dir_list.entries.iter().find(|e| e.name == name.to_string_lossy()) {
                    // Check if we already have this path mapped to an inode
                    let path = format!("/{}", entry.name);
                    if let Some((&existing_ino, _)) = self.paths.iter().find(|(_, p)| **p == path) {
                        // Reuse existing inode
                        if let Some(attr) = self.inodes.get(&existing_ino) {
                            reply.entry(&TTL, attr, 0);
                            return;
                        }
                    }

                    // If not found, create new inode
                    let file_type = match entry.type_.as_str() {
                        "file" => FileType::RegularFile,
                        "dir" => FileType::Directory,
                        _ => {
                            reply.error(ENOENT);
                            return;
                        }
                    };

                    let ino = self.next_inode;
                    let attr = FileAttr {
                        ino,
                        size: entry.size,
                        blocks: (entry.size + 511) / 512,
                        atime: SystemTime::now(),
                        mtime: SystemTime::now(),
                        ctime: SystemTime::now(),
                        crtime: SystemTime::now(),
                        kind: file_type,
                        perm: entry.mode as u16,
                        nlink: 1,
                        uid: 1000,
                        gid: 1000,
                        rdev: 0,
                        flags: 0,
                        blksize: 512,
                    };

                    // Store both the attributes and the path
                    self.paths.insert(ino, format!("/{}", entry.name));
                    self.inodes.insert(ino, attr);
                    info!("Mapped inode {} to path {}", ino, format!("/{}", entry.name));
                    self.next_inode += 1;
                    reply.entry(&TTL, &attr, 0);
                } else {
                    reply.error(ENOENT);
                }
            }
            Err(e) => {
                warn!("Failed to look up file: {}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn getattr(&mut self, _req: &FuseRequest, ino: u64, reply: ReplyAttr) {
        info!("getattr: {}", ino);
        
        // For root directory, just return cached attributes
        if ino == ROOT_INODE {
            if let Some(attr) = self.inodes.get(&ino) {
                reply.attr(&TTL, attr);
                return;
            }
        }

        // For files, fetch current attributes from server
        let attr_result: Result<FileAttr, anyhow::Error> = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                // Get the stored path for this inode
                let path = self.paths.get(&ino)
                    .ok_or_else(|| anyhow::anyhow!("Path not found for inode {}", ino))?
                    .clone();
                
                // Make request to list the parent directory
                let parent_path = std::path::Path::new(&path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| "/".to_string());
                
                let dir_list = self.list_directory(&parent_path).await?;
                
                // Find the file in the directory listing
                let filename = std::path::Path::new(&path)
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("Invalid path"))?
                    .to_string_lossy();
                
                let entry = dir_list.entries.iter()
                    .find(|e| e.name == filename)
                    .ok_or_else(|| anyhow::anyhow!("File not found in directory listing"))?;

                // Update our cached attributes with the current size
                let mut attr = self.inodes.get(&ino)
                    .ok_or_else(|| anyhow::anyhow!("Inode not found"))?
                    .clone();
                
                attr.size = entry.size;
                attr.blocks = (entry.size + 511) / 512;
                
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
        _size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        info!("read: {} at offset {} size {}", ino, offset, _size);
        
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
                
                self.read_file(&path, offset as u64, _size).await
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
        
        if ino != ROOT_INODE {
            reply.error(ENOENT);
            return;
        }

        // Create entries vector with . and ..
        let mut entries = vec![
            (ROOT_INODE, FileType::Directory, "."),
            (ROOT_INODE, FileType::Directory, ".."),
        ];

        // Fetch directory contents from server and append to entries
        let server_entries = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.list_directory("/").await
            })
        });

        match server_entries {
            Ok(dir_list) => {
                // Collect names into owned strings first
                let file_entries: Vec<(u64, FileType, String)> = dir_list.entries
                    .into_iter()
                    .filter_map(|entry| {
                        let file_type = match entry.type_.as_str() {
                            "file" => FileType::RegularFile,
                            "dir" => FileType::Directory,
                            _ => return None,
                        };
                        Some((self.next_inode, file_type, entry.name))
                    })
                    .collect();

                // Add all entries to our vector
                entries.extend(file_entries.iter().map(|(ino, ft, name)| 
                    (*ino, *ft, name.as_str())
                ));

                // Handle offset and add entries
                for (i, (ino, file_type, name)) in entries.iter().enumerate().skip(offset as usize) {
                    // Reply is full, break the loop
                    if reply.add(*ino, (i + 1) as i64, *file_type, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => {
                warn!("Failed to list directory: {}", e);
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
                    
                    // Make a request to truncate the file
                    let req = Request::builder()
                        .method("PATCH")
                        .uri(format!("{}/file{}", self.server_url, path))
                        .header("Content-Range", format!("bytes */{}", size))
                        .body(())?;

                    self.ensure_connected().await?;
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
        info!("write: {} at offset {} size {}", ino, offset, contents.len());
        info!("Known paths: {:?}", self.paths);
        
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
                // Clone the path string to avoid borrow checker issues
                let path = self.paths.get(&ino)
                    .ok_or_else(|| anyhow::anyhow!("Path not found for inode {}", ino))?
                    .clone();
                
                info!("Writing to path {} at offset {} with {} bytes", path, offset, contents.len());
                let result = self.write_file(&path, offset as u64, contents).await;
                info!("Write result: {:?}", result);
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
    tracing_subscriber::fmt::init();
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
        &[MountOption::FSName("quicfs".to_string())],
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
