//! VFS server that exposes vixen CAS contents as a filesystem
//!
//! This allows mounting CAS-backed toolchains and source files for hermetic builds.

use fs_kitty_proto::{
    errno, mode, CreateResult, DirEntry, GetAttributesResult, ItemAttributes, ItemId, ItemType,
    LookupResult, ReadDirResult, ReadResult, SetAttributesParams, Vfs, VfsServer, VfsResult,
    WriteResult,
};
use rapace::RpcSession;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;

/// VFS item backed by CAS
#[derive(Clone, Debug)]
struct CasVfsItem {
    id: ItemId,
    parent_id: ItemId,
    name: String,
    item_type: ItemType,
    mode: u32,
    /// For files: CAS blob hash; for directories: None
    cas_hash: Option<String>,
    /// Cached file size
    size: u64,
}

/// VFS that exposes CAS contents
struct CasVfs {
    items: RwLock<HashMap<ItemId, CasVfsItem>>,
    next_id: RwLock<ItemId>,
}

impl CasVfs {
    fn new() -> Self {
        let mut items = HashMap::new();
        let mut next_id = 1;

        // Root directory (id=1)
        items.insert(
            next_id,
            CasVfsItem {
                id: next_id,
                parent_id: 0,
                name: String::new(),
                item_type: ItemType::Directory,
                mode: mode::DIRECTORY,
                cas_hash: None,
                size: 0,
            },
        );
        next_id += 1;

        // /toolchain directory
        items.insert(
            next_id,
            CasVfsItem {
                id: next_id,
                parent_id: 1,
                name: "toolchain".to_string(),
                item_type: ItemType::Directory,
                mode: mode::DIRECTORY,
                cas_hash: None,
                size: 0,
            },
        );
        next_id += 1;

        // /src directory
        items.insert(
            next_id,
            CasVfsItem {
                id: next_id,
                parent_id: 1,
                name: "src".to_string(),
                item_type: ItemType::Directory,
                mode: mode::DIRECTORY,
                cas_hash: None,
                size: 0,
            },
        );
        next_id += 1;

        // Example: /src/main.c
        items.insert(
            next_id,
            CasVfsItem {
                id: next_id,
                parent_id: 3, // /src
                name: "main.c".to_string(),
                item_type: ItemType::File,
                mode: mode::FILE_REGULAR,
                cas_hash: Some("example_hash".to_string()),
                size: 100,
            },
        );
        next_id += 1;

        Self {
            items: RwLock::new(items),
            next_id: RwLock::new(next_id),
        }
    }

    fn get_children(&self, parent_id: ItemId) -> Vec<CasVfsItem> {
        let items = self.items.read().unwrap();
        items
            .values()
            .filter(|item| item.parent_id == parent_id)
            .cloned()
            .collect()
    }
}

impl Vfs for CasVfs {
    async fn lookup(&self, parent_id: ItemId, name: String) -> LookupResult {
        tracing::debug!("lookup(parent={}, name={:?})", parent_id, name);

        let items = self.items.read().unwrap();
        for item in items.values() {
            if item.parent_id == parent_id && item.name == name {
                tracing::debug!("  -> found id={}", item.id);
                return LookupResult {
                    item_id: item.id,
                    item_type: item.item_type,
                    error: errno::OK,
                };
            }
        }

        tracing::debug!("  -> not found");
        LookupResult {
            item_id: 0,
            item_type: ItemType::File,
            error: errno::ENOENT,
        }
    }

    async fn get_attributes(&self, item_id: ItemId) -> GetAttributesResult {
        tracing::debug!("get_attributes({})", item_id);

        let items = self.items.read().unwrap();
        match items.get(&item_id) {
            Some(item) => {
                tracing::debug!("  -> found {:?} (mode={:o})", item.name, item.mode);
                GetAttributesResult {
                    attrs: ItemAttributes {
                        item_id: item.id,
                        item_type: item.item_type,
                        size: item.size,
                        modified_time: 0,
                        created_time: 0,
                        mode: item.mode,
                    },
                    error: errno::OK,
                }
            }
            None => {
                tracing::debug!("  -> not found");
                GetAttributesResult {
                    attrs: ItemAttributes {
                        item_id: 0,
                        item_type: ItemType::File,
                        size: 0,
                        modified_time: 0,
                        created_time: 0,
                        mode: 0,
                    },
                    error: errno::ENOENT,
                }
            }
        }
    }

