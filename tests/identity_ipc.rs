use lightbulb::{daemon, paths};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

async fn connect(dir: &Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) {
    for _ in 0..50 {
        if let Ok(s) = UnixStream::connect(dir.join(paths::SOCKET)).await {
            let (r, w) = s.into_split();
            return (BufReader::new(r).lines(), w);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon socket never came up");
}

async fn call(
    lines: &mut Lines<BufReader<OwnedReadHalf>>,
    w: &mut OwnedWriteHalf,
    req: Value,
) -> Value {
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn identity_get_matches_status() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    std::env::set_var("LIGHTBULB_CONNECT_CLAUDE_HOME", dir.path());
    std::fs::write(dir.path().join("config.toml"), "computer = \"mac-b\"\n").unwrap();
    tokio::spawn(async move { daemon::run(&home).await.unwrap() });
    let (mut lines, mut w) = connect(dir.path()).await;

    let status = call(&mut lines, &mut w, json!({"id": 1, "op": "status"})).await;
    let me = call(&mut lines, &mut w, json!({"id": 2, "op": "identity.get"})).await;
    assert_eq!(me["ok"], true);
    assert_eq!(me["member_key_short"], status["member_key_short"]);
    let key = me["member_key"].as_str().unwrap();
    assert_eq!(key.len(), 52);
    assert!(key.starts_with(me["member_key_short"].as_str().unwrap()));
    assert_eq!(me["computer"], "mac-b");
    assert_eq!(me["person_id"], key);
    assert!(me["name"].as_str().is_some_and(|n| !n.is_empty()));

    call(&mut lines, &mut w, json!({"id": 3, "op": "shutdown"})).await;
}

/// A throwaway keychain file (never the login keychain), deleted with its search-list entry on
/// drop. `None` where `security` cannot make one (a CI runner without a keychain service).
#[cfg(target_os = "macos")]
struct TestKeychain {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

#[cfg(target_os = "macos")]
impl TestKeychain {
    fn new() -> Option<(
        TestKeychain,
        security_framework::os::macos::keychain::SecKeychain,
    )> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.keychain");
        let made = std::process::Command::new("security")
            .args(["create-keychain", "-p", ""])
            .arg(&path)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("skipped: no keychain service here");
            return None;
        }
        let guard = TestKeychain {
            _dir: dir,
            path: path.clone(),
        };
        let mut kc = security_framework::os::macos::keychain::SecKeychain::open(&path).ok()?;
        kc.unlock(Some("")).ok()?;
        Some((guard, kc))
    }
}

#[cfg(target_os = "macos")]
impl Drop for TestKeychain {
    fn drop(&mut self) {
        let _ = std::process::Command::new("security")
            .arg("delete-keychain")
            .arg(&self.path)
            .status();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_key_is_not_on_disk_on_macos() {
    use lightbulb::identity::Identity;
    let Some((_guard, kc)) = TestKeychain::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let me = Identity::load_or_create_in(dir.path(), Some(&kc)).unwrap();
    assert!(!dir.path().join("identity.key").exists());
    assert!(lightbulb::identity::exists_in(dir.path(), Some(&kc)).unwrap());
    // The same key on the next start.
    let again = Identity::load_or_create_in(dir.path(), Some(&kc)).unwrap();
    assert_eq!(again.key, me.key);
    // Another state dir is another item.
    let other = tempfile::tempdir().unwrap();
    assert!(!lightbulb::identity::exists_in(other.path(), Some(&kc)).unwrap());
    assert_ne!(
        Identity::load_or_create_in(other.path(), Some(&kc))
            .unwrap()
            .key,
        me.key
    );
}

#[cfg(target_os = "macos")]
#[test]
fn an_existing_key_file_moves_into_the_keychain_once() {
    use lightbulb::identity::Identity;
    let Some((_guard, kc)) = TestKeychain::new() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // A key made by an earlier version: the file.
    let old = Identity::load_or_create_in(dir.path(), None).unwrap();
    assert!(dir.path().join("identity.key").exists());
    let moved = Identity::load_or_create_in(dir.path(), Some(&kc)).unwrap();
    assert_eq!(moved.key, old.key);
    assert!(!dir.path().join("identity.key").exists());
    // A later start reads the item; the file stays gone.
    let again = Identity::load_or_create_in(dir.path(), Some(&kc)).unwrap();
    assert_eq!(again.key, old.key);
    assert!(!dir.path().join("identity.key").exists());
}
