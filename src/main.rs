use anyhow::{Result, bail}; // Removed Context
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::{self, WasiP1Ctx};

#[derive(Parser)]
#[command(name = "rsxtk", about = "Rust WASM Toolkit (2024 Edition)", version = "0.9.5")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Optimize {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    Compile {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    Run {
        path: PathBuf,
    },
}

struct MyState {
    wasi: WasiP1Ctx,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Optimize { input, output } => optimize_wasm(&input, output),
        Commands::Compile { input, output } => {
            let engine = Engine::default();
            compile_to_cwasm(&engine, &input, output).map(|_| ())
        }
        Commands::Run { path } => run_wasm(&path),
    }
}

fn optimize_wasm(input_path: &Path, output_path: Option<PathBuf>) -> Result<()> {
    let binding;
    let output = if let Some(ref path) = output_path { path } else {
        binding = input_path.to_path_buf();
        &binding
    };

    println!("🧹 Optimizing WASM via Walrus...");
    let original_bytes = fs::read(input_path)?;
    let mut module = walrus::Module::from_buffer(&original_bytes)?;
    let optimized_bytes = module.emit_wasm();
    fs::write(output, optimized_bytes)?;
    println!("✅ Done.");
    Ok(())
}

fn compile_to_cwasm(engine: &Engine, input_path: &Path, output_path: Option<PathBuf>) -> Result<PathBuf> {
    let output = output_path.unwrap_or_else(|| input_path.with_extension("cwasm"));
    let wasm_bytes = fs::read(input_path)?;
    let cwasm_bytes = engine.precompile_module(&wasm_bytes)?;
    fs::write(&output, cwasm_bytes)?;
    Ok(output)
}

fn run_wasm(path: &Path) -> Result<()> {
    let engine = Engine::default();
    
    // Changed to immutable as per compiler suggestion
    let target_artifact: PathBuf;

    // --- SMART DISPATCHER ---
    if path.extension().and_then(|s| s.to_str()) == Some("rs") {
        target_artifact = build_and_cache_script(&engine, path)?;
    } else if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
        target_artifact = compile_to_cwasm(&engine, path, None)?;
    } else {
        target_artifact = path.to_path_buf();
    }

    println!("🏃 Executing native artifact: {}...", target_artifact.display());
    let module = unsafe { Module::deserialize_file(&engine, &target_artifact)? };

    let mut linker: Linker<MyState> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |state| &mut state.wasi)?;

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio().inherit_args();
    
    let cur_dir = std::env::current_dir()?;
    builder.preopened_dir(&cur_dir, ".", wasmtime_wasi::DirPerms::all(), wasmtime_wasi::FilePerms::all())?;
    
    let mut store = Store::new(&engine, MyState { 
        wasi: builder.build_p1(), 
    });

    let instance = linker.instantiate(&mut store, &module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    start.call(&mut store, ())?;

    Ok(())
}

fn build_and_cache_script(engine: &Engine, script_path: &Path) -> Result<PathBuf> {
    let internal_name = "script";
    let script_stem = script_path.file_stem().unwrap().to_str().unwrap();
    
    // Absolute path to the cache for this script
    let cache_root = std::env::current_dir()?.join(".rsxtk_cache").join(script_stem);
    let src_dir = cache_root.join("src");
    let cwasm_path = cache_root.join(format!("{}.cwasm", script_stem));

    // --- STALENESS CHECK ---
    if cwasm_path.exists() {
        let script_mtime = fs::metadata(script_path)?.modified()?;
        let cwasm_mtime = fs::metadata(&cwasm_path)?.modified()?;
        if cwasm_mtime > script_mtime {
            return Ok(cwasm_path);
        }
    }

    // --- VIRTUAL PROJECT SETUP ---
    println!("📦 Preparing virtual build in cache...");
    let content = fs::read_to_string(script_path)?;
    let parts: Vec<&str> = content.split("---").collect();
    if parts.len() < 3 { bail!("Script missing frontmatter (--- [dependencies] ---)."); }
    
    let manifest_content = parts[1].trim();
    let code_content = parts[2..].join("---");

    fs::create_dir_all(&src_dir)?;

    let cargo_toml = format!(
        "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n{}",
        internal_name, manifest_content
    );
    
    fs::write(cache_root.join("Cargo.toml"), cargo_toml)?;
    fs::write(src_dir.join("main.rs"), code_content)?;

    // --- BUILD WITH EXPLICIT TARGET DIR ---
    println!("🔨 Building WASM...");
    let status = std::process::Command::new("cargo")
        .env("CARGO_TARGET_DIR", &cache_root.join("target")) // FORCE output into cache
        .args([
            "build", 
            "--manifest-path", cache_root.join("Cargo.toml").to_str().unwrap(),
            "--target", "wasm32-wasip1", 
            "--release",
        ])
        .status()?;

    if !status.success() { bail!("Cargo build failed."); }

    // Path is now strictly controlled: cache_root/target/wasm32-wasip1/release/script.wasm
    let wasm_output = cache_root
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join("script.wasm");
    
    if !wasm_output.exists() {
        bail!("WASM output not found at predicted path: {}", wasm_output.display());
    }

    // --- PRECOMPILE TO CACHE ---
    println!("🚀 Precompiling to CWASM...");
    let result_path = compile_to_cwasm(engine, &wasm_output, Some(cwasm_path))?;
    
    Ok(result_path)
}