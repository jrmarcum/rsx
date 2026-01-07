use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use toml_edit::DocumentMut;
use update_informer::{Check, registry};

#[derive(Parser)]
#[command(name = "rsxtk", version, about = "Stable-compatible Rust script manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// ➕ Add a dependency
    Add {
        script: PathBuf,
        dependency: String,
        #[arg(short, long, value_delimiter = ',')]
        features: Vec<String>,
    },
    /// 🗑️ Remove a dependency
    Remove { script: PathBuf, dependency: String },
    /// 📦 List dependencies
    List { script: PathBuf },
    /// ⚙️ Build for WASM/WASI (Stable)
    Build {
        script: PathBuf,
        #[arg(value_enum, default_value_t = BuildTarget::Wasi)]
        target: BuildTarget,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// 🏃 Run (Fast execution via rust-script)
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

async fn build_script_stable(path: &PathBuf, target: BuildTarget, out: Option<PathBuf>) -> Result<()> {
    let (triple, folder_name) = match target {
        BuildTarget::Wasm => ("wasm32-unknown-unknown", "wasm-mod"),
        BuildTarget::Wasi => ("wasm32-wasip1", "wasm-wasi"),
    };

    // 1. Resolve paths relative to the SCRIPT file location
    let script_abs = path.canonicalize().context("Could not find script path")?;
    let script_dir = script_abs.parent().context("Could not get script directory")?;
    let stem = path.file_stem().unwrap().to_str().unwrap();

    // 2. Setup the output directory (wasm-wasi or wasm-mod)
    let output_dir = script_dir.join(folder_name);
    fs::create_dir_all(&output_dir).context("Failed to create output directory")?;

    // 3. Define the final output file path
    let final_filename = out.unwrap_or_else(|| {
        let mut p = PathBuf::from(stem);
        p.set_extension("wasm");
        p
    });
    let absolute_out = output_dir.join(final_filename);

    // 4. Setup Local Build Directory (Synthetic Project)
    let local_build_dir = script_dir.join(format!(".rsxtk_build_{}_{}", stem, folder_name));
    let src_dir = local_build_dir.join("src");
    
    if local_build_dir.exists() { let _ = fs::remove_dir_all(&local_build_dir); }
    fs::create_dir_all(&src_dir)?;

    // 5. Manifest and Source extraction
    let content = fs::read_to_string(&script_abs)?;
    let (_start, end, doc) = parse_manifest(&content)
        .context("Script requires a --- front-matter block.")?;

    let mut manifest = doc.clone();
    let mut pkg = toml_edit::Table::new();
    pkg.insert("name", toml_edit::value("wasm_out")); 
    pkg.insert("version", toml_edit::value("0.1.0"));
    pkg.insert("edition", toml_edit::value("2021"));
    manifest.insert("package", toml_edit::Item::Table(pkg));
    
    // Force binary target for WASI compatibility
    let mut bin = toml_edit::ArrayOfTables::new();
    let mut target_bin = toml_edit::Table::new();
    target_bin.insert("name", toml_edit::value("wasm_out"));
    target_bin.insert("path", toml_edit::value("src/main.rs"));
    bin.push(target_bin);
    manifest.insert("bin", toml_edit::Item::ArrayOfTables(bin));

    fs::write(local_build_dir.join("Cargo.toml"), manifest.to_string())?;

    let actual_code = match content[end..].find("---") {
        Some(pos) => content[end + pos + 3..].trim_start(),
        None => content[end..].trim_start(),
    };
    fs::write(src_dir.join("main.rs"), actual_code)?;

    println!("⚒️ Compiling {} for {}...", stem, triple);

    // 6. Build
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg(triple)
        .current_dir(&local_build_dir)
        .status()?;

    if status.success() {
        let release_dir = local_build_dir.join("target").join(triple).join("release");
        
        let wasm_src = fs::read_dir(&release_dir)?
            .filter_map(|entry| entry.ok())
            .map(|e| e.path())
            .find(|p| p.extension().map_or(false, |ext| ext == "wasm"))
            .context("No .wasm file found in release directory")?;

        // 7. Copy to the organized folder
        fs::copy(&wasm_src, &absolute_out)
            .with_context(|| format!("OS Error 3 fix: Failed copy from {} to {}", wasm_src.display(), absolute_out.display()))?;
            
        println!("✨ Success! Created: {}/{}", folder_name, absolute_out.file_name().unwrap().to_string_lossy());
        
        let _ = fs::remove_dir_all(local_build_dir);
    } else {
        anyhow::bail!("Cargo build failed.");
    }

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
    
    if fs::read_to_string(&temp_path).map(|old| old != script_body).unwrap_or(true) {
        fs::write(&temp_path, script_body)?;
    }

    Command::new("rust-script")
        .args(&dep_flags)
        .arg(&temp_path)
        .args(args)
        .status()
        .context("Make sure 'rust-script' is installed")?;
    Ok(())
}

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
    
    // 1. Ensure the dependencies table exists
    let deps = doc.entry("dependencies").or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    
    // 2. Determine the version (using "*" as a placeholder or fetch from API)
    let version = "*"; 

    if features.is_empty() {
        // Use the simple string format: name = "version"
        deps[name] = toml_edit::value(version);
    } else {
        // Use the inline table format: name = { version = "...", features = [...] }
        let mut table = toml_edit::InlineTable::default();
        table.insert("version", toml_edit::Value::from(version));
        
        let mut feat_array = toml_edit::Array::default();
        for f in features {
            feat_array.push(f);
        }
        table.insert("features", toml_edit::Value::from(feat_array));
        
        deps[name] = toml_edit::value(toml_edit::Value::InlineTable(table));
    }

    // 3. Reconstruct and save the file
    let mut final_content = content.clone();
    if start == 0 {
        final_content = format!("---\n{}---\n\n{}", doc, content);
    } else {
        final_content.replace_range(start..end, &doc.to_string());
    }
    
    fs::write(path, final_content)?;
    println!("✅ Added {} with features to {}", name, path.display());
    Ok(())
}

fn remove_dep(path: &PathBuf, name: &str) -> Result<()> {
    let content = fs::read_to_string(path)?;
    if let Some((start, end, mut doc)) = parse_manifest(&content) {
        doc.get_mut("dependencies").and_then(|d| d.as_table_mut()).map(|t| t.remove(name));
        let mut final_content = content.clone();
        final_content.replace_range(start..end, &doc.to_string());
        fs::write(path, final_content)?;
    }
    Ok(())
}

fn list_deps(path: &PathBuf) -> Result<()> {
    let content = fs::read_to_string(path)?;
    if let Some((_, _, doc)) = parse_manifest(&content) {
        if let Some(deps) = doc.get("dependencies").and_then(|d| d.as_table()) {
            for (n, v) in deps.iter() { println!("  ├── {:<15} : {}", n, v); }
        }
    }
    Ok(())
}

fn check_updates() {
    let informer = update_informer::new(registry::Crates, "rsxtk", env!("CARGO_PKG_VERSION"));
    if let Some(new_version) = informer.check_version().ok().flatten() {
        println!("🚀 New version available: {}", new_version);
    }
}