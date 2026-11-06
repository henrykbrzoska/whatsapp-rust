//! Generates `client/src/app/types/specta.ts` from the Rust wire types.
//!
//! Run from the repo root with `npm run specta` (or
//! `cargo run -p bebok-server --bin export-types`).

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../client/src/app/types/specta.ts");
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let path = out.to_string_lossy().to_string();
    let touched = bebok_server::bindings::export(&path)?;
    println!("specta: wrote {path} ({touched} wire types)");
    Ok(())
}
