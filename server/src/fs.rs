use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use std::os::unix::fs::PermissionsExt;
use anyhow::Result;
use quicfs_common::types::{DirList, DirEntry};
use tracing::{info, warn};
use memmap2::Mmap;
use std::fs::File;
use http_body::Body as HttpBody;
use std::pin::Pin;
use std::task::{Context, Poll};
use bytes::Bytes;

pub struct MmapBody {
    pub(crate) mmap: Mmap,
    pub(crate) start: usize,
    pub(crate) end: usize,
    position: usize,
}

impl MmapBody {
    pub fn new(mmap: Mmap, start: usize, end: usize) -> Self {
        Self {
            mmap,
            start,
            end,
            position: start,
        }
    }
    
    pub fn full_file(mmap: Mmap) -> Self {
        let len = mmap.len();
        Self::new(mmap, 0, len)
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn end(&self) -> usize {
        self.end
    }

    pub fn get_chunk(&self, position: usize, size: usize) -> &[u8] {
        let end_pos = std::cmp::min(position + size, self.end);
        &self.mmap[position..end_pos]
    }
}

impl HttpBody for MmapBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if self.position >= self.end {
            return Poll::Ready(None);
        }

        // Send data in chunks to avoid large allocations
        const CHUNK_SIZE: usize = 64 * 1024; // 64KB chunks
        let remaining = self.end - self.position;
        let chunk_size = std::cmp::min(remaining, CHUNK_SIZE);
        
        // This is the only copy - from mmap to Bytes
        let chunk = Bytes::copy_from_slice(
            &self.mmap[self.position..self.position + chunk_size]
        );
        
        self.position += chunk_size;
        Poll::Ready(Some(Ok(http_body::Frame::data(chunk))))
    }
}

pub struct FileSystem {
    root: PathBuf,
}

impl FileSystem {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(FileSystem { root })
    }

    pub async fn ensure_root_exists(&self) -> Result<()> {
        if !self.root.exists() {
            fs::create_dir_all(&self.root).await?;
            info!("Created root directory: {:?}", self.root);
        }
        Ok(())
    }

    pub async fn get_file_size(&self, path: &str) -> Result<u64> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        
        let metadata = fs::metadata(&full_path).await?;
        Ok(metadata.len())
    }

    pub async fn list_directory(&self, path: &str) -> Result<DirList> {
        info!("Listing directory: {}", path);
        
        // Normalize the path - remove /dir prefix and handle empty paths
        let clean_path = path.trim_start_matches("/dir").trim_start_matches('/');
        let full_path = if clean_path.is_empty() {
            self.root.clone()
        } else {
            self.root.join(clean_path)
        };
        
        info!("Resolved to filesystem path: {:?}", full_path);
        
        if !full_path.exists() {
            warn!("Path does not exist: {:?}", full_path);
            return Err(anyhow::anyhow!("Directory does not exist: {:?}", full_path));
        }
        if !full_path.is_dir() {
            warn!("Path is not a directory: {:?}", full_path);
            return Err(anyhow::anyhow!("Path is not a directory: {:?}", full_path));
        }
        
        let mut entries = Vec::new();
        let mut dir = fs::read_dir(&full_path).await?;
        
        while let Some(entry) = dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            let file_type = if metadata.is_dir() {
                "dir"
            } else {
                "file"
            };

            let mtime = metadata.modified()?;
            let atime = metadata.accessed()?;
            let ctime = metadata.created()?;

            let entry_name = entry.file_name().to_string_lossy().into_owned();
            // info!("Found entry: {} (type: {})", entry_name, file_type);
            
            entries.push(DirEntry {
                name: entry_name,
                type_: file_type.to_string(),
                size: metadata.len(),
                mode: metadata.permissions().mode() & 0o777,
                mtime: mtime.duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string(),
                atime: atime.duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string(),
                ctime: ctime.duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string(),
            });
        }

        Ok(DirList { entries })
    }

    pub fn read_file_mmap_body(&self, path: &str) -> Result<MmapBody> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        
        let file = File::open(&full_path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(MmapBody::full_file(mmap))
    }

    pub fn read_file_range_mmap_body(&self, path: &str, offset: u64, length: u64) -> Result<MmapBody> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        
        let file = File::open(&full_path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        
        let start = offset as usize;
        let end = std::cmp::min(start + length as usize, mmap.len());
        
        if start >= mmap.len() {
            // Return empty body for out-of-bounds reads
            return Ok(MmapBody::new(mmap, 0, 0));
        }
        
        Ok(MmapBody::new(mmap, start, end))
    }

    pub async fn truncate_file(&self, path: &str, size: u64) -> Result<()> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        
        // Open file and set the new length
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&full_path)
            .await?;
            
        file.set_len(size).await?;
        Ok(())
    }

    pub async fn write_file(&self, path: &str, offset: u64, contents: &[u8]) -> Result<()> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Open file for writing, creating it if it doesn't exist
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&full_path)
            .await?;
        
        // Seek to offset and write contents
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(contents).await?;
        
        Ok(())
    }
}
