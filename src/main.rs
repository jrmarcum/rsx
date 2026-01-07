use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use toml_edit::DocumentMut;
use wasmtime::*;
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::{WasiCtxBuilder, DirPerms, FilePerms};

#[derive(Parser)]
#[command(name = "rsxtk", version = "0.4.9", about = "Rust Script & WASM Toolkit")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init { script: PathBuf },
    InitMod { script: PathBuf },
    Add { script: PathBuf, dependency: String, #[arg(short, long, value_delimiter = ',')] features: Vec<String> },
    Remove { script: PathBuf, dependency: String },
    List { path: PathBuf },
    Build {
        script: PathBuf,
        #[arg(value_enum, default_value_t = BuildTarget::Wasi)]
        target: BuildTarget,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    Run {
        path: PathBuf,
        #[arg(short, long)]
        invoke: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(ValueEnum, Clone)]
enum BuildTarget { Wasm, Wasi }

struct HostState { wasi: WasiP1Ctx }

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init { script } => {
            let template = "---\n[dependencies]\n---\n\nfn main() {\n    println!(\"Hello from rsxtk!\");\n}\n";
            write_template(&script, template)?;
        },
        Commands::InitMod { script } => {
            let template = "---\n[lib]\ncrate-type = [\"cdylib\"]\n---\n\n#[no_mangle]\npub extern \"C\" fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
            write_template(&script, template)?;
        },
        Commands::Add { script, dependency, features } => add_dep(&script, &dependency, features).await?,
        Commands::Remove { script, dependency } => remove_dep(&script, &dependency)?,
        Commands::List { path } => {
            if path.extension().map_or(false, |e| e == "wasm") {
                list_wasm_exports(&path)?;
            } else {
                list_deps(&path)?;
            }
        },
        Commands::Build { script, target, out } => build_script_stable(&script, target, out).await?,
        Commands::Run { path, invoke, args } => {
            if path.extension().map_or(false, |e| e == "wasm") {
                run_wasm_internal(&path, invoke, args).await?;
            } else {
                run_script(&path, args)?;
            }
        },
    }
    Ok(())
}

// (run_wasm_internal, parse_manifest, etc. are identical to previous versions)

