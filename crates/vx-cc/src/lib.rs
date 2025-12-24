//! C/C++ build support for vx
//!
//! This crate provides types and utilities for building C/C++ code
//! using the zig cc hermetic toolchain.

pub mod depfile;

use camino::Utf8PathBuf;
use facet::Facet;
use vx_cass_proto::Blake3Hash;

/// Uniquely identifies a translation unit in the build
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct TuKey {
    /// Target this TU belongs to (e.g., "hello")
    pub target: String,
    /// Source file path (workspace-relative)
    pub source: Utf8PathBuf,
    /// Build profile ("debug" or "release")
    pub profile: String,
    /// Target triple (e.g., "x86_64-linux-musl")
    pub triple: String,
}

impl TuKey {
    pub fn new(
        target: impl Into<String>,
        source: impl Into<Utf8PathBuf>,
        profile: impl Into<String>,
        triple: impl Into<String>,
    ) -> Self {
        Self {
            target: target.into(),
            source: source.into(),
            profile: profile.into(),
            triple: triple.into(),
        }
    }
}

/// A build artifact stored in CAS
#[derive(Clone, Debug)]
pub struct Artifact {
    /// Content hash (blob in CAS)
    pub blob: Blake3Hash,
    /// Logical type ("obj", "exe", "staticlib", "depfile")
    pub logical: String,
    /// Filename to use when materializing
    pub filename: Utf8PathBuf,
    /// Whether to set executable bit
    pub executable: bool,
}

impl Artifact {
    /// Create an object file artifact
    pub fn object(blob: Blake3Hash, filename: impl Into<Utf8PathBuf>) -> Self {
        Self {
            blob,
            logical: "obj".to_string(),
            filename: filename.into(),
            executable: false,
        }
    }

    /// Create an executable artifact
    pub fn executable(blob: Blake3Hash, filename: impl Into<Utf8PathBuf>) -> Self {
        Self {
            blob,
            logical: "exe".to_string(),
            filename: filename.into(),
            executable: true,
        }
    }

    /// Create a depfile artifact
    pub fn depfile(blob: Blake3Hash, filename: impl Into<Utf8PathBuf>) -> Self {
        Self {
            blob,
            logical: "depfile".to_string(),
            filename: filename.into(),
            executable: false,
        }
    }
}

/// Expected output from an action
#[derive(Clone, Debug, Facet)]
pub struct ExpectedOutput {
    /// Logical type ("obj", "exe", "depfile")
    pub logical: String,
    /// Path where execd should find the output (relative to action workdir)
    pub path: Utf8PathBuf,
    /// Whether to set executable bit
    pub executable: bool,
}

/// Invocation for compiling a single translation unit
#[derive(Clone, Debug, Facet)]
pub struct CcCompileInvocation {
    /// Path to zig binary (from toolchain cache)
    pub program: Utf8PathBuf,
    /// Arguments: ["cc", "-c", "src/main.c", "-o", "main.o", ...]
    pub args: Vec<String>,
    /// Environment variables (minimal, controlled)
    pub env: Vec<(String, String)>,
    /// Working directory
    pub cwd: Utf8PathBuf,
    /// Expected outputs
    pub expected_outputs: Vec<ExpectedOutput>,
    /// Depfile path (relative to cwd)
    pub depfile: Option<Utf8PathBuf>,
}

/// Invocation for linking objects into an executable
#[derive(Clone, Debug, Facet)]
pub struct CcLinkInvocation {
    /// Path to zig binary
    pub program: Utf8PathBuf,
    /// Arguments
    pub args: Vec<String>,
    /// Environment variables
    pub env: Vec<(String, String)>,
    /// Working directory
    pub cwd: Utf8PathBuf,
    /// Expected outputs
    pub expected_outputs: Vec<ExpectedOutput>,
}

/// Result of compiling a translation unit
#[derive(Clone, Debug)]
pub enum CcCompileResult {
    /// Compilation succeeded
    Success {
        /// The compiled object file
        object: Artifact,
        /// The depfile (if generated)
        depfile: Option<Artifact>,
        /// Parsed and normalized header dependencies (workspace-relative)
        discovered_deps: Vec<Utf8PathBuf>,
    },
    /// Compilation failed
    Failure {
        /// Exit code from compiler
        exit_code: i32,
        /// Standard error output
        stderr: String,
        /// Standard output
        stdout: String,
    },
}

impl CcCompileResult {
    /// Returns true if compilation succeeded
    pub fn is_success(&self) -> bool {
        matches!(self, CcCompileResult::Success { .. })
    }

