use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::{value, DocumentMut, Item};
use update_informer::{Check, registry};

#[derive(Parser)]
#[command(name = "rsx", version, about = "Standard-compliant Cargo Script manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add a dependency to the script's --- manifest
    Add {
        script: PathBuf,
        dependency: String,
        #[arg(short, long, value_delimiter = ',')]
        features: Vec<String>,
    },
    /// Remove a dependency from the script
    #[command(alias = "rm")]
    Remove { script: PathBuf, dependency: String },
    /// List all dependencies in the script
    #[command(alias = "ls")]
    List { script: PathBuf },
    /// Format the script using rustfmt
    Fmt { script: PathBuf },
    /// Run any #[test] blocks in the script
    Test { script: PathBuf },
    /// Compile the script to WASM or WASI
    Build {
        script: PathBuf,
        #[arg(value_enum, default_value_t = BuildTarget::Wasi)]
        target: BuildTarget,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Run the script via rust-script engine
    Run {
        script: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(ValueEnum, Clone)]
enum BuildTarget { Wasm, Wasi }

#[tokio::main]
async fn main() -> Result<()> {
    check_updates();
    let cli = Cli::parse();

    match cli.command {
        Commands::Add { script, dependency, features } => add_dep(&script, &dependency, features).await?,
        Commands::Remove { script, dependency } => remove_dep(&script, &dependency)?,
        Commands::List { script } => list_deps(&script)?,
        Commands::Fmt { script } => format_script(&script)?,
        Commands::Test { script } => test_script(&script)?,
        Commands::Build { script, target, out } => build_script(&script, target, out).await?,
        Commands::Run { script, args } => run_script(&script, args)?,
    }
    Ok(())
}

// --- Manifest Logic ---

fn parse_manifest(content: &str) -> Option<(usize, usize, DocumentMut)> {
    if !content.starts_with("---\n") { return None; }
    let lines: Vec<&str> = content.lines().collect();
    let end_idx = lines.iter().skip(1).position(|&l| l.trim() == "---")? + 1;
    let toml_str = lines[1..end_idx].join("\n");
    let doc = toml_str.parse::<DocumentMut>().ok()?;
    let start_byte = lines[0].len() + 1;
    let end_byte = lines[..end_idx].join("\n").len();
    Some((start_byte, end_byte, doc))
}

async fn get_latest_version(name: &str) -> String {
    let client = reqwest::Client::builder().user_agent("rsx-cli").build().ok();
    if let Some(c) = client {
        if let Ok(resp) = c.get(format!("https://crates.io/api/v1/crates/{}", name)).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                return json["crate"]["max_stable_version"].as_str().unwrap_or("*").to_string();
            }
        }
    }
    "*".to_string()
}

// --- Commands ---

async fn add_dep(path: &PathBuf, name: &str, features: Vec<String>) -> Result<()> {
    let content = fs::read_to_string(path).unwrap_or_default();
    println!("🔍 Searching Crates.io for {}...", name);
    let version = get_latest_version(name).await;
    let (start, end, mut doc) = parse_manifest(&content).unwrap_or((0, 0, DocumentMut::new()));

    let mut dep_item = Item::Value(toml_edit::Value::from(version.clone()));
    if !features.is_empty() {
        let mut table = toml_edit::InlineTable::default();
        table.insert("version", version.into());
        let mut feats = toml_edit::Array::default();
        for f in features { feats.push(f); }
        table.insert("features", value(feats));
        dep_item = value(table);
    }
    doc["dependencies"][name] = dep_item;

    let mut final_content = content.clone();
    if start == 0 {
        final_content = format!("---\n{}---\n\n{}", doc, content);
    } else {
        final_content.replace_range(start..end, &doc.to_string());
    }
    fs::write(path, final_content)?;
    println!("✅ Added {} v{} to {}", name, version, path.display());
    Ok(())
}

fn run_script(path: &PathBuf, args: Vec<String>) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let mut dep_flags = Vec::new();
    if let Some((_, _, doc)) = parse_manifest(&content) {
        if let Some(deps) = doc.get("dependencies").and_then(|d| d.as_table()) {
            for (name, item) in deps.iter() {
                dep_flags.push("--dep".to_string());
                let spec = match item {
                    Item::Value(toml_edit::Value::String(s)) => format!("{}={}", name, s.value()),
                    _ => {
                        let v = item.get("version").and_then(|i| i.as_str()).unwrap_or("*");
                        let f = item.get("features").and_then(|i| i.as_array())
                            .map(|a| a.iter().map(|v| v.as_str().unwrap_or("")).collect::<Vec<_>>().join(","))
                            .unwrap_or_default();
                        if f.is_empty() { format!("{}={}", name, v) } else { format!("{}:{}/{}", name, v, f) }
                    }
                };
                dep_flags.push(spec);
            }
        }
    }
    Command::new("rust-script").args(dep_flags).arg(path).args(args).status()?;
    Ok(())
}

async fn build_script(path: &PathBuf, target: BuildTarget, out: Option<PathBuf>) -> Result<()> {
    let triple = match target {
        BuildTarget::Wasm => "wasm32-unknown-unknown",
        BuildTarget::Wasi => "wasm32-wasip1",
    };
    Command::new("rustup").args(["target", "add", triple]).status()?;
    let status = Command::new("cargo").args(["+nightly", "-Zscript", "build", "--release", "--target", triple]).arg(path).status()?;
    if status.success() {
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let wasm_src = format!("target/{}/release/{}.wasm", triple, stem);
        let wasm_out = out.unwrap_or_else(|| PathBuf::from(format!("{}.wasm", stem)));
        fs::copy(wasm_src, &wasm_out)?;
        println!("✨ Compiled to: {}", wasm_out.display());
    }
    Ok(())
}

fn test_script(path: &PathBuf) -> Result<()> {
    Command::new("cargo").args(["+nightly", "-Zscript", "test"]).arg(path).status()?;
    Ok(())
}

fn remove_dep(path: &PathBuf, name: &str) -> Result<()> {
    let content = fs::read_to_string(path)?;
    if let Some((start, end, mut doc)) = parse_manifest(&content) {
        doc.get_mut("dependencies").and_then(|d| d.as_table_mut()).map(|t| t.remove(name));
        let mut final_content = content.clone();
        final_content.replace_range(start..end, &doc.to_string());
        fs::write(path, final_content)?;
        println!("🗑️ Removed {} from {}", name, path.display());
    }
    Ok(())
}

fn list_deps(path: &PathBuf) -> Result<()> {
    let content = fs::read_to_string(path)?;
    if let Some((_, _, doc)) = parse_manifest(&content) {
        println!("📦 Dependencies in {}:", path.display());
        if let Some(deps) = doc.get("dependencies").and_then(|d| d.as_table()) {
            for (n, v) in deps.iter() { println!("  ├── {:<15} : {}", n, v); }
        }
    }
    Ok(())
}

fn format_script(path: &PathBuf) -> Result<()> {
    Command::new("rustfmt").arg(path).status()?;
    Ok(())
}

fn check_updates() {
    let informer = update_informer::new(registry::Crates, "rsx", env!("CARGO_PKG_VERSION"));
    if let Some(new_version) = informer.check_version().ok().flatten() {
        println!("🚀 New version available: {} -> {}", env!("CARGO_PKG_VERSION"), new_version);
    }
}