async fn build_script_stable(path: &PathBuf, target: BuildTarget, out: Option<PathBuf>) -> Result<()> {
    let (triple, folder_name) = match target {
        BuildTarget::Wasm => ("wasm32-unknown-unknown", "wasm-mod"),
        BuildTarget::Wasi => ("wasm32-wasip1", "wasm-wasi"),
    };

    let script_abs = strip_unc_prefix(fs::canonicalize(path).context("Failed to canonicalize path")?);
    let script_dir = script_abs.parent().context("No parent")?;
    let stem = script_abs.file_stem().unwrap().to_str().unwrap();
    
    let output_dir = script_dir.join(folder_name);
    fs::create_dir_all(&output_dir)?;
    let absolute_out = match out {
        Some(o) => output_dir.join(o),
        None => output_dir.join(format!("{}.wasm", stem)),
    };

    let local_build_dir = script_dir.join(format!(".rsxtk_build_{}", stem));
    let src_dir = local_build_dir.join("src");
    if local_build_dir.exists() { fs::remove_dir_all(&local_build_dir)?; }
    fs::create_dir_all(&src_dir)?;

    let content = fs::read_to_string(&script_abs)?;
    let (code_start, mut manifest) = match parse_manifest(&content) {
        Some((_, end, doc)) => (end, doc),
        None => (0, DocumentMut::new()),
    };

    manifest.remove("bin"); manifest.remove("lib");
    let mut pkg = toml_edit::Table::new();
    pkg.insert("name", toml_edit::value("wasm_out"));
    pkg.insert("version", toml_edit::value("0.1.0"));
    pkg.insert("edition", toml_edit::value("2021"));
    manifest.insert("package", toml_edit::Item::Table(pkg));

    let src_filename = match target {
        BuildTarget::Wasi => {
            let mut bin = toml_edit::ArrayOfTables::new();
            let mut t = toml_edit::Table::new();
            t.insert("name", toml_edit::value("wasm_out"));
            t.insert("path", toml_edit::value("src/main.rs"));
            bin.push(t); manifest.insert("bin", toml_edit::Item::ArrayOfTables(bin));
            "main.rs"
        },
        BuildTarget::Wasm => {
            let mut lib = toml_edit::Table::new();
            lib.insert("path", toml_edit::value("src/lib.rs"));
            let mut types = toml_edit::Array::new(); types.push("cdylib");
            lib.insert("crate-type", toml_edit::value(types));
            manifest.insert("lib", toml_edit::Item::Table(lib));
            "lib.rs"
        }
    };

    fs::write(local_build_dir.join("Cargo.toml"), manifest.to_string())?;
    fs::write(src_dir.join(src_filename), content[code_start..].trim_start())?;

    println!("⚒️  Building {} for {}...", stem, triple);
    let build_start_time = SystemTime::now();
    
    // Using explicit target-dir to avoid any ambiguity
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg(triple)
        .env("CARGO_TARGET_DIR", "target") // Explicitly set target dir
        .current_dir(&local_build_dir)
        .status()?;

    if status.success() {
        let wasm_src = find_newest_wasm(&local_build_dir, build_start_time);
        
        match wasm_src {
            Some(src) => {
                fs::copy(&src, &absolute_out)
                    .with_context(|| format!("Failed to copy from {} to {}", src.display(), absolute_out.display()))?;
                println!("✨ Created: {}", absolute_out.display());
                let _ = fs::remove_dir_all(local_build_dir);
            },
            None => {
                println!("❌ Build error: No .wasm file found. Scanning directory structure...");
                debug_list_files(&local_build_dir);
                return Err(anyhow::anyhow!("Cargo succeeded, but no .wasm artifact was detected. Check the list above."));
            }
        }
    }
    Ok(())
}

fn debug_list_files(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                println!("  FILE: {}", path.display());
                if path.is_dir() { stack.push(path); }
            }
        }
    }
}

// (find_newest_wasm and other helpers remain the same as previous)
fn find_newest_wasm(root: &Path, start_time: SystemTime) -> Option<PathBuf> {
    let mut newest_file = None;
    let mut newest_time = start_time;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() { stack.push(path); } 
                else if path.extension().map_or(false, |e| e == "wasm") {
                    if let Ok(metadata) = entry.metadata() {
                        if let Ok(modified) = metadata.modified() {
                            if modified >= newest_time {
                                newest_time = modified;
                                newest_file = Some(path);
                            }
                        }
                    }
                }
            }
        }
    }
    newest_file
}