    /// Returns the object artifact if successful
    pub fn object(&self) -> Option<&Artifact> {
        match self {
            CcCompileResult::Success { object, .. } => Some(object),
            CcCompileResult::Failure { .. } => None,
        }
    }
}

/// Result of linking
#[derive(Clone, Debug)]
pub enum CcLinkResult {
    /// Linking succeeded
    Success {
        /// The linked executable
        executable: Artifact,
    },
    /// Linking failed
    Failure {
        exit_code: i32,
        stderr: String,
        stdout: String,
    },
}

/// C language standard
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CStandard {
    C89,
    C99,
    #[default]
    C11,
    C17,
    C23,
}

impl CStandard {
    pub fn as_flag(&self) -> &'static str {
        match self {
            CStandard::C89 => "-std=c89",
            CStandard::C99 => "-std=c99",
            CStandard::C11 => "-std=c11",
            CStandard::C17 => "-std=c17",
            CStandard::C23 => "-std=c23",
        }
    }
}

/// C++ language standard
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CxxStandard {
    Cxx11,
    Cxx14,
    #[default]
    Cxx17,
    Cxx20,
    Cxx23,
}

impl CxxStandard {
    pub fn as_flag(&self) -> &'static str {
        match self {
            CxxStandard::Cxx11 => "-std=c++11",
            CxxStandard::Cxx14 => "-std=c++14",
            CxxStandard::Cxx17 => "-std=c++17",
            CxxStandard::Cxx20 => "-std=c++20",
            CxxStandard::Cxx23 => "-std=c++23",
        }
    }
}

/// Compilation flags for C/C++
#[derive(Clone, Debug, Default)]
pub struct CompileFlags {
    /// Include directories (workspace-relative, -I)
    pub include_dirs: Vec<Utf8PathBuf>,
    /// System include directories (-isystem)
    pub system_include_dirs: Vec<Utf8PathBuf>,
    /// Preprocessor defines (-D)
    pub defines: Vec<String>,
    /// Optimization level (0, 1, 2, 3, s, z)
    pub opt_level: Option<String>,
    /// Enable debug info (-g)
    pub debug_info: bool,
    /// Warning flags
    pub warnings: Vec<String>,
    /// Additional raw flags
    pub extra: Vec<String>,
}

impl CompileFlags {
    /// Create flags for debug profile
    pub fn debug() -> Self {
        Self {
            opt_level: Some("0".to_string()),
            debug_info: true,
            warnings: vec!["-Wall".to_string(), "-Wextra".to_string()],
            ..Default::default()
        }
    }

    /// Create flags for release profile
    pub fn release() -> Self {
        Self {
            opt_level: Some("2".to_string()),
            debug_info: false,
            warnings: vec!["-Wall".to_string()],
            ..Default::default()
        }
    }

    /// Convert to command-line arguments
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        for dir in &self.include_dirs {
            args.push("-I".to_string());
            args.push(dir.to_string());
        }

        for dir in &self.system_include_dirs {
            args.push("-isystem".to_string());
            args.push(dir.to_string());
        }

        for def in &self.defines {
            args.push(format!("-D{}", def));
        }

        if let Some(ref level) = self.opt_level {
            args.push(format!("-O{}", level));
        }

        if self.debug_info {
            args.push("-g".to_string());
        }

        for warn in &self.warnings {
            args.push(warn.clone());
        }

        args.extend(self.extra.clone());

        args
    }
}

/// Link flags
#[derive(Clone, Debug, Default)]
pub struct LinkFlags {
    /// Library search directories (-L)
    pub lib_dirs: Vec<Utf8PathBuf>,
    /// Libraries to link (-l)
    pub libs: Vec<String>,
    /// Static libraries (full paths)
    pub static_libs: Vec<Utf8PathBuf>,
    /// Link-time optimization
    pub lto: bool,
    /// Strip symbols
    pub strip: bool,
    /// Additional raw flags
    pub extra: Vec<String>,
}

impl LinkFlags {
    /// Convert to command-line arguments
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        for dir in &self.lib_dirs {
            args.push("-L".to_string());
            args.push(dir.to_string());
        }

        for lib in &self.libs {
            args.push(format!("-l{}", lib));
        }

        for lib in &self.static_libs {
            args.push(lib.to_string());
        }

        if self.lto {
            args.push("-flto".to_string());
        }

        if self.strip {
            args.push("-s".to_string());
        }

        args.extend(self.extra.clone());

        args
    }
}

#[cfg(test)]
mod tests;
