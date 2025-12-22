use std::sync::atomic::AtomicU64;

use camino::Utf8PathBuf;

use crate::{RegistryManager, toolchain::ToolchainManager};

/// CAS service implementation
pub(crate) struct CasService {
    /// Root directory for CAS storage (typically .vx/cas)
    pub(crate) root: Utf8PathBuf,

    /// Next upload session ID
    pub(crate) next_upload_id: AtomicU64,

    /// Toolchain acquisition manager (handles inflight deduplication)
    pub(crate) toolchain_manager: ToolchainManager,

    /// Registry crate acquisition manager (handles inflight deduplication)
    pub(crate) registry_manager: RegistryManager,
}
