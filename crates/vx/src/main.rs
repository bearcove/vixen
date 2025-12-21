use eyre::Result;
use facet::Facet;
use facet_args as args;

/// vx - Build execution engine with deterministic caching and introspection
#[derive(Facet, Debug)]
struct Cli {
    /// Show version information
    #[facet(args::named, args::short = 'V')]
    version: bool,

    /// Command to run
    #[facet(args::subcommand)]
    command: Command,
}

#[derive(Facet, Debug)]
#[repr(u8)]
enum Command {
    /// Execute a build with caching and explain any rebuilds
    Build {
        /// Build in release mode
        #[facet(args::named, args::short = 'r')]
        release: bool,

        /// Package to build
        #[facet(default, args::named, args::short = 'p')]
        package: Option<String>,

        /// Build all packages in the workspace
        #[facet(args::named)]
        workspace: bool,

        /// Additional arguments to pass to cargo
        #[facet(default, args::positional)]
        cargo_args: Vec<String>,
    },

    /// Explain why targets were rebuilt
    Explain {
        /// Show details for a specific package
        #[facet(default, args::positional)]
        package: Option<String>,
    },

    /// Inspect or manage the cache
    Cache {
        /// Cache action to perform
        #[facet(args::subcommand)]
        action: CacheAction,
    },
}

#[derive(Facet, Debug)]
#[repr(u8)]
enum CacheAction {
    /// Show cache statistics
    Stats,

    /// Prune old cache entries
    Prune {
        /// Remove entries older than N days
        #[facet(default, args::named)]
        older_than: Option<u32>,

        /// Dry run - show what would be deleted
        #[facet(args::named, args::short = 'n')]
        dry_run: bool,
    },

    /// Clear the entire cache
    Clear {
        /// Skip confirmation prompt
        #[facet(args::named, args::short = 'y')]
        yes: bool,
    },
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let cli: Cli = args::from_std_args()?;

    if cli.version {
        println!("vx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match cli.command {
        Command::Build {
            release,
            package,
            workspace,
            cargo_args,
        } => {
            cmd_build(release, package, workspace, cargo_args)?;
        }
        Command::Explain { package } => {
            cmd_explain(package)?;
        }
        Command::Cache { action } => match action {
            CacheAction::Stats => cmd_cache_stats()?,
            CacheAction::Prune {
                older_than,
                dry_run,
            } => cmd_cache_prune(older_than, dry_run)?,
            CacheAction::Clear { yes } => cmd_cache_clear(yes)?,
        },
    }

    Ok(())
}

fn cmd_build(
    release: bool,
    package: Option<String>,
    workspace: bool,
    cargo_args: Vec<String>,
) -> Result<()> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    cmd.arg("--message-format=json");

    if release {
        cmd.arg("--release");
    }
    if let Some(ref pkg) = package {
        cmd.arg("-p").arg(pkg);
    }
    if workspace {
        cmd.arg("--workspace");
    }
    for arg in &cargo_args {
        cmd.arg(arg);
    }

    // Enable fingerprint tracing
    cmd.env("CARGO_LOG", "cargo::core::compiler::fingerprint=trace");

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    eprintln!(
        "Running: cargo build{}",
        if release { " --release" } else { "" }
    );

    let output = cmd.output()?;

    // Parse JSON messages from stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Process fingerprint logs and JSON messages
    let rebuild_reasons = parse_fingerprint_logs(&stderr);
    let artifacts = parse_cargo_json(&stdout);

    // Display results
    display_build_results(&artifacts, &rebuild_reasons);

    if !output.status.success() {
        // Show compiler errors
        eprintln!("{}", stderr);
        std::process::exit(output.status.code().unwrap_or(1));
    }

    Ok(())
}

#[derive(Debug)]
struct RebuildReason {
    package: String,
    target: String,
    reason: String,
    changed_file: Option<String>,
}

/// Cargo JSON message types
mod cargo_messages {
    use facet::Facet;

    #[derive(Facet, Debug)]
    pub struct CompilerArtifact {
        pub reason: String,
        pub package_id: String,
        pub target: Target,
        pub fresh: bool,
    }

    #[derive(Facet, Debug)]
    pub struct Target {
        pub name: String,
        pub kind: Vec<String>,
        pub src_path: String,
    }

    #[derive(Facet, Debug)]
    pub struct BuildFinished {
        pub reason: String,
        pub success: bool,
    }
}

#[derive(Debug)]
struct Artifact {
    package: String,
    target: String,
    fresh: bool,
}

