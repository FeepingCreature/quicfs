use serde::{Deserialize, Serialize};

// Common types and traits shared between client and server
pub mod types {
    use super::*;
    
    #[derive(Debug, Serialize, Deserialize)]
    pub struct DirEntry {
        pub name: String,
        pub type_: String,
        pub size: u64,
        pub mode: u32,
        pub mtime: String,
        pub atime: String,
        pub ctime: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct DirList {
        pub entries: Vec<DirEntry>
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct FileStat {
        pub name: String,
        pub type_: String,
        pub size: u64,
        pub mode: u32,
        pub mtime: String,
        pub atime: String,
        pub ctime: String,
    }
}
