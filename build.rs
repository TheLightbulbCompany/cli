//! Embeds every file under `starter/` as `apps::starter::FILES`, sorted by path, skipping what
//! the starter's own `.gitignore` ignores (a local `pnpm install`, build or `up` leaves those).

use std::path::{Path, PathBuf};

/// `*` matches any run of characters within one path component.
fn glob(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat == s,
        Some((pre, rest)) => {
            s.starts_with(pre)
                && (pre.len()..=s.len()).any(|i| s.is_char_boundary(i) && glob(rest, &s[i..]))
        }
    }
}

/// Whether `rel` (a path under `starter/`) matches one of the `.gitignore` lines: a pattern
/// with a `/` before its end is anchored at the root, else it matches any component; a
/// trailing `/` matches directories only.
fn ignored(rel: &str, is_dir: bool, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let (p, dir_only) = p
            .strip_suffix('/')
            .map_or((p.as_str(), false), |p| (p, true));
        if dir_only && !is_dir {
            return false;
        }
        if p.contains('/') {
            let p = p.trim_start_matches('/');
            let (pc, rc): (Vec<_>, Vec<_>) = (p.split('/').collect(), rel.split('/').collect());
            pc.len() == rc.len() && pc.iter().zip(&rc).all(|(p, r)| glob(p, r))
        } else {
            glob(p, rel.rsplit('/').next().unwrap())
        }
    })
}

fn walk(root: &Path, dir: &Path, patterns: &[String], out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let rel = path.strip_prefix(root).unwrap().to_str().unwrap();
        // The addon mirrors the app's tree, so its paths are matched as the app's.
        let as_app = rel.strip_prefix("addons/content/").unwrap_or(rel);
        if ignored(as_app, path.is_dir(), patterns) {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, patterns, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=starter");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("starter");
    let patterns: Vec<String> = std::fs::read_to_string(root.join(".gitignore"))
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    assert!(
        patterns.iter().all(|p| !p.starts_with('!')),
        "build.rs does not handle negated .gitignore lines"
    );
    let mut files = Vec::new();
    walk(&root, &root, &patterns, &mut files);
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
