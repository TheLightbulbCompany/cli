//! Embeds every file under `starter/` as `apps::starter::FILES`, sorted by path.

use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap();
        if name == "node_modules" || name == ".next" {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=starter");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("starter");
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut files: Vec<(String, &PathBuf)> = files
        .iter()
        .map(|f| {
            let rel = f.strip_prefix(&root).unwrap().to_str().unwrap();
            (rel.replace('\\', "/"), f)
        })
        .collect();
    files.sort();
    let mut code = String::from("pub const FILES: &[(&str, &[u8])] = &[\n");
    for (rel, f) in &files {
        code += &format!(
            "    ({rel:?}, include_bytes!({:?})),\n",
            f.to_str().unwrap()
        );
    }
    code += "];\n";
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("starter_files.rs");
    std::fs::write(out, code).unwrap();
}
