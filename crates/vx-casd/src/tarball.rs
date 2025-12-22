//! Tarball Validation

/// Validate that a tarball has exactly one top-level directory.
/// Returns the number of components to strip (always 1 for valid tarballs).
///
/// PERF: This decompresses the entire tarball to validate structure.
/// Future optimization: validate during extraction in execd.
pub(crate) fn validate_tarball_structure(tarball_bytes: &[u8]) -> Result<u32, String> {
    use std::io::Read;
    use xz2::read::XzDecoder;

    let mut decoder = XzDecoder::new(tarball_bytes);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("xz decompression failed: {}", e))?;

    let mut archive = tar::Archive::new(decompressed.as_slice());
    let mut top_level_dirs = std::collections::HashSet::new();

    for entry in archive.entries().map_err(|e| format!("tar error: {}", e))? {
        let entry = entry.map_err(|e| format!("tar entry error: {}", e))?;
        let path = entry.path().map_err(|e| format!("path error: {}", e))?;

        // Get the first component
        if let Some(first) = path.components().next() {
            if let std::path::Component::Normal(name) = first {
                top_level_dirs.insert(name.to_string_lossy().to_string());
            }
        }
    }

    if top_level_dirs.len() != 1 {
        return Err(format!(
            "tarball must have exactly one top-level directory, found {}: {:?}",
            top_level_dirs.len(),
            top_level_dirs
        ));
    }

    Ok(1) // strip_components = 1
}
