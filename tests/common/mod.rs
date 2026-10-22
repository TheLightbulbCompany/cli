//! What the CLI test files share: a temp state dir and HOME, and a daemon shut down on drop.
//! ponytail: only `cli.rs` and `team_cli.rs` use it; the other test files keep their own copies.
#![allow(dead_code)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Output;

pub struct Env {
    pub state: tempfile::TempDir,
    pub home: tempfile::TempDir,
    /// `LIGHTBULB_NETWORK_HOOK_CALLER` for the daemon: the process it treats as an agent.
    pub pin: Option<String>,
}

impl Env {
    pub fn new() -> Env {
        Env {
            state: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
            pin: None,
        }
    }

    /// One daemon request on a fresh connection (the CLI starts the daemon on first use).
    pub fn ask(&self, body: Value) -> Value {
        let mut s = UnixStream::connect(self.state.path().join("sock")).unwrap();
        writeln!(s, "{body}").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Ok(mut s) = UnixStream::connect(self.state.path().join("sock")) {
            let _ = writeln!(s, r#"{{"id":1,"op":"shutdown"}}"#);
            let _ = s.read(&mut [0; 64]);
        }
    }
}

pub fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
