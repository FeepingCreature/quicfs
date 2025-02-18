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

struct QuicFS {
    // TODO: Add HTTP/3 client here
}

impl Filesystem for QuicFS {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        warn!("lookup: {} in {}", name.to_string_lossy(), parent);
        reply.error(ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        warn!("getattr: {}", ino);
        reply.error(ENOENT);
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
        warn!("readdir: {} at offset {}", ino, offset);
        reply.error(ENOENT);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let opts = Opts::parse();
    
    info!("Mounting QuicFS at {} with server {}", opts.mountpoint, opts.server);
    
    let fs = QuicFS {
        // TODO: Initialize HTTP/3 client
    };
    
    fuser::mount2(
        fs,
        opts.mountpoint,
        &[MountOption::RO, MountOption::FSName("quicfs".to_string())],
    )?;
    
    Ok(())
}
