//! Compiles every `*.proto` under `proto/` (vendored from the Go client) with prost.
//!
//! `protoc` comes from `protoc-bin-vendored`, so no system install is needed.
//! The generated modules are concatenated into `$OUT_DIR/_includes.rs`.

use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "proto") {
            out.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("proto");
    let root = root.canonicalize().expect("wa-proto/proto not found");

    let mut protos = Vec::new();
    collect(&root, &mut protos);
    protos.sort();
    println!("cargo:rerun-if-changed={}", root.display());

    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    // SAFETY: build scripts are single-threaded.
    unsafe { std::env::set_var("PROTOC", protoc) };

    prost_build::Config::new().include_file("_includes.rs").compile_protos(&protos, &[&root]).expect("prost compile");
}
