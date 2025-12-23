use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: cargo xtask <command>");
        eprintln!("\nAvailable commands:");
        eprintln!("  install    Install all project binaries");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "install" => install_all(),
        cmd => {
            eprintln!("Unknown command: {}", cmd);
            std::process::exit(1);
        }
    }
}

fn install_all() -> Result<(), Box<dyn std::error::Error>> {
    let binaries = vec![
        "vx",
        "vx-oort",
        "vx-rhea",
        "vx-aether",
    ];

    println!("Building all vixen binaries in release mode...\n");

    // Build all binaries in one go
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("-p")
        .arg("vx")
        .arg("-p")
        .arg("vx-oort")
        .arg("-p")
        .arg("vx-rhea")
        .arg("-p")
        .arg("vx-aether")
        .status()?;

    if !status.success() {
        eprintln!("Failed to build binaries");
        std::process::exit(1);
    }

    println!("✓ Build completed\n");

    // Determine cargo bin directory
    let cargo_bin = dirs::home_dir()
        .ok_or("Could not determine home directory")?
        .join(".cargo")
        .join("bin");

    // Create .cargo/bin if it doesn't exist
    if !cargo_bin.exists() {
        fs::create_dir_all(&cargo_bin)?;
    }

    let release_dir = PathBuf::from("target/release");

    println!("Installing binaries to {}...\n", cargo_bin.display());

    for binary_name in &binaries {
        let src = release_dir.join(binary_name);

        if !src.exists() {
            eprintln!("Warning: {} not found in target/release", binary_name);
            continue;
        }

        let dst = cargo_bin.join(binary_name);

        fs::copy(&src, &dst)?;
        println!("✓ {} installed", binary_name);
    }

    println!("\nAll binaries installed successfully!");
    Ok(())
}
