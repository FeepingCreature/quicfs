use anyhow::Result;
use clap::Parser;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use libc::ENOENT;
use std::ffi::OsStr;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

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

use std::collections::HashMap;
use std::sync::Arc;
use quicfs_common::types::{DirEntry, DirList};
use http::{Request, Response};

// Temporary certificate verification skip for development
struct SkipServerVerification;

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

const ROOT_INODE: u64 = 1;

struct QuicFS {
    client: h3::client::Connection<h3_quinn::Connection>,
    inodes: HashMap<u64, FileAttr>,
    next_inode: u64,
    server_url: String,
}

impl QuicFS {
    async fn list_directory(&mut self, path: &str) -> Result<DirList> {
        let req = Request::builder()
            .method("GET")
            .uri(format!("{}/dir{}", self.server_url, path))
            .body(())?;

        let mut resp = self.client.send_request(req).await?;
        let (parts, body) = resp.into_parts();
        
        if !parts.status.is_success() {
            return Err(anyhow::anyhow!("Server returned error: {}", parts.status));
        }

        let body = h3::client::read_body(body).await?;
        let dir_list: DirList = serde_json::from_slice(&body)?;
        Ok(dir_list)
    }

    async fn new(server_url: String) -> Result<Self> {
        // Parse server URL and connect
        let url = server_url.parse::<http::Uri>()?;
        let host = url.host().unwrap();
        let port = url.port_u16().unwrap_or(4433);
        
        let client_config = quinn::ClientConfig::new(Arc::new(quinn::rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth()));

        let endpoint = quinn::Endpoint::client("[::]:0".parse()?)?;
        let connection = endpoint.connect_with(client_config, (host, port), host)?
            .await?;
            
        let h3_conn = h3_quinn::Connection::new(connection);
        let client = h3::client::Connection::new(h3_conn).await?;

        let mut fs = QuicFS {
            client,
            inodes: HashMap::new(),
            next_inode: 2,  // 1 is reserved for root
            server_url,
        };
        
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
        fs
    }
}

impl Filesystem for QuicFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        info!("lookup: {} in {}", name.to_string_lossy(), parent);
        
        // For now, only handle root directory
        if parent != ROOT_INODE {
            reply.error(ENOENT);
            return;
        }
        
        // Ignore special directories for now
        if name.to_string_lossy().starts_with('.') {
            reply.error(ENOENT);
            return;
        }
        
        // TODO: Actually look up files/directories from server
        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        info!("getattr: {}", ino);
        
        match self.inodes.get(&ino) {
            Some(attr) => reply.attr(&TTL, attr),
            None => reply.error(ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        _size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        warn!("read: {} at offset {}", ino, offset);
        reply.error(ENOENT);
    }

    fn readdir(
        &mut self,
        _req: &Request,
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

        // Standard directory entries
        let mut idx = 1;
        if offset == 0 {
            reply.add(ROOT_INODE, idx, FileType::Directory, ".");
            idx += 1;
            reply.add(ROOT_INODE, idx, FileType::Directory, "..");
            idx += 1;
        }

        // Fetch directory contents from server
        let entries = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.list_directory("/").await
            })
        });

        match entries {
            Ok(dir_list) => {
                for entry in dir_list.entries {
                    let file_type = match entry.type_.as_str() {
                        "file" => FileType::RegularFile,
                        "dir" => FileType::Directory,
                        _ => continue,
                    };
                    reply.add(self.next_inode + idx, idx as i64, file_type, entry.name);
                    idx += 1;
                }
                reply.ok();
            }
            Err(e) => {
                warn!("Failed to list directory: {}", e);
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
    
    let fs = QuicFS::new(opts.server).await?;
    
    fuser::mount2(
        fs,
        opts.mountpoint,
        &[MountOption::RO, MountOption::FSName("quicfs".to_string())],
    )?;
    
    Ok(())
}
