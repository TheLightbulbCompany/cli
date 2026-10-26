//! `lightbulb update` against a fake release folder (`LIGHTBULB_RELEASE_BASE=file://…`),
//! its `.sha256` signed with a test release key (`LIGHTBULB_RELEASE_KEY`).

mod common;

use common::{Env, EXE};
use ed25519_dalek::{Signer, SigningKey};
use lightbulb::cli::ASSET;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// The tests' release key: a fixed seed, so the update helper can name its public half.
fn release_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

/// A release folder whose `latest` names `tag` and whose tarball holds a stub `lightbulb`;
/// `sum` replaces the real checksum when set. Its `.sha256` is signed with `release_key`, as
/// `openssl pkeyutl -sign -rawin` signs it: the file's exact bytes.
fn release(dir: &Path, tag: &str, sum: Option<&str>) {
    let stage = dir.join("stage");
    fs::create_dir_all(&stage).unwrap();
    let stub = stage.join("lightbulb");
    fs::write(
        &stub,
        "#!/bin/sh\n[ \"$1\" = version ] && echo 'lightbulb 9.9.9'\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    let ok = |c: &mut Command| assert!(c.current_dir(dir).status().unwrap().success());
    ok(Command::new("tar").args(["-czf", ASSET, "-C", "stage", "lightbulb"]));
    let sha = Command::new("shasum")
        .args(["-a", "256", ASSET])
        .current_dir(dir)
        .output()
        .unwrap()
        .stdout;
    let sha = String::from_utf8(sha).unwrap();
    let sha = match sum {
        Some(s) => format!("{s}  {ASSET}\n"),
        None => sha,
    };
    let sig = release_key().sign(sha.as_bytes()).to_bytes();
    fs::write(dir.join(format!("{ASSET}.sha256")), sha).unwrap();
    fs::write(dir.join(format!("{ASSET}.sha256.sig")), sig).unwrap();
    fs::write(dir.join("latest"), format!("{tag}\n")).unwrap();
}

fn update(env: &Env, dir: &Path, args: &[&str]) -> Output {
    env.base_cmd(Command::new(EXE))
        .arg("update")
        .args(args)
        .env("LIGHTBULB_ALLOW_UNSIGNED", "1")
        .env(
            "LIGHTBULB_RELEASE_KEY",
            data_encoding::HEXLOWER.encode(release_key().verifying_key().as_bytes()),
        )
        .env(
            "LIGHTBULB_RELEASE_BASE",
            format!("file://{}", dir.display()),
        )
        .output()
        .unwrap()
}

fn canonical(env: &Env) -> std::path::PathBuf {
    env.home.path().join(".lightbulb/connect/bin/lightbulb")
}

fn text(o: &[u8]) -> String {
    String::from_utf8_lossy(o).into_owned()
}

#[test]
fn a_signed_release_installs() {
    let env = Env::new();
    let rel = tempfile::tempdir().unwrap();
    release(rel.path(), "v9.9.9", None);
    let o = update(&env, rel.path(), &[]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains(&format!("Updating {} → 9.9.9…", env!("CARGO_PKG_VERSION"))));
    let v = Command::new(canonical(&env))
        .arg("version")
        .output()
        .unwrap();
    assert_eq!(text(&v.stdout).trim(), "lightbulb 9.9.9");
    let mode = fs::metadata(canonical(&env)).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    // Nothing left beside it.
    let names: Vec<_> = fs::read_dir(canonical(&env).parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["lightbulb"]);
}

#[test]
fn a_wrong_checksum_changes_nothing() {
    let env = Env::new();
    let canon = canonical(&env);
    fs::create_dir_all(canon.parent().unwrap()).unwrap();
    fs::write(&canon, "old").unwrap();
    let rel = tempfile::tempdir().unwrap();
    release(rel.path(), "v9.9.9", Some(&"0".repeat(64)));
    let o = update(&env, rel.path(), &[]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(
        text(&o.stderr).trim(),
        "Checksum mismatch; nothing changed."
    );
    assert_eq!(fs::read_to_string(&canon).unwrap(), "old");
}

#[test]
fn an_unsigned_or_wrongly_signed_release_changes_nothing() {
    let env = Env::new();
    let canon = canonical(&env);
    fs::create_dir_all(canon.parent().unwrap()).unwrap();
    fs::write(&canon, "old").unwrap();
    let rel = tempfile::tempdir().unwrap();
    release(rel.path(), "v9.9.9", None);
    let sig = rel.path().join(format!("{ASSET}.sha256.sig"));
    // Signed by another key.
    let other = SigningKey::from_bytes(&[8; 32]);
    let sum = fs::read(rel.path().join(format!("{ASSET}.sha256"))).unwrap();
    fs::write(&sig, other.sign(&sum).to_bytes()).unwrap();
    let o = update(&env, rel.path(), &[]);
    assert_ne!(o.status.code(), Some(0));
    assert!(
        text(&o.stderr).contains("nothing changed"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(fs::read_to_string(&canon).unwrap(), "old");
    // No signature at all.
    fs::remove_file(&sig).unwrap();
    let o = update(&env, rel.path(), &[]);
    assert_ne!(o.status.code(), Some(0));
    assert!(
        text(&o.stderr).contains("nothing changed"),
        "{}",
        text(&o.stderr)
    );
    assert_eq!(fs::read_to_string(&canon).unwrap(), "old");
}

#[test]
fn versions_before_0_2_2_are_refused() {
    let env = Env::new();
    let rel = tempfile::tempdir().unwrap();
    let o = update(&env, rel.path(), &["--version", "v0.2.1"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(
        text(&o.stderr).trim(),
        "Versions before 0.2.2 cannot be installed."
    );
    assert!(!canonical(&env).exists());
}

#[test]
fn the_same_version_downloads_nothing() {
    let env = Env::new();
    let rel = tempfile::tempdir().unwrap();
    // Only `latest`: no asset to download.
    fs::write(
        rel.path().join("latest"),
        format!("v{}\n", env!("CARGO_PKG_VERSION")),
    )
    .unwrap();
    let o = update(&env, rel.path(), &[]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(
        text(&o.stdout).trim(),
        format!("Already on {}.", env!("CARGO_PKG_VERSION"))
    );
    assert!(!canonical(&env).exists());
}
