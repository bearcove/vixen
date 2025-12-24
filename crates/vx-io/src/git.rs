//! Git repository utilities

use camino::{Utf8Path, Utf8PathBuf};

/// Find the git repository root by searching for `.git` directory upward from the given path.
///
/// Returns `None` if no git repository is found.
pub async fn find_git_root(start_path: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut current = start_path;

    loop {
        let git_dir = current.join(".git");
        if tokio::fs::try_exists(&git_dir).await.unwrap_or(false) {
            return Some(current.to_path_buf());
        }

        current = current.parent()?;
    }
}

/// Check if `.gitignore` already contains an entry that ignores `.vx`
///
/// This checks for:
/// - `/.vx` (exact match)
/// - `.vx` (matches everywhere)
/// - `/.vx/` (directory-only)
/// - `.vx/` (directory everywhere)
async fn gitignore_contains_vx(gitignore_path: &Utf8Path) -> bool {
    let Ok(content) = tokio::fs::read_to_string(gitignore_path).await else {
        return false;
    };

    for line in content.lines() {
        let trimmed = line.trim();
        // Ignore comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Check if this pattern would ignore .vx
        if trimmed == "/.vx" || trimmed == ".vx" || trimmed == "/.vx/" || trimmed == ".vx/" {
            return true;
        }
    }

    false
}

/// Ensure `.vx` is in `.gitignore` at the git repository root.
///
/// If we're in a git repository and `.gitignore` doesn't already ignore `.vx`,
/// this appends `/.vx` with an explanatory comment.
///
/// Returns `Ok(true)` if the entry was added, `Ok(false)` if not needed,
/// or an error if the operation failed.
pub async fn ensure_vx_gitignored(workspace_root: &Utf8Path) -> Result<bool, std::io::Error> {
    // Find git root
    let Some(git_root) = find_git_root(workspace_root).await else {
        // Not in a git repo, nothing to do
        return Ok(false);
    };

    let gitignore_path = git_root.join(".gitignore");

    // Check if already ignored
    if tokio::fs::try_exists(&gitignore_path).await.unwrap_or(false) {
        if gitignore_contains_vx(&gitignore_path).await {
            return Ok(false);
        }
    }

    // Append entry to .gitignore
    let entry = "\n# vx build system cache (auto-added by vx)\n/.vx\n";

    if tokio::fs::try_exists(&gitignore_path).await.unwrap_or(false) {
        // File exists, append
        let mut content = tokio::fs::read_to_string(&gitignore_path).await?;
        // Ensure there's a trailing newline before our entry
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(entry);
        tokio::fs::write(&gitignore_path, content).await?;
    } else {
        // File doesn't exist, create it
        tokio::fs::write(&gitignore_path, entry.trim_start()).await?;
    }

    Ok(true)
}
