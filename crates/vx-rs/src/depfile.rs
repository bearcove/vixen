//! Makefile-style dependency file parser for rustc dep-info
//!
//! Parses the output of `rustc --emit=dep-info` to extract the list of
//! files that were actually read during compilation.
//!
//! ## Format
//!
//! Rustc's dep-info files follow Makefile syntax:
//! ```make
//! target1 target2: dep1 \
//!   dep2 dep3 \
//!   dep4
//! ```
//!
//! With escaping rules:
//! - `\ ` (backslash space) = literal space in filename
//! - `\\` = literal backslash
//! - `\#` = literal hash (though rare in practice)
//! - `\` at end of line = line continuation
//!
//! ## Usage
//!
//! ```no_run
//! use vx_rs::depfile::parse_depfile;
//! use camino::Utf8Path;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let deps = parse_depfile(Utf8Path::new("target/debug/libfoo.d"))?;
//! for dep in deps {
//!     println!("Dependency: {}", dep);
//! }
//! # Ok(())
//! # }
//! ```

use camino::{Utf8Path, Utf8PathBuf};
use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DepfileError {
    #[error("failed to read depfile: {path}: {source}")]
    IoError {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("invalid UTF-8 in depfile: {path}")]
    InvalidUtf8 { path: Utf8PathBuf },
}

/// Normalize a dependency path to workspace-relative form
///
/// This is used after parsing dep-info from a remote execd run.
/// The remote workspace was materialized at `remote_workspace_root`,
/// and we want to convert absolute paths back to workspace-relative.
///
/// # Arguments
///
/// * `dep_path` - Absolute path from dep-info
/// * `remote_workspace_root` - Where the workspace was materialized remotely
///
/// # Returns
///
/// The workspace-relative path if the dep was under the workspace,
/// otherwise returns the original path unchanged (for toolchain files, etc.)
pub fn normalize_dep_path(
    dep_path: &Utf8Path,
    remote_workspace_root: &Utf8Path,
) -> Utf8PathBuf {
    dep_path
        .strip_prefix(remote_workspace_root)
        .map(|p| p.to_owned())
        .unwrap_or_else(|_| dep_path.to_owned())
}

/// Parse a Makefile-style dependency file and return all dependency paths.
///
/// This ignores target names (the part before the colon) and only extracts
/// the dependency list (the part after the colon).
///
/// Returns a deduplicated list of absolute or workspace-relative paths.
pub fn parse_depfile(path: &Utf8Path) -> Result<Vec<Utf8PathBuf>, DepfileError> {
    let content = std::fs::read(path).map_err(|source| DepfileError::IoError {
        path: path.to_owned(),
        source,
    })?;

    let content_str =
        std::str::from_utf8(&content).map_err(|_| DepfileError::InvalidUtf8 {
            path: path.to_owned(),
        })?;

    Ok(parse_depfile_content(content_str))
}

