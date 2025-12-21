//! vx - Build execution engine with deterministic caching
//!
//! This is a thin CLI client that talks to the vx daemon.

use camino::Utf8PathBuf;
use eyre::{Result, bail};
use facet::Facet;
use facet_args as args;
use owo_colors::OwoColorize;

use vx_daemon::DaemonService;
use vx_daemon_proto::{BuildRequest, Daemon};

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
    Explain,
}

fn main() -> Result<()> {
    let cli: Cli = args::from_std_args()?;

    if cli.version {
        println!("vx {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match cli.command {
        CliCommand::Build { release } => cmd_build(release),
        CliCommand::Kill => cmd_kill(),
        CliCommand::Clean => cmd_clean(),
        CliCommand::Explain => cmd_explain(),
    }
}

fn cmd_build(release: bool) -> Result<()> {
    let cwd = Utf8PathBuf::try_from(std::env::current_dir()?)?;

    // For v0, we run the daemon in-process
    // Future: connect to a persistent daemon via rapace
    let cas_root = cwd.join(".vx/cas");
    let daemon = DaemonService::new(cas_root)?;

    let request = BuildRequest {
        project_path: cwd.clone(),
        release,
    };

    // Print building message
    println!("{} ({})", "Building".green().bold(), cwd);

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(daemon.build(request));

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

fn cmd_explain() -> Result<()> {
    println!("explain command not yet implemented for v0");
    Ok(())
}
