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
        // For now return dummy data
        Ok(DirList {
            entries: vec![
                DirEntry {
                    name: "test.txt".to_string(),
                    type_: "file".to_string(),
                    size: 42,
                    mode: 0o644,
                    mtime: "2024-02-18T15:04:05Z".to_string(),
                    atime: "2024-02-18T15:04:05Z".to_string(),
                    ctime: "2024-02-18T15:04:05Z".to_string(),
                }
            ]
        })
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
