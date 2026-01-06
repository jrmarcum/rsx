# rsxtk 🦀

A high-performance CLI tool to manage and run **Rust Scripts** using the official **RFC 3502** front-matter format.

## Features
- **RFC 3502 Compliance**: Uses the standard `---` manifest block.
- **Fast Execution**: Translates manifests for the `rust-script` engine to bypass Cargo overhead.
- **WASM/WASI Support**: Compile scripts into portable modules with a single command.
- **Full Lifecycle**: `add`, `remove`, `list`, `fmt`, `test`, and `run`.

## Installation
```bash
cargo install rsx
