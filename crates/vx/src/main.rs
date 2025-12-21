//! vx - Build execution engine with deterministic caching
//!
//! This is a thin CLI client that talks to the vx daemon.

use camino::Utf8PathBuf;
use eyre::{Result, bail};
use facet::Facet;
use facet_args as args;
use owo_colors::OwoColorize;
use tracing_subscriber::EnvFilter;

use vx_daemon::DaemonService;
use vx_daemon_proto::{BuildRequest, Daemon};
use vx_report::{CacheOutcome, ReportStore, RunDiff};

/// vx - Build execution engine with deterministic caching
#[derive(Facet, Debug)]
struct Cli {
    /// Show version information
    #[facet(args::named, args::short = 'V')]
    version: bool,

    /// Command to run
    #[facet(args::subcommand)]
    command: CliCommand,
}

#[derive(Facet, Debug)]
#[repr(u8)]
enum CliCommand {
    /// Execute a build
    Build {
        /// Build in release mode
        #[facet(args::named, args::short = 'r')]
        release: bool,
    },

    /// Stop the daemon process
    Kill,

    /// Stop the daemon and remove .vx/ directory
    Clean,

    /// Explain the last build
    Explain {
        /// Show only the first node that missed cache
        #[facet(args::named)]
        last_miss: bool,

        /// Compare against previous run
        #[facet(args::named)]
        diff: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing from RUST_LOG env var (e.g., RUST_LOG=vx_daemon=debug)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli: Cli = args::from_std_args()?;

    if cli.version {
        println!("vx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match cli.command {
        CliCommand::Build { release } => cmd_build(release).await,
        CliCommand::Kill => cmd_kill(),
        CliCommand::Clean => cmd_clean(),
        CliCommand::Explain {
            last_miss,
            diff,
        } => cmd_explain(last_miss, diff),
    }
}

fn get_vx_home() -> Result<Utf8PathBuf> {
    // Check VX_HOME env var first, then fall back to ~/.vx
    if let Ok(vx_home) = std::env::var("VX_HOME") {
        return Ok(Utf8PathBuf::from(vx_home));
    }

    let home = std::env::var("HOME").map_err(|_| eyre::eyre!("HOME not set"))?;
    Ok(Utf8PathBuf::from(home).join(".vx"))
}

async fn cmd_build(release: bool) -> Result<()> {
    let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;

    // For v0, we run the daemon in-process
    // Future: connect to a persistent daemon via rapace
    let vx_home = get_vx_home()?;
    let daemon = DaemonService::new(vx_home)?;

    let request = BuildRequest {
        project_path: cwd.clone(),
        release,
    };

    // Print building message
    println!("{} ({})", "Building".green().bold(), cwd);

    // Load picante cache from disk (if it exists)
    let _ = daemon.load_cache().await;

    let result = daemon.build(request).await;

    if result.success {
        if result.cached {
            println!("{} (cached)", "Finished".green().bold());
        } else {
            println!("{}", "Finished".green().bold());
        }

        if let Some(output_path) = &result.output_path {
            println!("  {} {}", "Binary:".dimmed(), output_path);
        }

        Ok(())
    } else {
        if let Some(error) = &result.error {
            eprintln!("{}", error);
        }
        bail!("{}", result.message)
    }
}

fn cmd_kill() -> Result<()> {
    // For v0, the daemon runs in-process, so there's nothing to kill.
    // When we have a persistent daemon, this will send a shutdown signal.
    println!(
        "{} daemon not running (v0 runs in-process)",
        "Note:".yellow().bold()
    );
    Ok(())
}

fn cmd_clean() -> Result<()> {
    let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;
    let vx_dir = cwd.join(".vx");

    // First, try to kill the daemon (no-op for v0)
    let _ = cmd_kill();

    // Then remove .vx/ if it exists
    if vx_dir.exists() {
        std::fs::remove_dir_all(&vx_dir)?;
        println!("{} {}", "Removed".green().bold(), vx_dir);
    } else {
        println!("{} {} does not exist", "Note:".yellow().bold(), vx_dir);
    }

    Ok(())
}

fn cmd_explain(last_miss: bool, diff: bool) -> Result<()> {
    let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;
    let store = ReportStore::new(&cwd);

    // Load the latest report
    let Some(report) = store.load_latest()? else {
        println!(
            "{} No build reports found. Run {} first.",
            "Error:".red().bold(),
            "vx build".cyan()
        );
        return Ok(());
    };

    if diff {
        // Compare with previous run
        let Some(prev_report) = store.load_previous()? else {
            println!(
                "{} Only one run found, nothing to compare.",
                "Note:".yellow().bold()
            );
            return cmd_explain_summary(&report);
        };

        return cmd_explain_diff(&prev_report, &report);
    }

    if last_miss {
        return cmd_explain_last_miss(&report);
    }

    cmd_explain_summary(&report)
}

fn cmd_explain_summary(report: &vx_report::BuildReport) -> Result<()> {
    // Header
    let status = if report.success {
        "SUCCESS".green().bold().to_string()
    } else {
        "FAILED".red().bold().to_string()
    };

    let duration_ms = report.duration_ms();
    let duration_str = if duration_ms >= 1000 {
        format!("{:.2}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{}ms", duration_ms)
    };

    println!(
        "{} {} in {}",
        "Build:".bold(),
        status,
        duration_str.dimmed()
    );
    println!(
        "  {} {}",
        "Run ID:".dimmed(),
        report.run_id
    );
    println!(
        "  {} {}",
        "Profile:".dimmed(),
        report.profile
    );
    println!(
        "  {} {}",
        "Target:".dimmed(),
        report.target_triple
    );

    if let Some(rust) = &report.toolchains.rust {
        if let Some(version) = &rust.version {
            println!("  {} {}", "Rust:".dimmed(), version);
        }
    }

    // Summary stats
    let hits = report.cache_hits();
    let misses = report.cache_misses();
    let total = hits + misses;

    println!();
    println!(
        "{} {} hit, {} miss ({} total)",
        "Cache:".bold(),
        hits.to_string().green(),
        misses.to_string().yellow(),
        total
    );

    // Node table
    if !report.nodes.is_empty() {
        println!();
        println!("{}", "Nodes:".bold());
        println!(
            "  {:<50} {:<10} {:<12} {}",
            "NODE".dimmed(),
            "CACHE".dimmed(),
            "DURATION".dimmed(),
            "OUTPUT".dimmed()
        );

        for node in &report.nodes {
            let cache_str = match &node.cache {
                CacheOutcome::Hit { .. } => "hit".green().to_string(),
                CacheOutcome::Miss { reason } => format!("miss ({})", reason).yellow().to_string(),
            };

            let duration_str = if node.timing.execute_ms > 0 {
                format!("{}ms", node.timing.execute_ms)
            } else {
                "-".to_string()
            };

            let output_path = node
                .outputs
                .first()
                .and_then(|o| o.path.as_ref())
                .map(|p| p.as_str())
                .unwrap_or("-");

            println!(
                "  {:<50} {:<10} {:<12} {}",
                truncate_node_id(&node.node_id, 50),
                cache_str,
                duration_str,
                output_path.dimmed()
            );
        }
    }

    // Error message if failed
    if let Some(error) = &report.error {
        println!();
        println!("{}", "Error:".red().bold());
        // Show first few lines of error
        for line in error.lines().take(10) {
            println!("  {}", line);
        }
        if error.lines().count() > 10 {
            println!("  {} ({} more lines)", "...".dimmed(), error.lines().count() - 10);
        }
    }

    Ok(())
}

fn cmd_explain_last_miss(report: &vx_report::BuildReport) -> Result<()> {
    // Find the first miss
    let miss = report.nodes.iter().find(|n| matches!(n.cache, CacheOutcome::Miss { .. }));

    let Some(node) = miss else {
        println!(
            "{} All nodes hit cache!",
            "Note:".green().bold()
        );
        return Ok(());
    };

    println!("{}", "First cache miss:".bold());
    println!("  {} {}", "Node:".dimmed(), node.node_id);

    if let CacheOutcome::Miss { reason } = &node.cache {
        println!("  {} {}", "Reason:".dimmed(), reason);
    }

    println!("  {} {}", "Cache key:".dimmed(), &node.cache_key[..16]);

    println!();
    println!("{}", "Inputs:".bold());
    for input in &node.inputs {
        let value_display = if input.value.len() > 16 {
            format!("{}...", &input.value[..16])
        } else {
            input.value.clone()
        };
        println!("  {:<30} {}", input.label, value_display.dimmed());
    }

    Ok(())
}

fn cmd_explain_diff(before: &vx_report::BuildReport, after: &vx_report::BuildReport) -> Result<()> {
    let diff = RunDiff::compute(before, after);

    let before_short = if diff.before.0.len() >= 10 {
        &diff.before.0[..10]
    } else {
        &diff.before.0
    };
    let after_short = if diff.after.0.len() >= 10 {
        &diff.after.0[..10]
    } else {
        &diff.after.0
    };
    println!(
        "{} {} {} {}",
        "Comparing:".bold(),
        before_short.dimmed(),
        "→".dimmed(),
        after_short
    );

    // Toolchain changes
    if !diff.toolchain_changes.is_empty() {
        println!();
        println!("{}", "Toolchain changes:".bold());
        for tc in &diff.toolchain_changes {
            println!(
                "  {} {} → {}",
                tc.name,
                tc.old.as_deref().unwrap_or("(none)").dimmed(),
                tc.new.as_deref().unwrap_or("(none)")
            );
        }
    }

    // Nodes that flipped to miss
    if !diff.flipped_to_miss.is_empty() {
        println!();
        println!("{}", "Nodes that flipped hit → miss:".yellow().bold());
        for node_diff in &diff.flipped_to_miss {
            println!("  {}", node_diff.node_id);

            if !node_diff.changed_inputs.is_empty() {
                println!("    {}:", "Changed inputs".dimmed());
                for (label, old, new) in &node_diff.changed_inputs {
                    let old_short = if old.len() > 12 {
                        format!("{}...", &old[..12])
                    } else {
                        old.clone()
                    };
                    let new_short = if new.len() > 12 {
                        format!("{}...", &new[..12])
                    } else {
                        new.clone()
                    };
                    println!(
                        "      {}: {} → {}",
                        label,
                        old_short.dimmed(),
                        new_short.yellow()
                    );
                }
            }
        }
    }

    // Nodes that flipped to hit
    if !diff.flipped_to_hit.is_empty() {
        println!();
        println!("{}", "Nodes that flipped miss → hit:".green().bold());
        for node_id in &diff.flipped_to_hit {
            println!("  {}", node_id);
        }
    }

    // Summary
    if diff.flipped_to_miss.is_empty() && diff.flipped_to_hit.is_empty() && diff.toolchain_changes.is_empty() {
        println!();
        println!(
            "{} No significant changes between runs.",
            "Note:".dimmed()
        );
    }

    Ok(())
}

/// Truncate a node ID to fit in a column, adding "..." if needed.
fn truncate_node_id(id: &str, max_len: usize) -> String {
    if id.len() <= max_len {
        id.to_string()
    } else {
        format!("{}...", &id[..max_len - 3])
    }
}
