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
        ("vx", "./crates/vx"),
        ("vx-oort", "./crates/vx-oort"),
        ("vx-rhea", "./crates/vx-rhea"),
        ("vx-aether", "./crates/vx-aether"),
    ];

    println!("Installing all vertex binaries...\n");

    for (name, path) in &binaries {
        println!("Installing {}...", name);
        let status = Command::new("cargo")
            .arg("install")
            .arg("--path")
            .arg(path)
            .arg("--force") // Overwrite existing installations
            .status()?;

        if !status.success() {
            eprintln!("Failed to install {}", name);
            std::process::exit(1);
        }
        println!("✓ {} installed\n", name);
    }

    println!("All binaries installed successfully!");
    Ok(())
}
