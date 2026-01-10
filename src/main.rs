use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::{Engine, Linker, Module, Store};
// Updated imports for modern Wasmtime WASI
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::preview1::{self, WasiP1Ctx};

#[derive(Parser)]
#[command(name = "rsxtk")]
#[command(about = "Rust WASM Toolkit", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Optimize a WASM file using Walrus (Pure Rust)
    Optimize {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Compile a WASM file to a Wasmtime-native .cwasm file (AOT)
    Compile {
        #[arg(short, long)]
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Run a .wasm or .cwasm file
    Run {
        path: PathBuf,
    },
}

// Preview1 uses WasiP1Ctx specifically for traditional WASI compatibility
struct MyState {
    wasi: WasiP1Ctx,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Optimize { input, output } => optimize_wasm(&input, output)?,
        Commands::Compile { input, output } => compile_to_cwasm(&input, output)?,
        Commands::Run { path } => run_wasm(&path)?,
    }

    Ok(())
}

fn optimize_wasm(input_path: &Path, output_path: Option<PathBuf>) -> Result<()> {
    let output = output_path.unwrap_or_else(|| input_path.to_path_buf());
    println!("🧹 Optimizing WASM via Walrus...");
    
    let original_bytes = fs::read(input_path)?;
    let original_size = original_bytes.len() as u64;

    let mut module = walrus::Module::from_buffer(&original_bytes)?;
    let optimized_bytes = module.emit_wasm();

    fs::write(&output, optimized_bytes)?;
    let new_size = fs::metadata(&output)?.len();
    
    let reduction = if original_size > 0 {
        100 - (new_size * 100 / original_size)
    } else { 0 };

    println!("✅ Optimization complete: {}% reduction ({} -> {} bytes)", 
             reduction, original_size, new_size);
    Ok(())
}

fn compile_to_cwasm(input_path: &Path, output_path: Option<PathBuf>) -> Result<()> {
    let output = output_path.unwrap_or_else(|| input_path.with_extension("cwasm"));
    println!("🚀 Compiling to native code (AOT)...");
    
    let engine = Engine::default();
    let wasm_bytes = fs::read(input_path)?;
    let cwasm_bytes = engine.precompile_module(&wasm_bytes)?;

    fs::write(&output, cwasm_bytes)?;
    println!("✅ Created AOT artifact: {}", output.display());
    Ok(())
}

fn run_wasm(path: &Path) -> Result<()> {
    let engine = Engine::default();
    
    let module = if path.extension().and_then(|s| s.to_str()) == Some("cwasm") {
        println!("🏃 Running precompiled AOT module...");
        unsafe { Module::deserialize_file(&engine, path)? }
    } else {
        println!("🔨 Compiling and running WASM...");
        Module::from_file(&engine, path)?
    };

    let mut linker: Linker<MyState> = Linker::new(&engine);
    
    // FIX: Provide the closure that points to the WASI context in our state
    preview1::add_to_linker_sync(&mut linker, |state| &mut state.wasi)?;

    // Build the Preview 1 context
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio().inherit_args();
    
    let mut store = Store::new(&engine, MyState { 
        wasi: builder.build_p1(), // Crucial: build_p1 for Core WASM
    });

    let instance = linker.instantiate(&mut store, &module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    start.call(&mut store, ())?;

    Ok(())
}