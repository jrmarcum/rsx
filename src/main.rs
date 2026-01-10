use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::{Engine, Linker, Module, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView, ResourceTable};
use wasmtime_wasi::preview1;

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

// In Wasmtime 29+, the Store state must provide access to WASI context and a resource table
struct MyState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for MyState {
    fn ctx(&mut self) -> &mut WasiCtx { &mut self.ctx }
    fn table(&mut self) -> &mut ResourceTable { &mut self.table }
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

    // Define the linker explicitly for your state
    let mut linker: Linker<MyState> = Linker::new(&engine);
    
    // FIX: Use the Preview1 (Core WASM) specific linker function
    preview1::add_to_linker_sync(&mut linker)?;

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio().inherit_args();
    let wasi_ctx = builder.build();

    let mut store = Store::new(&engine, MyState { 
        ctx: wasi_ctx,
        table: ResourceTable::new(),
    });

    let instance = linker.instantiate(&mut store, &module)?;
    
    // Look for the standard WASI entry point
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    start.call(&mut store, ())?;

    Ok(())
}