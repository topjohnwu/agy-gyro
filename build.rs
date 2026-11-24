// SPDX-License-Identifier: MIT

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        println!("cargo:rerun-if-changed={}", path.display());
        out.push(path.to_path_buf());
    } else if path.is_dir() {
        println!("cargo:rerun-if-changed={}", path.display());
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                collect_files(&entry.path(), out);
            }
        }
    }
}

fn main() {
    let mut files = Vec::new();
    for path in ["Cargo.toml", "Cargo.lock", "build.rs", "models.toml", "src"] {
        collect_files(Path::new(path), &mut files);
    }

    let mut entries: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|p| (p.to_string_lossy().replace('\\', "/"), p))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = DefaultHasher::new();
    for (rel_path, path) in entries {
        rel_path.hash(&mut hasher);
        fs::read(&path)
            .expect("failed to read source file")
            .hash(&mut hasher);
    }

    println!("cargo:rustc-env=AGY_GYRO_BUILD_ID={:016x}", hasher.finish());
}
