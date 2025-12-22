//! vx - Build execution engine with deterministic caching
//!
//! This is a thin CLI client that talks to the vx daemon.

use camino::Utf8PathBuf;
use eyre::{Result, bail};
use facet::Facet;
use facet_args as args;
use owo_colors::OwoColorize;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

use vx_daemon_proto::{BuildRequest,  DaemonClient};
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
        /// Show all nodes including cache hits
        #[facet(args::named)]
        all: bool,

        /// Show verbose output (full hashes, unchanged deps)
        #[facet(args::named, args::short = 'v')]
        verbose: bool,

        /// Explain only this specific node ID
        #[facet(args::named)]
        node: Option<String>,

        /// Show only fanout analysis
        #[facet(args::named)]
        fanout: bool,

        /// Show detailed input diffs
        #[facet(args::named)]
        inputs: bool,

        /// Show detailed dependency diffs
        #[facet(args::named)]
        deps: bool,

        /// Output in JSON format
        #[facet(args::named)]
        json: bool,

        /// Compare last two builds and show what changed
        #[facet(args::named)]
        diff: bool,

        /// Show details of the last cache miss
        #[facet(args::named)]
        last_miss: bool,
    },
}

fn init_tracing() {
    // Default to info for vx crates, warn for everything else
    // Can be overridden with RUST_LOG env var
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("warn,vx=info,vx_daemon=info,vx_casd=info,vx_toolchain=info,vx_execd=info")
    });

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(false)
        // Show span close events with duration - critical for toolchain downloads
        .with_span_events(FmtSpan::CLOSE)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with sensible defaults and span events for performance monitoring
    init_tracing();

    // Install Miette's graphical error handler for nice CLI diagnostics
    miette::set_hook(Box::new(|_| {
        Box::new(miette::MietteHandlerOpts::new().build())
    }))
    .ok();

    let cli: Cli = args::from_std_args().unwrap_or_else(|e| {
        eprintln!("{:?}", miette::Report::new(e));
        std::process::exit(1);
    });

    if cli.version {
        println!("vx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match cli.command {
        CliCommand::Build { release } => cmd_build(release).await,
        CliCommand::Kill => cmd_kill().await,
        CliCommand::Clean => cmd_clean(),
        CliCommand::Explain {
            all,
            verbose,
            node,
            fanout,
            inputs,
            deps,
            json,
            diff,
            last_miss,
        } => cmd_explain(
            all, verbose, node, fanout, inputs, deps, json, diff, last_miss,
        ),
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

/// Display a hash in short form (first 8 hex chars) unless verbose
fn short_hex(hash: &str, verbose: bool) -> String {
    if verbose || hash.len() <= 8 {
        hash.to_string()
    } else {
        format!("{}…", &hash[..8])
    }
}

/// Connect to the daemon, spawning it if necessary
async fn get_or_spawn_daemon() -> Result<DaemonClient> {
    let endpoint = std::env::var("VX_DAEMON").unwrap_or_else(|_| "127.0.0.1:9001".to_string());

    let backoff_ms = [10, 50, 100, 500, 1000];

    // Try to connect first
    match try_connect_daemon(&endpoint).await {
        Ok(client) => return Ok(client),
        Err(_) => {
            // Spawn daemon
            tracing::info!("Daemon not running, spawning vx-daemon on {}", endpoint);

            std::process::Command::new("vx-daemon")
                .spawn()
                .map_err(|e| eyre::eyre!("failed to spawn vx-daemon: {}", e))?;

            // Retry with backoff
            for (attempt, delay) in backoff_ms.iter().enumerate() {
                tokio::time::sleep(tokio::time::Duration::from_millis(*delay)).await;
                if let Ok(client) = try_connect_daemon(&endpoint).await {
                    tracing::info!("Connected to daemon after {} attempts", attempt + 1);
                    return Ok(client);
                }
            }

            // Final attempt
            try_connect_daemon(&endpoint).await
        }
    }
}

async fn try_connect_daemon(endpoint: &str) -> Result<DaemonClient> {
    use std::sync::Arc;

    let stream = tokio::net::TcpStream::connect(endpoint).await?;
    let transport = rapace::Transport::stream(stream);

    // Create RPC session and client
    let session = Arc::new(rapace::RpcSession::new(transport));
    let client = DaemonClient::new(session.clone());

    // CRITICAL: spawn session.run() in background (rapace requires explicit receive loop)
    tokio::spawn(async move {
        if let Err(e) = session.run().await {
            tracing::error!("Daemon session error: {}", e);
        }
    });

    Ok(client)
}

async fn cmd_build(release: bool) -> Result<()> {
    let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;

    // Connect to daemon (spawning if necessary)
    let daemon = get_or_spawn_daemon().await?;

    let request = BuildRequest {
        project_path: cwd.clone(),
        release,
    };

    // Print building message
    println!("{} ({})", "Building".green().bold(), cwd);

    let result = daemon.build(request).await?;

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
            // Error already contains formatted diagnostic, just print and exit
            eprintln!("{}", error);
            std::process::exit(1);
        }
        bail!("{}", result.message)
    }
}