// ... include other helpers from previous main.rs ...
async fn run_wasm_internal(path: &PathBuf, invoke: Option<String>, args: Vec<String>) -> Result<()> {
    let mut config = Config::new();
    config.async_support(true); 
    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);
    preview1::add_to_linker_async(&mut linker, |t: &mut HostState| &mut t.wasi)?;
    let module = Module::from_file(&engine, path).context("Failed to load WASM module")?;
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdio().args(&args).preopened_dir(".", ".", DirPerms::all(), FilePerms::all())?;
    let mut store = Store::new(&engine, HostState { wasi: wasi_builder.build_p1() });
    let instance = linker.instantiate_async(&mut store, &module).await?;
    if let Some(func_name) = invoke {
        if args.len() >= 2 {
            let func = instance.get_typed_func::<(i32, i32), i32>(&mut store, &func_name)?;
            let a: i32 = args[0].parse().unwrap_or(0);
            let b: i32 = args[1].parse().unwrap_or(0);
            let result = func.call_async(&mut store, (a, b)).await?;
            println!("✨ Result: {}", result);
        } else {
            let func = instance.get_typed_func::<i32, i32>(&mut store, &func_name)?;
            let val: i32 = args.get(0).and_then(|s| s.parse().ok()).unwrap_or(0);
            let result = func.call_async(&mut store, val).await?;
            println!("✨ Result: {}", result);
        }
    } else {
        let start = instance.get_typed_func::<(), ()>(&mut store, "_start").context("No '_start' found.")?;
        start.call_async(&mut store, ()).await?;
    }
    Ok(())
}
fn parse_manifest(content: &str) -> Option<(usize, usize, DocumentMut)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") { return None; }
    let start_offset = content.len() - trimmed.len();
    let first_line_end = trimmed.find('\n')? + 1;
    let m_start = start_offset + first_line_end;
    let remainder = &content[m_start..];
    let end_rel = remainder.find("---")?;
    let m_end = m_start + end_rel;
    let doc = content[m_start..m_end].parse::<DocumentMut>().ok()?;
    Some((m_start, m_end + 3, doc))
}
fn write_template(path: &PathBuf, content: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, content)?;
        println!("✨ Created template: {}", path.display());
    } else {
        println!("⚠️  File '{}' already exists!", path.display());
    }
    Ok(())
}
async fn add_dep(path: &PathBuf, name: &str, features: Vec<String>) -> Result<()> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let (start, end, mut doc) = parse_manifest(&content).unwrap_or_else(|| (0, 0, DocumentMut::new()));
    let deps = doc.entry("dependencies").or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    if features.is_empty() { deps[name] = toml_edit::value("*"); } 
    else {
        let mut t = toml_edit::InlineTable::default();
        t.insert("version", toml_edit::Value::from("*"));
        let mut a = toml_edit::Array::default();
        for f in features { a.push(f); }
        t.insert("features", toml_edit::Value::from(a));
        deps[name] = toml_edit::value(toml_edit::Value::InlineTable(t));
    }
    if start == 0 {
        let new_content = format!("---\n{}---\n\n{}", doc, content);
        fs::write(path, new_content)?;
    } else {
        let mut final_content = content.clone();
        final_content.replace_range(start..end - 3, &doc.to_string());
        fs::write(path, final_content)?;
    }
    println!("✅ Added dependency: {}", name);
    Ok(())
}
fn remove_dep(path: &PathBuf, name: &str) -> Result<()> {
    let content = fs::read_to_string(path)?;
    if let Some((start, end, mut doc)) = parse_manifest(&content) {
        doc.get_mut("dependencies").and_then(|d| d.as_table_mut()).map(|t| t.remove(name));
        let mut final_content = content.clone();
        final_content.replace_range(start..end - 3, &doc.to_string());
        fs::write(path, final_content)?;
        println!("🗑️  Removed dependency: {}", name);
    }
    Ok(())
}
fn list_deps(path: &PathBuf) -> Result<()> {
    let content = fs::read_to_string(path)?;
    match parse_manifest(&content) {
        Some((_, _, doc)) => {
            println!("📦 Dependencies in {}:", path.display());
            if let Some(deps) = doc.get("dependencies").and_then(|d| d.as_table()) {
                for (n, v) in deps.iter() { println!("  ├── {:<15} : {}", n, v); }
            }
        },
        None => println!("ℹ️  No manifest found in {}. (Running with defaults)", path.display()),
    }
    Ok(())
}
fn list_wasm_exports(path: &PathBuf) -> Result<()> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, path)?;
    println!("🛠️  Exported functions in {}:", path.file_name().unwrap().to_string_lossy());
    for export in module.exports() {
        if let ExternType::Func(_) = export.ty() { println!("  ├── {}", export.name()); }
    }
    Ok(())
}
fn run_script(path: &PathBuf, args: Vec<String>) -> Result<()> {
    Command::new("rust-script").arg(path).args(args).status().context("rust-script failed")?;
    Ok(())
}
fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with(r"\\?\") { PathBuf::from(&s[4..]) } else { path }
}