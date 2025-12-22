fn main() {
    // Test inline table syntax
    let toml1 = r#"
[package]
name = "test"
edition = "2021"

[dependencies]
dioxus = { workspace = true, features = ["router"] }
backtrace = { path = "../..", features = ["std"] }
"#;

    println!("=== Testing inline table syntax ===");
    match facet_toml::from_str::<vx_manifest::full::CargoManifest>(toml1) {
        Ok(manifest) => {
            println!("Successfully parsed!");
            println!("Dependencies: {:#?}", manifest.dependencies);
        }
        Err(e) => {
            eprintln!("Failed to parse: {}", e);
        }
    }

    // Test table header syntax
    let toml2 = r#"
[package]
name = "test"
edition = "2021"

[dependencies.backtrace]
path = "../.."
features = ["std"]

[dependencies.dioxus]
workspace = true
features = ["router"]
"#;

    println!("\n=== Testing table header syntax ===");
    match facet_toml::from_str::<vx_manifest::full::CargoManifest>(toml2) {
        Ok(manifest) => {
            println!("Successfully parsed!");
            println!("Dependencies: {:#?}", manifest.dependencies);
        }
        Err(e) => {
            eprintln!("Failed to parse: {}", e);
        }
    }
}
