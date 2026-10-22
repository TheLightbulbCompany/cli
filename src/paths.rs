use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

/// Overrides the state dir (tests, side-by-side installs).
pub const HOME_ENV: &str = "LIGHTBULB_NETWORK_HOME";
pub const SOCKET: &str = "sock";

/// `~/.lightbulb/network` (or `$LIGHTBULB_NETWORK_HOME`), not created.
pub fn state_path() -> PathBuf {
    match std::env::var_os(HOME_ENV) {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir()
            .expect("no home directory")
            .join(".lightbulb/network"),
    }
}

/// `state_path`, created with mode 0700.
pub fn state_dir() -> PathBuf {
    let dir = state_path();
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .and_then(|_| fs::set_permissions(&dir, Permissions::from_mode(0o700)))
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    dir
}

pub fn socket() -> PathBuf {
    state_dir().join(SOCKET)
}
