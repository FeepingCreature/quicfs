use std::path::PathBuf;
use tokio::fs;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, AsyncReadExt};
use std::os::unix::fs::PermissionsExt;
use anyhow::Result;
use quicfs_common::types::{DirList, DirEntry};
use tracing::{info, warn};

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

    pub async fn list_directory(&self, path: &str) -> Result<DirList> {
        info!("Listing directory: {}", path);
        // Handle directory paths
        let clean_path = path.trim_start_matches("/dir").trim_start_matches('/');
        let full_path = if clean_path.is_empty() {
            self.root.clone()
        } else {
            self.root.join(clean_path)
        };
        let mut entries = Vec::new();

        if !full_path.exists() {
            warn!("Path does not exist: {:?}", full_path);
            return Err(anyhow::anyhow!("Directory does not exist: {:?}", full_path));
        }
        if !full_path.is_dir() {
            warn!("Path is not a directory: {:?}", full_path);
            return Err(anyhow::anyhow!("Path is not a directory: {:?}", full_path));
        }
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
            info!("Found entry: {} (type: {})", entry_name, file_type);
            
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

    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        info!("Reading file: {:?}", full_path);
        fs::read(&full_path).await.map_err(Into::into)
    }

    pub async fn read_file_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        
        let mut file = fs::File::open(&full_path).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        
        let mut buffer = vec![0u8; length as usize];
        let bytes_read = file.read(&mut buffer).await?;
        buffer.truncate(bytes_read);
        
        Ok(buffer)
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
