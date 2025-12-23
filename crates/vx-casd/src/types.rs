use std::sync::Arc;

use camino::Utf8PathBuf;
use tokio::sync::Semaphore;

use crate::{RegistryManager, toolchain::ToolchainManager};

/// Oort service implementation
#[derive(Clone)]
pub(crate) struct OortService {
    /// Inner state
    pub(crate) inner: Arc<OortServiceInner>,
}

impl std::ops::Deref for OortService {
    type Target = OortServiceInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Oort service implementation
pub(crate) struct OortServiceInner {
    /// Root directory for CAS storage (typically .vx/oort)
    pub(crate) root: Utf8PathBuf,

    /// VX home directory (typically .vx) - parent of root, used for toolchain spec mappings
    pub(crate) vx_home: Utf8PathBuf,

    /// Toolchain acquisition manager (handles inflight deduplication)
    pub(crate) toolchain_manager: ToolchainManager,

    /// Registry crate acquisition manager (handles inflight deduplication)
    pub(crate) registry_manager: RegistryManager,

    /// Semaphore to limit concurrent download/extract operations
    pub(crate) download_semaphore: Arc<Semaphore>,
}
