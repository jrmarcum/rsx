use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use toml_edit::DocumentMut;
use update_informer::{Check, registry};

#[derive(Parser)]
#[command(
    name = "rsxtk",
    version,
    about = "High-performance manager for Cargo-standard Rust scripts",
    long_about = "rsxtk bridges the gap between Cargo's RFC 3502 (front-matter) and rust-script's speed.",
    help_template = "{about-section}\n\nVersion: {version}\n\nUSAGE:\n  {usage}\n\nCOMMANDS:\n{subcommands}\n\nOPTIONS:\n{options}"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// ➕ Add a dependency to the script's front-matter
    Add {
        script: PathBuf,
        dependency: String,
        #[arg(short, long, value_delimiter = ',')]
        features: Vec<String>,
    },
    /// 🗑️ Remove a dependency from the script
    #[command(alias = "rm")]
    Remove { script: PathBuf, dependency: String },
    /// 📦 List all dependencies defined in the script
    #[command(alias = "ls")]
    List { script: PathBuf },
    /// ✨ Format the script code using rustfmt
    Fmt { script: PathBuf },
    /// 🧪 Run #[test] blocks inside the script
    Test { script: PathBuf },
    /// ⚙️ Build the script into a standalone WebAssembly (WASI) module
    Build {
        script: PathBuf,
        #[arg(value_enum, default_value_t = BuildTarget::Wasi)]
        target: BuildTarget,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// 🏃 Run the script using the rust-script engine (Fast execution)
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

// --- Manifest Parsing ---

fn parse_manifest(content: &str) -> Option<(usize, usize, DocumentMut)> {
    if !content.starts_with("---") { return None; }
    let first_line_end = content.find('\n')?;
    let start_pos = first_line_end + 1;
    let end_marker_relative = content[start_pos..].find("---")?;
    let end_pos = start_pos + end_marker_relative;
    
    let toml_str = &content[start_pos..end_pos];
    let doc = toml_str.parse::<DocumentMut>().ok()?;
    Some((start_pos, end_pos, doc))
}

async fn get_latest_version(name: &str) -> String {
    let client = reqwest::Client::builder()
        .user_agent("rsxtk-cli")
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok();
    
    if let Some(c) = client {
        if let Ok(resp) = c.get(format!("https://crates.io/api/v1/crates/{}", name)).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                return json["crate"]["max_stable_version"].as_str().unwrap_or("*").to_string();
            }
        }
    }
    "*".to_string()
}

// --- Commands Implementation ---

async fn add_dep(path: &PathBuf, name: &str, features: Vec<String>) -> Result<()> {
    let content = fs::read_to_string(path).unwrap_or_default();
    println!("🔍 Searching Crates.io for {}...", name);
    let version = get_latest_version(name).await;
    
    let (start, end, mut doc) = parse_manifest(&content).unwrap_or_else(|| (0, 0, DocumentMut::new()));
    let deps = doc.entry("dependencies").or_insert(toml_edit::Item::Table(toml_edit::Table::new()));

    if features.is_empty() {
        deps[name] = toml_edit::value(version);
    } else {
        let mut table = toml_edit::InlineTable::default();
        table.insert("version", toml_edit::Value::from(version));
        let mut feats = toml_edit::Array::default();
        for f in features { feats.push(f); }
        table.insert("features", toml_edit::Value::from(feats));
        deps[name] = toml_edit::Item::Value(toml_edit::Value::InlineTable(table));
    }

    let mut final_content = content.clone();
    if start == 0 {
        final_content = format!("---\n{}---\n\n{}", doc, content);
    } else {
        final_content.replace_range(start..end, &doc.to_string());
    }
    
    fs::write(path, final_content)?;
    println!("✅ Added {} to {}", name, path.display());
    Ok(())
}

fn run_script(path: &PathBuf, args: Vec<String>) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let mut dep_flags = Vec::new();
    let mut script_body = content.clone();

    if let Some((_start, end, doc)) = parse_manifest(&content) {
        if let Some(deps) = doc.get("dependencies").and_then(|d| d.as_table()) {
            for (name, item) in deps.iter() {
                dep_flags.push("--dep".to_string());
                let spec = match item {
                    toml_edit::Item::Value(toml_edit::Value::String(s)) => format!("{}={}", name, s.value()),
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
        let after = &content[end..];
        if let Some(pos) = after.find("---") {
            script_body = after[pos + 3..].trim_start().to_string();
        }
    }

    let temp_dir = std::env::temp_dir().join("rsxtk_cache");
    fs::create_dir_all(&temp_dir)?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("script");
    let temp_path = temp_dir.join(format!("{}_exec.rs", stem));
    
    let should_write = fs::read_to_string(&temp_path).map(|old| old != script_body).unwrap_or(true);
    if should_write { fs::write(&temp_path, script_body)?; }

    Command::new("rust-script")
        .args(&dep_flags)
        .arg(&temp_path)
        .args(args)
        .status()
        .context("Failed to run rust-script")?;

    Ok(())
}

async fn build_script(path: &PathBuf, target: BuildTarget, out: Option<PathBuf>) -> Result<()> {
    let triple = match target {
        BuildTarget::Wasm => "wasm32-unknown-unknown",
        BuildTarget::Wasi => "wasm32-wasip1",
    };

    println!("⚒️ Compiling {} for {}...", path.display(), triple);

    // FIX: Using --manifest-path ensures cargo recognizes the script file correctly.
    let status = Command::new("cargo")
        .arg("+nightly")
        .arg("-Zscript")
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(path) 
        .arg("--target")
        .arg(triple)
        .env("RUSTFLAGS", "-Z crate-attr=\"feature(frontmatter)\"")
        .status()?;

    if status.success() {
        let stem = path.file_stem().unwrap().to_str().unwrap();
        // The output binary is located in target/[triple]/release/[stem].wasm
        let wasm_src = format!("target/{}/release/{}.wasm", triple, stem);
        let final_out = out.unwrap_or_else(|| {
            let mut p = path.clone();
            p.set_extension("wasm");
            PathBuf::from(p.file_name().unwrap())
        });

        fs::copy(&wasm_src, &final_out)?;
        println!("✨ WASM/WASI module created: {}", final_out.display());
    } else {
        anyhow::bail!("Build failed.");
    }

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
    if let Some((_s, _e, doc)) = parse_manifest(&content) {
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

fn test_script(path: &PathBuf) -> Result<()> {
    // FIX: Also use --manifest-path for tests to ensure consistency
    Command::new("cargo")
        .args(["+nightly", "-Zscript", "test", "--manifest-path"])
        .arg(path)
        .status()?;
    Ok(())
}

fn check_updates() {
    let informer = update_informer::new(registry::Crates, "rsxtk", env!("CARGO_PKG_VERSION"));
    if let Some(new_version) = informer.check_version().ok().flatten() {
        println!("🚀 New version available: {} -> {}", env!("CARGO_PKG_VERSION"), new_version);
    }
}