use std::sync::Arc;

use camino::Utf8PathBuf;

use crate::{RegistryManager, toolchain::ToolchainManager};

/// CAS service implementation
#[derive(Clone)]
pub(crate) struct CasService {
    /// Inner state
    pub(crate) inner: Arc<CasServiceInner>,
}

impl std::ops::Deref for CasService {
    type Target = CasServiceInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// CAS service implementation
pub(crate) struct CasServiceInner {
    /// Root directory for CAS storage (typically .vx/cas)
    pub(crate) root: Utf8PathBuf,

    /// Toolchain acquisition manager (handles inflight deduplication)
    pub(crate) toolchain_manager: ToolchainManager,

    /// Registry crate acquisition manager (handles inflight deduplication)
    pub(crate) registry_manager: RegistryManager,
}