fn parse_fingerprint_logs(stderr: &str) -> Vec<RebuildReason> {
    let mut reasons = Vec::new();

    for line in stderr.lines() {
        // Look for "stale: changed" lines
        if line.contains("stale: changed") {
            if let Some(file) = extract_quoted_path(line) {
                // Try to extract package info from the prepare_target span
                let (package, target) = extract_package_target(line);
                reasons.push(RebuildReason {
                    package,
                    target,
                    reason: "file changed".to_string(),
                    changed_file: Some(file),
                });
            }
        }
        // Look for "fingerprint error" (first build, no cached artifacts)
        else if line.contains("fingerprint error") {
            let (package, target) = extract_package_target(line);
            reasons.push(RebuildReason {
                package,
                target,
                reason: "no cached fingerprint".to_string(),
                changed_file: None,
            });
        }
        // Look for "fingerprint dirty" with specific reason
        else if line.contains("fingerprint dirty") && line.contains("dirty:") {
            let (package, target) = extract_package_target(line);
            let dirty_reason = extract_dirty_reason(line);
            reasons.push(RebuildReason {
                package,
                target,
                reason: dirty_reason,
                changed_file: None,
            });
        }
    }

    reasons
}

fn extract_quoted_path(line: &str) -> Option<String> {
    // Extract path from: stale: changed "/path/to/file.rs"
    let start = line.find("changed \"")?;
    let rest = &line[start + 9..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_package_target(line: &str) -> (String, String) {
    // Extract from: prepare_target{...package_id=foo v0.1.0...target="bar"}
    let package = line
        .find("package_id=")
        .and_then(|start| {
            let rest = &line[start + 11..];
            rest.split_whitespace().next().map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let target = line
        .find("target=\"")
        .and_then(|start| {
            let rest = &line[start + 8..];
            rest.find('"').map(|end| rest[..end].to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    (package, target)
}

fn extract_dirty_reason(line: &str) -> String {
    // Extract reason from dirty: FsStatusOutdated(...) or similar
    if let Some(start) = line.find("dirty: ") {
        let rest = &line[start + 7..];
        if let Some(paren) = rest.find('(') {
            return rest[..paren].to_string();
        }
        return rest
            .split_whitespace()
            .next()
            .unwrap_or("unknown")
            .to_string();
    }
    "unknown".to_string()
}

fn parse_cargo_json(stdout: &str) -> Vec<Artifact> {
    let mut artifacts = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // Only try to parse lines that look like compiler-artifact messages
        if !line.contains("\"reason\":\"compiler-artifact\"") {
            continue;
        }

        match facet_json::from_str::<cargo_messages::CompilerArtifact>(line) {
            Ok(msg) => {
                // Extract package name from package_id (e.g., "path+file:///...#name@version")
                let package = msg
                    .package_id
                    .split('#')
                    .last()
                    .and_then(|s| s.split('@').next())
                    .unwrap_or("unknown")
                    .to_string();

                artifacts.push(Artifact {
                    package,
                    target: msg.target.name,
                    fresh: msg.fresh,
                });
            }
            Err(_) => {
                // Skip malformed lines
                continue;
            }
        }
    }

    artifacts
}

fn display_build_results(artifacts: &[Artifact], reasons: &[RebuildReason]) {
    use owo_colors::OwoColorize;

    let fresh_count = artifacts.iter().filter(|a| a.fresh).count();
    let rebuilt_count = artifacts.iter().filter(|a| !a.fresh).count();

    if rebuilt_count == 0 && !artifacts.is_empty() {
        println!("{} All {} targets fresh", "✓".green(), artifacts.len());
        return;
    }

    if rebuilt_count > 0 {
        println!(
            "\n{} {} rebuilt, {} fresh\n",
            "●".yellow(),
            rebuilt_count,
            fresh_count
        );

        // Show what was rebuilt and why
        for artifact in artifacts.iter().filter(|a| !a.fresh) {
            println!("  {} {}", "→".cyan(), artifact.target.bold());

            // Find matching reason
            let matching_reasons: Vec<_> = reasons
                .iter()
                .filter(|r| r.target == artifact.target || r.package.contains(&artifact.target))
                .collect();

            if matching_reasons.is_empty() {
                println!("    {}", "reason unknown".dimmed());
            } else {
                for reason in matching_reasons {
                    if let Some(ref file) = reason.changed_file {
                        println!("    {} {}", reason.reason.yellow(), file.dimmed());
                    } else {
                        println!("    {}", reason.reason.yellow());
                    }
                }
            }
        }
        println!();
    }
}

fn cmd_explain(_package: Option<String>) -> Result<()> {
    println!("explain command not yet implemented");
    Ok(())
}

fn cmd_cache_stats() -> Result<()> {
    println!("cache stats not yet implemented");
    Ok(())
}

fn cmd_cache_prune(_older_than: Option<u32>, _dry_run: bool) -> Result<()> {
    println!("cache prune not yet implemented");
    Ok(())
}

fn cmd_cache_clear(_yes: bool) -> Result<()> {
    println!("cache clear not yet implemented");
    Ok(())
}
