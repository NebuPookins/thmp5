use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));
    let mut rust_files = vec![manifest_dir.join("build.rs")];
    collect_rust_files(&manifest_dir.join("src"), &mut rust_files);

    for path in &rust_files {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    run_rustfmt(&rust_files);
    tauri_build::build()
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out);
            continue;
        }

        if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn run_rustfmt(paths: &[PathBuf]) {
    let status = Command::new("rustfmt")
        .arg("--edition")
        .arg("2021")
        .args(paths)
        .status()
        .expect("failed to run rustfmt from build script");

    if !status.success() {
        panic!("rustfmt failed from build script");
    }
}