    async fn read_dir(&self, item_id: ItemId, cursor: u64) -> ReadDirResult {
        tracing::debug!("read_dir({}, cursor={})", item_id, cursor);

        let items = self.items.read().unwrap();
        match items.get(&item_id) {
            Some(item) if item.item_type == ItemType::Directory => {
                let children = self.get_children(item_id);
                let entries: Vec<DirEntry> = children
                    .iter()
                    .skip(cursor as usize)
                    .take(100)
                    .map(|child| DirEntry {
                        name: child.name.clone(),
                        item_id: child.id,
                        item_type: child.item_type,
                    })
                    .collect();

                let has_more = children.len() > (cursor as usize + entries.len());
                let next_cursor = if has_more {
                    cursor + entries.len() as u64
                } else {
                    0
                };

                tracing::debug!("  -> {} entries", entries.len());
                ReadDirResult {
                    entries,
                    next_cursor,
                    error: errno::OK,
                }
            }
            Some(_) => {
                tracing::debug!("  -> not a directory");
                ReadDirResult {
                    entries: Vec::new(),
                    next_cursor: 0,
                    error: errno::ENOTDIR,
                }
            }
            None => {
                tracing::debug!("  -> not found");
                ReadDirResult {
                    entries: Vec::new(),
                    next_cursor: 0,
                    error: errno::ENOENT,
                }
            }
        }
    }

    async fn read(&self, item_id: ItemId, offset: u64, len: u64) -> ReadResult {
        tracing::debug!("read({}, offset={}, len={})", item_id, offset, len);

        let items = self.items.read().unwrap();
        match items.get(&item_id) {
            Some(item) if item.item_type == ItemType::File => {
                // TODO: Fetch from CAS using item.cas_hash
                // For now, return dummy data
                let dummy_data = format!("// File: {}\n// CAS hash: {:?}\n", item.name, item.cas_hash);
                let bytes = dummy_data.as_bytes();

                let start = (offset as usize).min(bytes.len());
                let end = ((offset + len) as usize).min(bytes.len());
                let data = bytes[start..end].to_vec();

                tracing::debug!("  -> {} bytes", data.len());
                ReadResult {
                    data,
                    error: errno::OK,
                }
            }
            Some(_) => {
                tracing::debug!("  -> is a directory");
                ReadResult {
                    data: Vec::new(),
                    error: errno::EISDIR,
                }
            }
            None => {
                tracing::debug!("  -> not found");
                ReadResult {
                    data: Vec::new(),
                    error: errno::ENOENT,
                }
            }
        }
    }

    async fn write(&self, _item_id: ItemId, _offset: u64, _data: Vec<u8>) -> WriteResult {
        // Read-only filesystem
        WriteResult {
            bytes_written: 0,
            error: errno::EACCES,
        }
    }

    async fn create(&self, _parent_id: ItemId, _name: String, _item_type: ItemType) -> CreateResult {
        // Read-only filesystem
        CreateResult {
            item_id: 0,
            error: errno::EACCES,
        }
    }

    async fn delete(&self, _item_id: ItemId) -> VfsResult {
        // Read-only filesystem
        VfsResult {
            error: errno::EACCES,
        }
    }

    async fn rename(&self, _item_id: ItemId, _new_parent_id: ItemId, _new_name: String) -> VfsResult {
        // Read-only filesystem
        VfsResult {
            error: errno::EACCES,
        }
    }

    async fn set_attributes(&self, _item_id: ItemId, _params: SetAttributesParams) -> VfsResult {
        // Read-only filesystem
        VfsResult {
            error: errno::EACCES,
        }
    }

    async fn ping(&self) -> String {
        tracing::debug!("ping()");
        "pong from vixen CAS VFS".to_string()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let addr = "127.0.0.1:10001";
    let listener = TcpListener::bind(addr).await?;

    tracing::info!("=== vixen VFS Server ===");
    tracing::info!("Listening on {}", addr);
    tracing::info!("");
    tracing::info!("CAS-backed filesystem with:");
    tracing::info!("  /toolchain/ - Zig toolchains from CAS");
    tracing::info!("  /src/       - Source files from CAS");
    tracing::info!("");
    tracing::info!("Waiting for connections...");

    let vfs = Arc::new(CasVfs::new());

    loop {
        let (socket, peer_addr) = listener.accept().await?;
        tracing::info!("New connection from {}", peer_addr);

        let vfs = Arc::clone(&vfs);
        tokio::spawn(async move {
            let transport = rapace::Transport::stream(socket);
            let session = Arc::new(RpcSession::new(transport.clone()));

            let vfs_server = VfsServer::new(vfs);
            session.set_dispatcher(vfs_server.into_session_dispatcher(transport));

            if let Err(e) = session.run().await {
                tracing::error!("Connection error from {}: {}", peer_addr, e);
            }
            tracing::info!("Connection from {} closed", peer_addr);
        });
    }
}
