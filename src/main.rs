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
    version = "0.2.4",
    about = "Stable Rust script manager with WASM/WASI support",
    long_about = "A high-performance bridge between Cargo front-matter scripts and standalone execution."
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
    /// ⚙️ Build the script into a standalone WASM or WASI module (Stable)
    Build {
        script: PathBuf,
        #[arg(value_enum, default_value_t = BuildTarget::Wasi)]
        target: BuildTarget,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// 🏃 Run the script using the rust-script engine
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
        Commands::Build { script, target, out } => build_script_stable(&script, target, out).await?,
        Commands::Run { script, args } => run_script(&script, args)?,
    }
    Ok(())
}

// --- Core Logic: Build (Stable & Organized) ---

async fn build_script_stable(path: &PathBuf, target: BuildTarget, out: Option<PathBuf>) -> Result<()> {
    let (triple, folder_name) = match target {
        BuildTarget::Wasm => ("wasm32-unknown-unknown", "wasm-mod"),
        BuildTarget::Wasi => ("wasm32-wasip1", "wasm-wasi"),
    };

    // 1. Resolve absolute paths and strip Windows UNC prefix
    let script_abs = strip_unc_prefix(fs::canonicalize(path).context("Failed to find script file")?);
    let script_dir = script_abs.parent().context("Could not determine script directory")?;
    let stem = path.file_stem().unwrap().to_str().unwrap();

    // 2. Setup output directory
    let output_dir = script_dir.join(folder_name);
    fs::create_dir_all(&output_dir).context("Failed to create output directory")?;

    let final_filename = out.unwrap_or_else(|| {
        let mut p = PathBuf::from(stem);
        p.set_extension("wasm");
        p
    });
    let absolute_out = output_dir.join(final_filename);

    // 3. Setup Local Build Directory
    let local_build_dir = script_dir.join(format!(".rsxtk_build_{}_{}", stem, folder_name));
    let src_dir = local_build_dir.join("src");
    
    if local_build_dir.exists() { let _ = fs::remove_dir_all(&local_build_dir); }
    fs::create_dir_all(&src_dir)?;

    // --- FIX STARTS HERE ---
    // 4. READ THE FILE (This defines the 'content' variable)
    let content = fs::read_to_string(&script_abs)
        .with_context(|| format!("Failed to read script at {}", script_abs.display()))?;

    // 5. PARSE MANIFEST (Using .context() to turn Option into Result)
    let (_start, end, doc) = parse_manifest(&content)
        .context("Script is missing the required '---' front-matter block")?;
    // --- FIX ENDS HERE ---

    // 6. Generate Cargo.toml
    let mut manifest = doc.clone();
    let mut pkg = toml_edit::Table::new();
    pkg.insert("name", toml_edit::value("wasm_out")); 
    pkg.insert("version", toml_edit::value("0.1.0"));
    pkg.insert("edition", toml_edit::value("2021"));
    manifest.insert("package", toml_edit::Item::Table(pkg));
    
    let mut bin = toml_edit::ArrayOfTables::new();
    let mut target_bin = toml_edit::Table::new();
    target_bin.insert("name", toml_edit::value("wasm_out"));
    target_bin.insert("path", toml_edit::value("src/main.rs"));
    bin.push(target_bin);
    manifest.insert("bin", toml_edit::Item::ArrayOfTables(bin));

    fs::write(local_build_dir.join("Cargo.toml"), manifest.to_string())?;

    // Extract pure Rust source
    let actual_code = match content[end..].find("---") {
        Some(pos) => content[end + pos + 3..].trim_start(),
        None => content[end..].trim_start(),
    };
    fs::write(src_dir.join("main.rs"), actual_code)?;

    println!("⚒️ Compiling {} for {}...", stem, triple);

    // 7. Execute Build with forced target-dir
    let target_dir = local_build_dir.join("target");
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg(triple)
        .arg("--target-dir")
        .arg(&target_dir)
        .current_dir(&local_build_dir)
        .status()?;

    if status.success() {
        let release_dir = target_dir.join(triple).join("release");
        
        // Find the wasm file
        let wasm_src = fs::read_dir(&release_dir)
            .context("Could not read release directory")?
            .filter_map(|entry| entry.ok())
            .map(|e| e.path())
            .find(|p| p.extension().map_or(false, |ext| ext == "wasm"))
            .context("No .wasm file found in release directory")?;

        // 8. Copy to the organized folder
        fs::copy(&wasm_src, &absolute_out)
            .with_context(|| format!("Failed to copy to {}", absolute_out.display()))?;
            
        println!("✨ Success! Created: {}/{}", folder_name, absolute_out.file_name().unwrap().to_string_lossy());
        
        let _ = fs::remove_dir_all(local_build_dir);
    } else {
        anyhow::bail!("Cargo build failed.");
    }

    Ok(())
}

// --- Logic: Run ---

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
    
    if fs::read_to_string(&temp_path).map(|old| old != script_body).unwrap_or(true) {
        fs::write(&temp_path, script_body)?;
    }

    Command::new("rust-script")
        .args(&dep_flags)
        .arg(&temp_path)
        .args(args)
        .status()
        .context("Make sure 'rust-script' is installed (cargo install rust-script)")?;

    Ok(())
}

// --- Manifest Helpers ---

fn parse_manifest(content: &str) -> Option<(usize, usize, DocumentMut)> {
    if !content.starts_with("---") { return None; }
    let first_line_end = content.find('\n')?;
    let start_pos = first_line_end + 1;
    let end_marker_relative = content[start_pos..].find("---")?;
    let end_pos = start_pos + end_marker_relative;
    let doc = content[start_pos..end_pos].parse::<DocumentMut>().ok()?;
    Some((start_pos, end_pos, doc))
}

async fn add_dep(path: &PathBuf, name: &str, features: Vec<String>) -> Result<()> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let (start, end, mut doc) = parse_manifest(&content).unwrap_or_else(|| (0, 0, DocumentMut::new()));
    let deps = doc.entry("dependencies").or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    
    if features.is_empty() {
        deps[name] = toml_edit::value("*");
    } else {
        let mut table = toml_edit::InlineTable::default();
        table.insert("version", toml_edit::Value::from("*"));
        let mut feats = toml_edit::Array::default();
        for f in features { feats.push(f); }
        table.insert("features", toml_edit::Value::from(feats));
        deps[name] = toml_edit::value(toml_edit::Value::InlineTable(table));
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

// --- Utility Helpers ---

fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    let path_str = path.to_string_lossy();
    if path_str.starts_with(r"\\?\") {
        PathBuf::from(&path_str[4..])
    } else {
        path
    }
}

fn check_updates() {
    let informer = update_informer::new(registry::Crates, "rsxtk", env!("CARGO_PKG_VERSION"));
    if let Some(new_version) = informer.check_version().ok().flatten() {
        println!("🚀 New version available: {}", new_version);
    }
}