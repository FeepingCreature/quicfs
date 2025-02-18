use std::path::PathBuf;
use tokio::fs;
use std::os::unix::fs::PermissionsExt;
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
        println!("list_directory called with path: {}", path);
        // Handle directory paths
        let clean_path = path.trim_start_matches("/dir").trim_start_matches('/');
        println!("After trimming, clean_path: {:?}", clean_path);
        let full_path = if clean_path.is_empty() {
            println!("Using root path directly");
            self.root.clone()
        } else {
            println!("Joining with root: {} + {}", self.root.display(), clean_path);
            self.root.join(clean_path)
        };
        println!("Full path to list: {:?}", full_path);
        let mut entries = Vec::new();

        println!("Reading directory contents from {:?}...", full_path);
        if !full_path.exists() {
            println!("Path does not exist: {:?}", full_path);
            println!("Current directory: {:?}", std::env::current_dir()?);
            return Err(anyhow::anyhow!("Directory does not exist: {:?}", full_path));
        }
        if !full_path.is_dir() {
            println!("Path is not a directory: {:?}", full_path);
            println!("Metadata: {:?}", full_path.metadata()?);
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
            println!("Found entry: {} (type: {})", entry_name, file_type);
            
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
        println!("Reading file: {:?}", full_path);
        fs::read(&full_path).await.map_err(Into::into)
    }

    pub async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        let clean_path = path.trim_start_matches("/file").trim_start_matches('/');
        let full_path = self.root.join(clean_path);
        println!("Writing file to: {:?}", full_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&full_path, contents).await.map_err(Into::into)
    }
}