async fn cmd_kill() -> Result<()> {
    let endpoint = std::env::var("VX_DAEMON").unwrap_or_else(|_| "127.0.0.1:9001".to_string());

    // Try to connect to daemon
    match try_connect_daemon(&endpoint).await {
        Ok(daemon) => {
            println!("{} daemon at {}", "Stopping".green().bold(), endpoint);
            daemon.shutdown().await;
            // Give it a moment to shut down
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            println!("{} daemon stopped", "✓".green().bold());
            Ok(())
        }
        Err(_) => {
            eprintln!("{} daemon not running", "Error:".red().bold());
            std::process::exit(1);
        }
    }
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

fn cmd_explain(
    show_all: bool,
    verbose: bool,
    node_filter: Option<String>,
    fanout_only: bool,
    show_inputs: bool,
    show_deps: bool,
    output_json: bool,
    show_diff: bool,
    show_last_miss: bool,
) -> Result<()> {
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

    // Handle --diff: compare last two builds
    if show_diff {
        let Some(prev_report) = store.load_previous()? else {
            println!(
                "{} Need at least two builds to show diff. Run {} again.",
                "Error:".red().bold(),
                "vx build".cyan()
            );
            return Ok(());
        };
        return cmd_explain_diff(&prev_report, &report);
    }

    // Handle --last-miss: show details of the last cache miss
    if show_last_miss {
        return cmd_explain_last_miss(&report);
    }

    // Load previous report for diffing deps
    let prev_report = store.load_previous().ok().flatten();

    if output_json {
        cmd_explain_json(&report, prev_report.as_ref(), node_filter)
    } else {
        cmd_explain_report(
            &report,
            prev_report.as_ref(),
            show_all,
            verbose,
            node_filter,
            fanout_only,
            show_inputs,
            show_deps,
        )
    }
}

fn cmd_explain_report(
    report: &vx_report::BuildReport,
    prev_report: Option<&vx_report::BuildReport>,
    show_all: bool,
    verbose: bool,
    node_filter: Option<String>,
    fanout_only: bool,
    _show_inputs: bool,
    _show_deps: bool,
) -> Result<()> {
    use std::collections::HashMap;

    // =========================================================================
    // 1. RUN HEADER
    // =========================================================================
    let status = if report.success {
        "SUCCESS".green().bold().to_string()
    } else {
        "FAILED".red().bold().to_string()
    };

    let duration_s = report.duration_ms() as f64 / 1000.0;
    println!(
        "{} {} in {:.2}s",
        "Build:".bold(),
        status,
        duration_s.to_string().dimmed()
    );

    println!("  {} {}", "Workspace:".dimmed(), report.workspace_root);
    println!("  {} {}", "Profile:".dimmed(), report.profile);
    println!("  {} {}", "Target:".dimmed(), report.target_triple);

    // Show toolchains
    if let Some(ref rust_tc) = report.toolchains.rust {
        let version_str = rust_tc.version.as_deref().unwrap_or("unknown");
        let id_short = short_hex(&rust_tc.id, verbose);
        println!("  {} {} ({})", "Rust:".dimmed(), version_str, id_short);
    }
    if let Some(ref zig_tc) = report.toolchains.zig {
        let version_str = zig_tc.version.as_deref().unwrap_or("unknown");
        let id_short = short_hex(&zig_tc.id, verbose);
        println!("  {} {} ({})", "Zig:".dimmed(), version_str, id_short);
    }

    println!();

    // =========================================================================
    // 2. NODE LIST
    // =========================================================================

    // Index previous run's nodes by node_id for dep diffing
    let prev_nodes: HashMap<&str, &vx_report::NodeReport> = prev_report
        .map(|r| r.nodes.iter().map(|n| (n.node_id.as_str(), n)).collect())
        .unwrap_or_default();

    // Handle fanout-only mode
    if fanout_only {
        print_fanout_analysis(report, verbose);
        return Ok(());
    }

    // Separate nodes by outcome
    let mut failures = Vec::new();
    let mut misses = Vec::new();
    let mut hits = Vec::new();

    for node in &report.nodes {
        // Apply node filter if specified
        if let Some(ref filter) = node_filter {
            if &node.node_id != filter {
                continue;
            }
        }

        match &node.cache {
            CacheOutcome::Miss { .. } => {
                if node
                    .invocation
                    .as_ref()
                    .map(|i| i.exit_code != 0)
                    .unwrap_or(false)
                {
                    failures.push(node);
                } else {
                    misses.push(node);
                }
            }
            CacheOutcome::Hit { .. } => hits.push(node),
        }
    }

    // Print failures first
    if !failures.is_empty() {
        println!("{}", "Failures:".red().bold());
        for node in &failures {
            print_node(node, None, verbose);
        }
        println!();
    }

    // Print misses with explanations
    if !misses.is_empty() {
        println!("{}", "=== WHY REBUILT ===".yellow().bold());
        println!();
        for node in &misses {
            let prev_node = prev_nodes.get(node.node_id.as_str()).copied();
            print_node_explanation(node, prev_node, verbose);
        }
        println!();
    }

    // Compute and print fanout
    if !misses.is_empty() {
        print_fanout_analysis(report, verbose);
    }

    // Summary of hits (unless --all)
    if !show_all && !hits.is_empty() {
        println!("{} {} cached", "Cache hits:".green().bold(), hits.len());
    } else if show_all && !hits.is_empty() {
        println!("{}", "Cache hits:".green().bold());
        for node in &hits {
            print_node(node, None, verbose);
        }
    }

    // Error message if failed
    if let Some(error) = &report.error {
        println!();
        println!("{}", "Error:".red().bold());
        for line in error.lines().take(10) {
            println!("  {}", line);
        }
        if error.lines().count() > 10 {
            println!(
                "  {} ({} more lines)",
                "...".dimmed(),
                error.lines().count() - 10
            );
        }
    }

    Ok(())
}

/// Print a node explanation using Lane 2 analysis
fn print_node_explanation(
    node: &vx_report::NodeReport,
    prev_node: Option<&vx_report::NodeReport>,
    verbose: bool,
) {
    use vx_report::NodeRebuildExplanation;

    // Compute explanation
    let Some(explanation) = NodeRebuildExplanation::compute(node, prev_node) else {
        // Cache hit - shouldn't happen in this context
        return;
    };

    let duration_ms = node.timing.execute_ms;
    let duration_str = if duration_ms >= 1000 {
        format!("{:.2}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{}ms", duration_ms)
    };

    // Print node header with observed reason
    println!(
        "  {} ({}) {}",
        node.node_id.cyan().bold(),
        explanation.observed_reason.to_string().yellow(),
        duration_str.dimmed()
    );

    // Print input changes
    if !explanation.input_changes.is_empty() {
        println!("    {}:", "Inputs".dimmed());
        for change in &explanation.input_changes {
            match (&change.old_value, &change.new_value) {
                (Some(old), Some(new)) => {
                    let old_short = short_hex(old, verbose);
                    let new_short = short_hex(new, verbose);
                    println!(
                        "      {} {} → {}",
                        change.label,
                        old_short.dimmed(),
                        new_short
                    );
                }
                (None, Some(new)) => {
                    let new_short = short_hex(new, verbose);
                    println!("      {} {} (added)", change.label, new_short);
                }
                (Some(old), None) => {
                    let old_short = short_hex(old, verbose);
                    println!("      {} {} (removed)", change.label, old_short.dimmed());
                }
                (None, None) => {}
            }
        }
    }

    // Print dependency changes
    if !explanation.dep_changes.is_empty() {
        println!("    {}:", "Dependencies".dimmed());
        for change in &explanation.dep_changes {
            use vx_report::DepChangeKind;

            let crate_id_short = short_hex(&change.crate_id, verbose);

            match &change.change {
                DepChangeKind::Added {
                    rlib_hash,
                    manifest_hash,
                } => {
                    let rlib_short = short_hex(rlib_hash, verbose);
                    let manifest_short = short_hex(manifest_hash, verbose);
                    println!(
                        "      {} (crate {}): rlib {} (manifest {})",
                        change.extern_name.cyan(),
                        crate_id_short.dimmed(),
                        rlib_short,
                        manifest_short.dimmed()
                    );
                }
                DepChangeKind::Removed {
                    rlib_hash,
                    manifest_hash,
                } => {
                    let rlib_short = short_hex(rlib_hash, verbose);
                    let manifest_short = short_hex(manifest_hash, verbose);
                    println!(
                        "      {} (crate {}): {} (removed)",
                        change.extern_name.cyan(),
                        crate_id_short.dimmed(),
                        format!("rlib {} manifest {}", rlib_short, manifest_short).dimmed()
                    );
                }
                DepChangeKind::RlibChanged {
                    old_rlib,
                    new_rlib,
                    old_manifest,
                    new_manifest,
                } => {
                    let old_rlib_short = short_hex(old_rlib, verbose);
                    let new_rlib_short = short_hex(new_rlib, verbose);
                    let old_manifest_short = short_hex(old_manifest, verbose);
                    let new_manifest_short = short_hex(new_manifest, verbose);

                    print!(
                        "      {} (crate {}): rlib {} → {}",
                        change.extern_name.cyan(),
                        crate_id_short.dimmed(),
                        old_rlib_short.dimmed(),
                        new_rlib_short
                    );
                    if verbose || old_manifest != new_manifest {
                        print!(
                            " (manifest {} → {})",
                            old_manifest_short.dimmed(),
                            new_manifest_short
                        );
                    }
                    println!();
                }
                DepChangeKind::ManifestChanged {
                    rlib_hash,
                    old_manifest,
                    new_manifest,
                } => {
                    let rlib_short = short_hex(rlib_hash, verbose);
                    let old_manifest_short = short_hex(old_manifest, verbose);
                    let new_manifest_short = short_hex(new_manifest, verbose);
                    println!(
                        "      {} (crate {}): rlib {} manifest {} → {}",
                        change.extern_name.cyan(),
                        crate_id_short.dimmed(),
                        rlib_short.dimmed(),
                        old_manifest_short.dimmed(),
                        new_manifest_short
                    );
                }
            }
        }
    }

    // If verbose, also show the reported reason from daemon
    if verbose {
        println!(
            "    {} {}",
            "Reported reason:".dimmed(),
            explanation.reported_reason.to_string().dimmed()
        );
    }

    println!();
}

/// Print the old-style node display (for cache hits and failures)
fn print_node(
    node: &vx_report::NodeReport,
    _prev_node: Option<&vx_report::NodeReport>,
    verbose: bool,
) {
    let outcome_str = match &node.cache {
        CacheOutcome::Hit { .. } => "HIT ".green().to_string(),
        CacheOutcome::Miss { reason } => format!("MISS ({})", reason).yellow().to_string(),
    };

    let duration_ms = node.timing.execute_ms;
    let duration_str = if duration_ms >= 1000 {
        format!("{:.2}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{}ms", duration_ms)
    };

    println!(
        "  {} {:<15} {}",
        node.node_id,
        outcome_str,
        duration_str.dimmed()
    );

    if verbose {
        let cache_key_short = short_hex(&node.cache_key, verbose);
        println!("    cache_key: {}", cache_key_short.dimmed());
    }
}

/// Print fanout analysis showing which rebuilds triggered downstream rebuilds
fn print_fanout_analysis(report: &vx_report::BuildReport, verbose: bool) {
    use vx_report::FanoutAnalysis;

    let fanout = FanoutAnalysis::compute(report);

    // Only print if there's actual fanout
    if fanout.fanout_map.is_empty() {
        return;
    }

    println!("{}", "=== FANOUT ===".cyan().bold());
    println!();

    // Sort crate IDs for stable output
    let mut crate_ids: Vec<_> = fanout.fanout_map.keys().collect();
    crate_ids.sort();

    for crate_id in crate_ids {
        let dependents = fanout.get_dependents(crate_id);
        if dependents.is_empty() {
            continue;
        }

        let crate_id_short = short_hex(crate_id, verbose);
        println!("  crate {}:", crate_id_short.cyan());

        for dependent in dependents {
            println!("    ↳ {}", dependent.dimmed());
        }
        println!();
    }
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
    println!("  {} {}", "Run ID:".dimmed(), report.run_id);
    println!("  {} {}", "Profile:".dimmed(), report.profile);
    println!("  {} {}", "Target:".dimmed(), report.target_triple);

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
            println!(
                "  {} ({} more lines)",
                "...".dimmed(),
                error.lines().count() - 10
            );
        }
    }

    Ok(())
}

