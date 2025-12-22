use camino::Utf8Path;

pub(crate) async fn atomic_write(path: &Utf8Path, contents: &[u8]) -> Result<(), std::io::Error> {
    // Create a temporary file in the same directory as the target path
    let parent_dir = path.parent().unwrap_or_else(|| camino::Utf8Path::new("."));

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
        .map_err(|e| std::io::Error::other(format!("Failed to persist temp file: {}", e)))?;

    Ok(())
}
