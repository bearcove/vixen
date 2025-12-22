//! Common I/O utilities for vertex crates

use camino::Utf8Path;

/// Atomically write contents to a file.
///
/// Creates a temporary file in the same directory, writes contents,
/// then atomically renames to the final path. This ensures the file
/// is never partially written.
pub async fn atomic_write(path: &Utf8Path, contents: &[u8]) -> Result<(), std::io::Error> {
    let parent_dir = path.parent().unwrap_or_else(|| Utf8Path::new("."));

    // Create parent directory if it doesn't exist
    tokio::fs::create_dir_all(parent_dir).await?;

    // Create a temporary file in the same directory to ensure it's on the same filesystem
    let temp_file = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(parent_dir)
        .map_err(std::io::Error::other)?;

    // Get the temporary path and write contents to it
    let temp_path = temp_file.into_temp_path();
    tokio::fs::write(&temp_path, contents).await?;

    // Atomically persist the temporary file to the final location
    temp_path
        .persist(path)
        .map_err(|e| std::io::Error::other(format!("failed to persist temp file: {}", e)))?;

    Ok(())
}

/// Atomically write contents to a file, optionally making it executable.
///
/// Like [`atomic_write`], but also sets the executable bit on Unix if requested.
pub async fn atomic_write_executable(
    path: &Utf8Path,
    contents: &[u8],
    executable: bool,
) -> Result<(), std::io::Error> {
    let parent_dir = path.parent().unwrap_or_else(|| Utf8Path::new("."));

    // Create parent directory if it doesn't exist
    tokio::fs::create_dir_all(parent_dir).await?;

    // Create a temporary file in the same directory
    let temp_file = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(parent_dir)
        .map_err(std::io::Error::other)?;

    let temp_path = temp_file.into_temp_path();
    tokio::fs::write(&temp_path, contents).await?;

    // Set executable bit if requested
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&temp_path).await?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        tokio::fs::set_permissions(&temp_path, perms).await?;
    }

    // Atomically persist
    temp_path
        .persist(path)
        .map_err(|e| std::io::Error::other(format!("failed to persist temp file: {}", e)))?;

    Ok(())
}

/// Synchronous version of atomic write for use in blocking contexts.
pub mod sync {
    use camino::Utf8Path;
    use std::io::Write;

    /// Atomically write contents to a file.
    pub fn atomic_write(path: &Utf8Path, contents: &[u8]) -> Result<(), std::io::Error> {
        let parent_dir = path.parent().unwrap_or_else(|| Utf8Path::new("."));
        std::fs::create_dir_all(parent_dir)?;

        let temp_file = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(parent_dir)
            .map_err(std::io::Error::other)?;

        let (mut file, temp_path) = temp_file.into_parts();
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        temp_path
            .persist(path)
            .map_err(|e| std::io::Error::other(format!("failed to persist temp file: {}", e)))?;

        Ok(())
    }

    /// Atomically write contents to a file, optionally making it executable.
    pub fn atomic_write_executable(
        path: &Utf8Path,
        contents: &[u8],
        executable: bool,
    ) -> Result<(), std::io::Error> {
        let parent_dir = path.parent().unwrap_or_else(|| Utf8Path::new("."));
        std::fs::create_dir_all(parent_dir)?;

        let temp_file = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(parent_dir)
            .map_err(std::io::Error::other)?;

        let (mut file, temp_path) = temp_file.into_parts();
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);

        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&temp_path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            std::fs::set_permissions(&temp_path, perms)?;
        }

        temp_path
            .persist(path)
            .map_err(|e| std::io::Error::other(format!("failed to persist temp file: {}", e)))?;

        Ok(())
    }
}
