//! VFS implementation backed by CAS
//!
//! This module implements the fs-kitty VFS trait, providing a virtual filesystem
//! that serves content from the Content-Addressed Storage (CAS).
//!
//! # Architecture
//!
//! The VFS is mounted once at `~/.rhea/` and serves multiple action prefixes:
//!
//! ```text
//! ~/.rhea/                        <- single persistent mount
//!   ├── build-01JFX.../           <- action prefix (sandboxed)
//!   │   ├── toolchain/            <- Zig/Rust toolchain from CAS
//!   │   │   └── bin/cc
//!   │   ├── src/                  <- source tree from CAS
//!   │   │   └── main.c
//!   │   └── out/                  <- outputs (written back to CAS)
//!   │       └── main.o
//!   └── build-01JFY.../           <- another action
//! ```
//!
//! Each action gets its own prefix directory. The sandbox (wardstone) restricts
//! the subprocess to only access its prefix.
//!
//! # Item ID Allocation
//!
//! - ID 1: Root directory (`/`)
//! - ID 2+: Dynamically allocated for prefixes and their contents

use fs_kitty_proto::{
    errno, mode, CreateResult, DirEntry, GetAttributesResult, ItemAttributes, ItemId, ItemType,
    LookupResult, ReadDirResult, ReadResult, SetAttributesParams, Vfs, VfsResult, WriteResult,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use vx_cass_proto::{Blake3Hash, CassClient};

/// A virtual filesystem item
#[derive(Debug, Clone)]
pub struct VfsItem {
    pub id: ItemId,
    pub parent_id: ItemId,
    pub name: String,
    pub item_type: ItemType,
    pub mode: u32,
    /// For files: CAS blob hash (None means empty/new file)
    pub cas_hash: Option<Blake3Hash>,
    /// For files: cached size (0 if unknown)
    pub size: u64,
    /// For files being written: buffered data
    pub write_buffer: Option<Vec<u8>>,
}

/// An action prefix - represents a build directory like `/build-01JFX.../`
#[derive(Debug)]
#[allow(dead_code)] // Fields used for debugging and future prefix management
pub struct ActionPrefix {
    /// Unique ID for this prefix (e.g., ULID)
    pub id: String,
    /// The ItemId of this prefix's root directory
    pub root_item_id: ItemId,
    /// Mapping of paths within prefix to CAS hashes
    /// e.g., "toolchain/bin/cc" -> Blake3Hash
    pub path_mappings: HashMap<String, PrefixEntry>,
}

/// An entry in a prefix - either a file or directory
#[derive(Debug, Clone)]
pub struct PrefixEntry {
    pub item_type: ItemType,
    /// For files: the CAS blob hash
    pub cas_hash: Option<Blake3Hash>,
    /// For files: size in bytes
    pub size: u64,
    /// File mode (permissions)
    pub mode: u32,
}

/// CAS-backed VFS implementation
pub struct CasVfs {
    /// CAS client for fetching blobs
    cas: Arc<CassClient>,
    /// All items in the VFS (keyed by ItemId)
    items: RwLock<HashMap<ItemId, VfsItem>>,
    /// Active action prefixes (keyed by prefix ID like "build-01JFX...")
    prefixes: RwLock<HashMap<String, ActionPrefix>>,
    /// Next available ItemId
    next_id: AtomicU64,
}

impl CasVfs {
    /// Create a new CAS-backed VFS
    pub fn new(cas: Arc<CassClient>) -> Self {
        let mut items = HashMap::new();

        // Create root directory (ID 1)
        items.insert(
            1,
            VfsItem {
                id: 1,
                parent_id: 0, // root has no parent
                name: String::new(),
                item_type: ItemType::Directory,
                mode: mode::DIRECTORY,
                cas_hash: None,
                size: 0,
                write_buffer: None,
            },
        );

        Self {
            cas,
            items: RwLock::new(items),
            prefixes: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(2), // 1 is root
        }
    }

    /// Allocate a new ItemId
    fn alloc_id(&self) -> ItemId {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Create a new action prefix
    ///
    /// Returns the prefix ID (e.g., "build-01JFX...") and the root ItemId
    pub async fn create_prefix(&self, prefix_id: String) -> (String, ItemId) {
        let prefix_root_id = self.alloc_id();

        // Create the prefix directory under root
        {
            let mut items = self.items.write().await;
            items.insert(
                prefix_root_id,
                VfsItem {
                    id: prefix_root_id,
                    parent_id: 1, // under root
                    name: prefix_id.clone(),
                    item_type: ItemType::Directory,
                    mode: mode::DIRECTORY,
                    cas_hash: None,
                    size: 0,
                    write_buffer: None,
                },
            );
        }

        // Register the prefix
        {
            let mut prefixes = self.prefixes.write().await;
            prefixes.insert(
                prefix_id.clone(),
                ActionPrefix {
                    id: prefix_id.clone(),
                    root_item_id: prefix_root_id,
                    path_mappings: HashMap::new(),
                },
            );
        }

        tracing::info!(prefix_id = %prefix_id, item_id = prefix_root_id, "created action prefix");
        (prefix_id, prefix_root_id)
    }

    /// Add a file to a prefix from CAS
    ///
    /// `path` is relative to the prefix root, e.g., "toolchain/bin/cc"
    pub async fn add_file_to_prefix(
        &self,
        prefix_id: &str,
        path: &str,
        cas_hash: Blake3Hash,
        size: u64,
        executable: bool,
    ) -> Option<ItemId> {
        let file_mode = if executable {
            mode::FILE_EXECUTABLE
        } else {
            mode::FILE_REGULAR
        };

        self.add_entry_to_prefix(
            prefix_id,
            path,
            PrefixEntry {
                item_type: ItemType::File,
                cas_hash: Some(cas_hash),
                size,
                mode: file_mode,
            },
        )
        .await
    }

    /// Add a directory to a prefix
    pub async fn add_dir_to_prefix(&self, prefix_id: &str, path: &str) -> Option<ItemId> {
        self.add_entry_to_prefix(
            prefix_id,
            path,
            PrefixEntry {
                item_type: ItemType::Directory,
                cas_hash: None,
                size: 0,
                mode: mode::DIRECTORY,
            },
        )
        .await
    }

    /// Internal: add an entry (file or dir) to a prefix
    async fn add_entry_to_prefix(
        &self,
        prefix_id: &str,
        path: &str,
        entry: PrefixEntry,
    ) -> Option<ItemId> {
        // Get the prefix root
        let prefix_root_id = {
            let prefixes = self.prefixes.read().await;
            prefixes.get(prefix_id)?.root_item_id
        };

        // Split path into components and create intermediate directories
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if components.is_empty() {
            return None;
        }

        let mut current_parent = prefix_root_id;

        // Create intermediate directories
        for (i, component) in components.iter().enumerate() {
            let is_last = i == components.len() - 1;

            // Check if this component already exists
            let existing = {
                let items = self.items.read().await;
                items
                    .values()
                    .find(|item| item.parent_id == current_parent && item.name == *component)
                    .map(|item| (item.id, item.item_type))
            };

            if let Some((existing_id, existing_type)) = existing {
                if is_last {
                    // This is the target - already exists
                    return Some(existing_id);
                } else if existing_type == ItemType::Directory {
                    current_parent = existing_id;
                    continue;
                } else {
                    // Path component is a file, can't traverse
                    return None;
                }
            }

            // Create the component
            let new_id = self.alloc_id();
            let (item_type, mode, cas_hash, size) = if is_last {
                (entry.item_type, entry.mode, entry.cas_hash, entry.size)
            } else {
                (ItemType::Directory, mode::DIRECTORY, None, 0)
            };

            {
                let mut items = self.items.write().await;
                items.insert(
                    new_id,
                    VfsItem {
                        id: new_id,
                        parent_id: current_parent,
                        name: component.to_string(),
                        item_type,
                        mode,
                        cas_hash,
                        size,
                        write_buffer: None,
                    },
                );
            }

            if is_last {
                // Update prefix mappings
                let mut prefixes = self.prefixes.write().await;
                if let Some(prefix) = prefixes.get_mut(prefix_id) {
                    prefix.path_mappings.insert(path.to_string(), entry);
                }
                return Some(new_id);
            }

            current_parent = new_id;
        }

        None
    }

    /// Remove an action prefix and all its contents
    pub async fn remove_prefix(&self, prefix_id: &str) {
        // Get items to remove
        let items_to_remove: Vec<ItemId> = {
            let prefixes = self.prefixes.read().await;
            let Some(prefix) = prefixes.get(prefix_id) else {
                return;
            };

            let items = self.items.read().await;
            // Collect all items under this prefix (including the prefix root)
            self.collect_descendants(&items, prefix.root_item_id)
        };

        // Remove items
        {
            let mut items = self.items.write().await;
            for id in items_to_remove {
                items.remove(&id);
            }
        }

        // Remove prefix registration
        {
            let mut prefixes = self.prefixes.write().await;
            prefixes.remove(prefix_id);
        }

        tracing::info!(prefix_id = %prefix_id, "removed action prefix");
    }

    /// Collect all descendant ItemIds (including the item itself)
    fn collect_descendants(&self, items: &HashMap<ItemId, VfsItem>, root_id: ItemId) -> Vec<ItemId> {
        let mut result = vec![root_id];
        let mut queue = vec![root_id];

        while let Some(parent_id) = queue.pop() {
            for item in items.values() {
                if item.parent_id == parent_id {
                    result.push(item.id);
                    if item.item_type == ItemType::Directory {
                        queue.push(item.id);
                    }
                }
            }
        }

        result
    }

}

impl Vfs for CasVfs {
    async fn lookup(&self, parent_id: ItemId, name: String) -> LookupResult {
        tracing::trace!(parent_id, name = %name, "lookup");

        let items = self.items.read().await;
        for item in items.values() {
            if item.parent_id == parent_id && item.name == name {
                tracing::trace!(item_id = item.id, "lookup found");
                return LookupResult {
                    item_id: item.id,
                    item_type: item.item_type,
                    error: errno::OK,
                };
            }
        }

        tracing::trace!("lookup: not found");
        LookupResult {
            item_id: 0,
            item_type: ItemType::File,
            error: errno::ENOENT,
        }
    }

    async fn get_attributes(&self, item_id: ItemId) -> GetAttributesResult {
        tracing::trace!(item_id, "get_attributes");

        let items = self.items.read().await;
        match items.get(&item_id) {
            Some(item) => GetAttributesResult {
                attrs: ItemAttributes {
                    item_id: item.id,
                    item_type: item.item_type,
                    size: item.size,
                    modified_time: 0,
                    created_time: 0,
                    mode: item.mode,
                },
                error: errno::OK,
            },
            None => GetAttributesResult {
                attrs: ItemAttributes {
                    item_id: 0,
                    item_type: ItemType::File,
                    size: 0,
                    modified_time: 0,
                    created_time: 0,
                    mode: 0,
                },
                error: errno::ENOENT,
            },
        }
    }

    async fn read_dir(&self, item_id: ItemId, cursor: u64) -> ReadDirResult {
        tracing::trace!(item_id, cursor, "read_dir");

        let items = self.items.read().await;
        match items.get(&item_id) {
            Some(item) if item.item_type == ItemType::Directory => {
                let children: Vec<_> = items
                    .values()
                    .filter(|i| i.parent_id == item_id)
                    .collect();

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

                ReadDirResult {
                    entries,
                    next_cursor,
                    error: errno::OK,
                }
            }
            Some(_) => ReadDirResult {
                entries: Vec::new(),
                next_cursor: 0,
                error: errno::ENOTDIR,
            },
            None => ReadDirResult {
                entries: Vec::new(),
                next_cursor: 0,
                error: errno::ENOENT,
            },
        }
    }

    async fn read(&self, item_id: ItemId, offset: u64, len: u64) -> ReadResult {
        tracing::trace!(item_id, offset, len, "read");

        // Get item info
        let (item_type, cas_hash) = {
            let items = self.items.read().await;
            match items.get(&item_id) {
                Some(item) => (item.item_type, item.cas_hash),
                None => {
                    return ReadResult {
                        data: Vec::new(),
                        error: errno::ENOENT,
                    }
                }
            }
        };

        if item_type != ItemType::File {
            return ReadResult {
                data: Vec::new(),
                error: errno::EISDIR,
            };
        }

        // Fetch from CAS if we have a hash
        let Some(hash) = cas_hash else {
            // Empty file
            return ReadResult {
                data: Vec::new(),
                error: errno::OK,
            };
        };

        // Fetch blob from CAS
        match self.cas.get_blob(hash).await {
            Ok(Some(data)) => {
                let start = (offset as usize).min(data.len());
                let end = ((offset + len) as usize).min(data.len());
                ReadResult {
                    data: data[start..end].to_vec(),
                    error: errno::OK,
                }
            }
            Ok(None) => {
                tracing::warn!(hash = %hash, "blob not found in CAS");
                ReadResult {
                    data: Vec::new(),
                    error: errno::EIO,
                }
            }
            Err(e) => {
                tracing::error!(hash = %hash, error = %e, "failed to fetch blob from CAS");
                ReadResult {
                    data: Vec::new(),
                    error: errno::EIO,
                }
            }
        }
    }

    async fn write(&self, item_id: ItemId, offset: u64, data: Vec<u8>) -> WriteResult {
        tracing::trace!(item_id, offset, len = data.len(), "write");

        let mut items = self.items.write().await;
        let Some(item) = items.get_mut(&item_id) else {
            return WriteResult {
                bytes_written: 0,
                error: errno::ENOENT,
            };
        };

        if item.item_type != ItemType::File {
            return WriteResult {
                bytes_written: 0,
                error: errno::EISDIR,
            };
        }

        // Initialize or extend write buffer
        let buffer = item.write_buffer.get_or_insert_with(Vec::new);
        let end = offset as usize + data.len();

        // Extend buffer if needed
        if buffer.len() < end {
            buffer.resize(end, 0);
        }

        // Copy data into buffer
        buffer[offset as usize..end].copy_from_slice(&data);
        item.size = buffer.len() as u64;

        WriteResult {
            bytes_written: data.len() as u64,
            error: errno::OK,
        }
    }

    async fn create(&self, parent_id: ItemId, name: String, item_type: ItemType) -> CreateResult {
        tracing::trace!(parent_id, name = %name, ?item_type, "create");

        // Verify parent exists and is a directory
        {
            let items = self.items.read().await;
            match items.get(&parent_id) {
                Some(item) if item.item_type == ItemType::Directory => {}
                Some(_) => {
                    return CreateResult {
                        item_id: 0,
                        error: errno::ENOTDIR,
                    }
                }
                None => {
                    return CreateResult {
                        item_id: 0,
                        error: errno::ENOENT,
                    }
                }
            }

            // Check if name already exists
            if items
                .values()
                .any(|i| i.parent_id == parent_id && i.name == name)
            {
                return CreateResult {
                    item_id: 0,
                    error: errno::EEXIST,
                };
            }
        }

        // Create the new item
        let new_id = self.alloc_id();
        let mode = match item_type {
            ItemType::Directory => mode::DIRECTORY,
            ItemType::File => mode::FILE_REGULAR,
            ItemType::Symlink => mode::FILE_REGULAR,
        };

        {
            let mut items = self.items.write().await;
            items.insert(
                new_id,
                VfsItem {
                    id: new_id,
                    parent_id,
                    name,
                    item_type,
                    mode,
                    cas_hash: None,
                    size: 0,
                    write_buffer: None,
                },
            );
        }

        CreateResult {
            item_id: new_id,
            error: errno::OK,
        }
    }

    async fn delete(&self, item_id: ItemId) -> VfsResult {
        tracing::trace!(item_id, "delete");

        // Don't allow deleting root
        if item_id == 1 {
            return VfsResult {
                error: errno::EACCES,
            };
        }

        let mut items = self.items.write().await;

        // Check if item exists
        let Some(item) = items.get(&item_id) else {
            return VfsResult {
                error: errno::ENOENT,
            };
        };

        // If directory, check if empty
        if item.item_type == ItemType::Directory {
            let has_children = items.values().any(|i| i.parent_id == item_id);
            if has_children {
                return VfsResult {
                    error: errno::ENOTEMPTY,
                };
            }
        }

        items.remove(&item_id);

        VfsResult { error: errno::OK }
    }

    async fn rename(&self, item_id: ItemId, new_parent_id: ItemId, new_name: String) -> VfsResult {
        tracing::trace!(item_id, new_parent_id, new_name = %new_name, "rename");

        let mut items = self.items.write().await;

        // Check item exists
        if !items.contains_key(&item_id) {
            return VfsResult {
                error: errno::ENOENT,
            };
        }

        // Check new parent exists and is directory
        match items.get(&new_parent_id) {
            Some(p) if p.item_type == ItemType::Directory => {}
            Some(_) => return VfsResult { error: errno::ENOTDIR },
            None => return VfsResult { error: errno::ENOENT },
        }

        // Check name doesn't exist in new parent
        if items
            .values()
            .any(|i| i.parent_id == new_parent_id && i.name == new_name && i.id != item_id)
        {
            return VfsResult {
                error: errno::EEXIST,
            };
        }

        // Perform rename
        if let Some(item) = items.get_mut(&item_id) {
            item.parent_id = new_parent_id;
            item.name = new_name;
        }

        VfsResult { error: errno::OK }
    }

    async fn set_attributes(&self, item_id: ItemId, params: SetAttributesParams) -> VfsResult {
        tracing::trace!(item_id, ?params, "set_attributes");

        let mut items = self.items.write().await;

        let Some(item) = items.get_mut(&item_id) else {
            return VfsResult {
                error: errno::ENOENT,
            };
        };

        if let Some(mode) = params.mode {
            item.mode = mode;
        }

        // modified_time is ignored for now (we don't track times)

        VfsResult { error: errno::OK }
    }

    async fn ping(&self) -> String {
        "pong from vx-rhea CAS VFS".to_string()
    }
}

/// Flush a file's write buffer to CAS and return the blob hash
impl CasVfs {
    pub async fn flush_to_cas(&self, item_id: ItemId) -> Option<Blake3Hash> {
        let data = {
            let mut items = self.items.write().await;
            let item = items.get_mut(&item_id)?;
            item.write_buffer.take()?
        };

        if data.is_empty() {
            return None;
        }

        match self.cas.put_blob(data).await {
            Ok(hash) => {
                // Update the item's cas_hash
                let mut items = self.items.write().await;
                if let Some(item) = items.get_mut(&item_id) {
                    item.cas_hash = Some(hash);
                }
                Some(hash)
            }
            Err(e) => {
                tracing::error!(item_id, error = %e, "failed to flush to CAS");
                None
            }
        }
    }

    /// Flush all written files in a prefix to CAS
    /// Returns a map of relative paths to their CAS hashes
    pub async fn flush_prefix_to_cas(&self, prefix_id: &str) -> HashMap<String, Blake3Hash> {
        let mut results = HashMap::new();

        // Get prefix root
        let prefix_root_id = {
            let prefixes = self.prefixes.read().await;
            match prefixes.get(prefix_id) {
                Some(p) => p.root_item_id,
                None => return results,
            }
        };

        // Find all items with write buffers under this prefix
        let items_to_flush: Vec<(ItemId, String)> = {
            let items = self.items.read().await;
            let descendants = self.collect_descendants(&items, prefix_root_id);

            descendants
                .into_iter()
                .filter_map(|id| {
                    let item = items.get(&id)?;
                    if item.write_buffer.is_some() {
                        // Build the path from root
                        let path = self.build_path(&items, id, prefix_root_id);
                        Some((id, path))
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Flush each item
        for (item_id, path) in items_to_flush {
            if let Some(hash) = self.flush_to_cas(item_id).await {
                results.insert(path, hash);
            }
        }

        results
    }

    /// Build the path from an item up to (but not including) a root
    fn build_path(&self, items: &HashMap<ItemId, VfsItem>, item_id: ItemId, root_id: ItemId) -> String {
        let mut components = Vec::new();
        let mut current = item_id;

        while current != root_id && current != 0 {
            if let Some(item) = items.get(&current) {
                components.push(item.name.clone());
                current = item.parent_id;
            } else {
                break;
            }
        }

        components.reverse();
        components.join("/")
    }
}
