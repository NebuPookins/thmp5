use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("missing manifest dir"));
    let mut rust_files = vec![manifest_dir.join("build.rs")];
    let mut watched_files = rust_files.clone();
    collect_rust_files(&manifest_dir.join("src"), &mut rust_files);
    collect_rust_files(&manifest_dir.join("src"), &mut watched_files);
    collect_files(&manifest_dir.join("taglib-helper"), &mut watched_files);

    for path in &watched_files {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    run_rustfmt(&rust_files);
    build_taglib_helper(&manifest_dir);
    tauri_build::build()
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    collect_files_with_extension(dir, out, "rs");
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.file_name().and_then(|name| name.to_str()) == Some("build") {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
            continue;
        }

        out.push(path);
    }
}

fn collect_files_with_extension(dir: &Path, out: &mut Vec<PathBuf>, extension: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_with_extension(&path, out, extension);
            continue;
        }

        if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
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

fn build_taglib_helper(manifest_dir: &Path) {
    let helper_dir = manifest_dir.join("taglib-helper");
    if !helper_dir.exists() {
        return;
    }

    let target = std::env::var("TARGET").expect("missing TARGET");
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("missing OUT_DIR"));
    let build_dir = out_dir.join("taglib-helper-build");
    let binaries_dir = manifest_dir.join("binaries");
    fs::create_dir_all(&build_dir).expect("failed to create taglib-helper build dir");

    let mut configure = Command::new("cmake");
    configure
        .arg("-S")
        .arg(&helper_dir)
        .arg("-B")
        .arg(&build_dir)
        .arg("-DCMAKE_BUILD_TYPE=Release");
    if let Ok(taglib_root) = std::env::var("TAGLIB_ROOT") {
        configure.arg(format!("-DTAGLIB_ROOT={taglib_root}"));
    }
    if let Ok(toolchain_file) = std::env::var("CMAKE_TOOLCHAIN_FILE") {
        configure.arg(format!("-DCMAKE_TOOLCHAIN_FILE={toolchain_file}"));
    } else if target.contains("windows") {
        if let Ok(vcpkg_root) = std::env::var("VCPKG_ROOT") {
            let toolchain = Path::new(&vcpkg_root)
                .join("scripts")
                .join("buildsystems")
                .join("vcpkg.cmake");
            if toolchain.exists() {
                configure.arg(format!("-DCMAKE_TOOLCHAIN_FILE={}", toolchain.display()));
            }
        }
    }
    run_command(&mut configure, "configure taglib-helper");
    run_command(
        Command::new("cmake")
            .arg("--build")
            .arg(&build_dir)
            .arg("--config")
            .arg("Release"),
        "build taglib-helper",
    );

    let built_binary = helper_binary_path(&build_dir, &target);
    if !built_binary.exists() {
        panic!(
            "taglib-helper build succeeded but binary was not found at {}",
            built_binary.display()
        );
    }

    if profile == "release" {
        fs::create_dir_all(&binaries_dir).expect("failed to create binaries dir");
        let staged_binary = staged_helper_binary_path(&binaries_dir, &target);
        fs::copy(&built_binary, &staged_binary).unwrap_or_else(|error| {
            panic!(
                "failed to stage taglib-helper from {} to {}: {error}",
                built_binary.display(),
                staged_binary.display()
            )
        });
        println!(
            "cargo:rustc-env=THMP5_TAGLIB_HELPER_BUILT={}",
            staged_binary.display()
        );
    } else {
        println!(
            "cargo:rustc-env=THMP5_TAGLIB_HELPER_BUILT={}",
            built_binary.display()
        );
    }
}

fn helper_binary_path(build_dir: &Path, target: &str) -> PathBuf {
    if target.contains("windows") {
        build_dir.join("Release").join("taglib-helper.exe")
    } else {
        build_dir.join("taglib-helper")
    }
}

fn staged_helper_binary_path(binaries_dir: &Path, target: &str) -> PathBuf {
    if target.contains("windows") {
        binaries_dir.join(format!("taglib-helper-{target}.exe"))
    } else {
        binaries_dir.join(format!("taglib-helper-{target}"))
    }
}

fn run_command(command: &mut Command, description: &str) {
    let status = command.status().unwrap_or_else(|error| {
        panic!("failed to {description}: {error}");
    });
    if !status.success() {
        panic!("{description} failed with status {status}");
    }
}