/// Parse the content of a depfile (for testing).
pub fn parse_depfile_content(content: &str) -> Vec<Utf8PathBuf> {
    let mut deps = Vec::new();
    let mut in_deps = false; // false = reading targets, true = reading deps
    let mut current_token = String::new();
    let mut chars = content.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            ':' if !in_deps => {
                // Switch from targets to dependencies
                in_deps = true;
                current_token.clear();
            }
            '\\' => {
                // Escape sequence
                match chars.peek() {
                    Some('\n') => {
                        // Line continuation - consume the newline
                        chars.next();
                        // Skip any leading whitespace on the next line
                        while chars.peek() == Some(&' ') || chars.peek() == Some(&'\t') {
                            chars.next();
                        }
                    }
                    Some(' ') => {
                        // Escaped space - literal space in filename
                        chars.next();
                        current_token.push(' ');
                    }
                    Some('\\') => {
                        // Escaped backslash
                        chars.next();
                        current_token.push('\\');
                    }
                    Some('#') => {
                        // Escaped hash
                        chars.next();
                        current_token.push('#');
                    }
                    _ => {
                        // Unknown escape - just keep the backslash
                        current_token.push('\\');
                    }
                }
            }
            ' ' | '\t' | '\n' => {
                // Whitespace terminates a token
                if in_deps && !current_token.is_empty() {
                    deps.push(Utf8PathBuf::from(current_token.clone()));
                    current_token.clear();
                }
                // If we hit newline and we're reading targets, reset state
                // (in case there are multiple target lines)
                if ch == '\n' {
                    in_deps = false;
                    current_token.clear();
                }
            }
            _ => {
                // Regular character
                current_token.push(ch);
            }
        }
    }

    // Flush final token
    if in_deps && !current_token.is_empty() {
        deps.push(Utf8PathBuf::from(current_token));
    }

    // Deduplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    deps.retain(|dep| seen.insert(dep.clone()));

    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_depfile() {
        let content = "target.o: src/main.rs src/lib.rs";
        let deps = parse_depfile_content(content);

        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0], Utf8PathBuf::from("src/main.rs"));
        assert_eq!(deps[1], Utf8PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn test_line_continuation() {
        let content = "target.o: src/main.rs \\\n  src/lib.rs \\\n  src/utils.rs";
        let deps = parse_depfile_content(content);

        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0], Utf8PathBuf::from("src/main.rs"));
        assert_eq!(deps[1], Utf8PathBuf::from("src/lib.rs"));
        assert_eq!(deps[2], Utf8PathBuf::from("src/utils.rs"));
    }

    #[test]
    fn test_escaped_space() {
        let content = r"target.o: src/my\ file.rs src/other.rs";
        let deps = parse_depfile_content(content);

        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0], Utf8PathBuf::from("src/my file.rs"));
        assert_eq!(deps[1], Utf8PathBuf::from("src/other.rs"));
    }

    #[test]
    fn test_escaped_backslash() {
        let content = r"target.o: src/foo\\bar.rs";
        let deps = parse_depfile_content(content);

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], Utf8PathBuf::from("src/foo\\bar.rs"));
    }

    #[test]
    fn test_multiple_targets() {
        let content = "target1.o target2.o: src/main.rs src/lib.rs";
        let deps = parse_depfile_content(content);

        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0], Utf8PathBuf::from("src/main.rs"));
        assert_eq!(deps[1], Utf8PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn test_deduplication() {
        let content = "target.o: src/main.rs src/lib.rs src/main.rs";
        let deps = parse_depfile_content(content);

        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0], Utf8PathBuf::from("src/main.rs"));
        assert_eq!(deps[1], Utf8PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn test_realistic_rustc_depfile() {
        let content = r"target/debug/libfoo.rmeta: src/lib.rs \
  src/module1.rs \
  src/module2.rs \
  /Users/user/.rustup/toolchains/stable/lib/rustlib/src/rust/library/std/src/lib.rs";

        let deps = parse_depfile_content(content);

        assert_eq!(deps.len(), 4);
        assert_eq!(deps[0], Utf8PathBuf::from("src/lib.rs"));
        assert_eq!(deps[1], Utf8PathBuf::from("src/module1.rs"));
        assert_eq!(deps[2], Utf8PathBuf::from("src/module2.rs"));
        assert!(deps[3].as_str().contains("rustlib"));
    }

    #[test]
    fn test_empty_depfile() {
        let content = "";
        let deps = parse_depfile_content(content);
        assert_eq!(deps.len(), 0);
    }

    #[test]
    fn test_only_targets_no_deps() {
        let content = "target.o:";
        let deps = parse_depfile_content(content);
        assert_eq!(deps.len(), 0);
    }

    #[test]
    fn test_normalize_dep_path_under_workspace() {
        let dep = Utf8Path::new("/tmp/vx-sandbox-12345/src/lib.rs");
        let remote_ws = Utf8Path::new("/tmp/vx-sandbox-12345");

        let normalized = normalize_dep_path(dep, remote_ws);

        assert_eq!(normalized, Utf8PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn test_normalize_dep_path_outside_workspace() {
        let dep = Utf8Path::new("/Users/user/.rustup/toolchains/stable/lib/libstd.rlib");
        let remote_ws = Utf8Path::new("/tmp/vx-sandbox-12345");

        let normalized = normalize_dep_path(dep, remote_ws);

        // Should remain unchanged
        assert_eq!(normalized, dep);
    }
}
