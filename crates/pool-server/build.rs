//! Embed the committed frontend without requiring Node in Rust builds.
use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn enumerate(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read frontend directory")
        .map(|entry| entry.expect("read frontend entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path).expect("inspect frontend entry");
        assert!(!metadata.file_type().is_symlink(), "frontend dist must not contain symlinks: {}", path.display());
        if metadata.is_dir() {
            enumerate(root, &path, files);
        } else if metadata.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            let key = path
                .strip_prefix(root)
                .unwrap()
                .components()
                .map(|part| part.as_os_str().to_str().expect("UTF-8 frontend path"))
                .collect::<Vec<_>>()
                .join("/");
            files.push((key, path));
        } else {
            panic!("unexpected frontend dist entry: {}", path.display());
        }
    }
}

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("static/dist");
    assert!(root.is_dir(), "missing committed static/dist; regenerate it with npm ci && npm run build in web/");
    let metadata = fs::symlink_metadata(&root).expect("inspect frontend dist");
    assert!(!metadata.file_type().is_symlink(), "frontend dist must not be a symlink");
    let mut files = Vec::new();
    enumerate(&root, &root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        files.iter().any(|(key, _)| key == "index.html"),
        "missing static/dist/index.html; regenerate frontend in web/"
    );
    let mut generated = String::from("pub fn lookup(path: &str) -> Option<&'static [u8]> {\nmatch path {\n");
    for (key, path) in files {
        generated.push_str(&format!("{key:?} => Some(include_bytes!({:?})),\n", path.to_str().unwrap()));
    }
    generated.push_str("_ => None,\n}\n}\n");
    fs::write(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("assets.rs"), generated)
        .expect("write embedded assets");
}