fn cmd_explain_last_miss(report: &vx_report::BuildReport) -> Result<()> {
    // Find the first miss
    let miss = report
        .nodes
        .iter()
        .find(|n| matches!(n.cache, CacheOutcome::Miss { .. }));

    let Some(node) = miss else {
        println!("{} All nodes hit cache!", "Note:".green().bold());
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
    if diff.flipped_to_miss.is_empty()
        && diff.flipped_to_hit.is_empty()
        && diff.toolchain_changes.is_empty()
    {
        println!();
        println!("{} No significant changes between runs.", "Note:".dimmed());
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

/// Output explanation in JSON format
fn cmd_explain_json(
    report: &vx_report::BuildReport,
    prev_report: Option<&vx_report::BuildReport>,
    node_filter: Option<String>,
) -> Result<()> {
    use serde_json::json;
    use std::collections::HashMap;
    use vx_report::{CacheOutcome, FanoutAnalysis, NodeRebuildExplanation};

    let prev_nodes: HashMap<&str, &vx_report::NodeReport> = prev_report
        .map(|r| r.nodes.iter().map(|n| (n.node_id.as_str(), n)).collect())
        .unwrap_or_default();

    let mut nodes_json = Vec::new();

    for node in &report.nodes {
        // Apply node filter if specified
        if let Some(ref filter) = node_filter {
            if &node.node_id != filter {
                continue;
            }
        }

        let prev_node = prev_nodes.get(node.node_id.as_str()).copied();

        // For cache hits, include minimal info
        if matches!(node.cache, CacheOutcome::Hit { .. }) {
            nodes_json.push(json!({
                "node_id": node.node_id,
                "kind": node.kind,
                "cache": "hit",
                "execute_ms": node.timing.execute_ms,
            }));
            continue;
        }

        // For cache misses, compute explanation
        let explanation = NodeRebuildExplanation::compute(node, prev_node);

        if let Some(expl) = explanation {
            let mut input_changes = Vec::new();
            for change in &expl.input_changes {
                input_changes.push(json!({
                    "label": change.label,
                    "old_value": change.old_value,
                    "new_value": change.new_value,
                }));
            }

            let mut dep_changes = Vec::new();
            for change in &expl.dep_changes {
                use vx_report::DepChangeKind;

                let change_json = match &change.change {
                    DepChangeKind::Added {
                        rlib_hash,
                        manifest_hash,
                    } => json!({
                        "kind": "added",
                        "rlib_hash": rlib_hash,
                        "manifest_hash": manifest_hash,
                    }),
                    DepChangeKind::Removed {
                        rlib_hash,
                        manifest_hash,
                    } => json!({
                        "kind": "removed",
                        "rlib_hash": rlib_hash,
                        "manifest_hash": manifest_hash,
                    }),
                    DepChangeKind::RlibChanged {
                        old_rlib,
                        new_rlib,
                        old_manifest,
                        new_manifest,
                    } => json!({
                        "kind": "rlib_changed",
                        "old_rlib": old_rlib,
                        "new_rlib": new_rlib,
                        "old_manifest": old_manifest,
                        "new_manifest": new_manifest,
                    }),
                    DepChangeKind::ManifestChanged {
                        rlib_hash,
                        old_manifest,
                        new_manifest,
                    } => json!({
                        "kind": "manifest_changed",
                        "rlib_hash": rlib_hash,
                        "old_manifest": old_manifest,
                        "new_manifest": new_manifest,
                    }),
                };

                dep_changes.push(json!({
                    "extern_name": change.extern_name,
                    "crate_id": change.crate_id,
                    "change": change_json,
                }));
            }

            nodes_json.push(json!({
                "node_id": expl.node_id,
                "kind": node.kind,
                "cache": "miss",
                "reported_reason": expl.reported_reason.to_string(),
                "observed_reason": expl.observed_reason.to_string(),
                "execute_ms": node.timing.execute_ms,
                "input_changes": input_changes,
                "dep_changes": dep_changes,
            }));
        }
    }

    // Compute fanout
    let fanout = FanoutAnalysis::compute(report);
    let mut fanout_json = serde_json::Map::new();
    for (crate_id, dependents) in &fanout.fanout_map {
        fanout_json.insert(crate_id.clone(), json!(dependents));
    }

    let output = json!({
        "run_id": report.run_id.to_string(),
        "prev_run_id": prev_report.map(|r| r.run_id.to_string()),
        "workspace_root": report.workspace_root,
        "profile": report.profile,
        "target_triple": report.target_triple,
        "success": report.success,
        "duration_ms": report.duration_ms(),
        "nodes": nodes_json,
        "fanout": fanout_json,
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}
