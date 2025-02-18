use std::path::PathBuf;
use tokio::fs;
use anyhow::Result;
use quicfs_common::types::{DirList, DirEntry};

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
            println!("Created root directory: {:?}", self.root);
        }
        Ok(())
    }

    pub async fn list_directory(&self, path: &str) -> Result<DirList> {
        let full_path = self.root.join(path.trim_start_matches('/'));
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

            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                type_: file_type.to_string(),
                size: metadata.len(),
                mode: metadata.permissions().mode(),
                mtime: mtime.duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string(),
                atime: atime.duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string(),
                ctime: ctime.duration_since(std::time::UNIX_EPOCH)?.as_secs().to_string(),
            });
        }

        Ok(DirList { entries })
    }

    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let full_path = self.root.join(path.trim_start_matches('/'));
        fs::read(&full_path).await.map_err(Into::into)
    }

    pub async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        let full_path = self.root.join(path.trim_start_matches('/'));
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&full_path, data).await.map_err(Into::into)
    }
}
