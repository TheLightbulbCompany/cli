//! `config.toml` in the state dir: this computer's settings, edited by hand or by `set`.

use crate::setup::{self, AgentReport};
use crate::{os, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

pub const FILE: &str = "config.toml";
const KEYS: [&str; 7] = [
    "computer",
    "issuer",
    "preview_to_workspace",
    "share_paths",
    "lan",
    "relay",
    "folders_off",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub computer: String,
    pub issuer: String,
    pub preview_to_workspace: bool,
    pub share_paths: bool,
    pub lan: bool,
    pub relay: Option<String>,
    pub folders_off: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            computer: os::hostname(),
            issuer: "https://thelightbulb.company/network".into(),
            preview_to_workspace: false,
            share_paths: false,
            lan: true,
            relay: None,
            folders_off: vec![],
        }
    }
}

impl Config {
    /// The settings in `dir/config.toml`; a missing file (or key) is the default.
    pub fn load(dir: &Path) -> Result<Config> {
        from_doc(&read(dir)?)
    }

    /// Sets one key, keeping the file's other text and comments. `value` is a TOML literal
    /// (`false`, `["/a"]`) or else a plain string. Refuses an unknown key or a wrong type.
    pub fn set(dir: &Path, key: &str, value: &str) -> Result<()> {
        if !KEYS.contains(&key) {
            return Err(format!("{FILE}: no setting {key}").into());
        }
        let mut doc = read(dir)?;
        let v: toml_edit::Value = value.parse().unwrap_or_else(|_| value.into());
        doc[key] = toml_edit::value(v);
        from_doc(&doc)?;
        setup::write(
            &dir.join(FILE),
            &doc.to_string(),
            false,
            &mut AgentReport::default(),
        )
    }
}

fn read(dir: &Path) -> Result<DocumentMut> {
    let text = match fs::read_to_string(dir.join(FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(text.parse().map_err(|e| format!("{FILE}: {e}"))?)
}

fn from_doc(doc: &DocumentMut) -> Result<Config> {
    let mut c = Config::default();
    for (k, item) in doc.iter() {
        let bad = || format!("{FILE}: {k} has the wrong type");
        let text = || item.as_str().map(String::from).ok_or_else(bad);
        let flag = || item.as_bool().ok_or_else(bad);
        match k {
            "computer" => c.computer = text()?,
            "issuer" => c.issuer = text()?,
            "preview_to_workspace" => c.preview_to_workspace = flag()?,
            "share_paths" => c.share_paths = flag()?,
            "lan" => c.lan = flag()?,
            "relay" => c.relay = Some(text()?),
            "folders_off" => {
                c.folders_off = item
                    .as_array()
                    .ok_or_else(bad)?
                    .iter()
                    .map(|v| v.as_str().map(PathBuf::from).ok_or_else(bad))
                    .collect::<std::result::Result<_, _>>()?
            }
            _ => {} // a setting from a newer version
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn config_defaults_and_set_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let c = Config::load(d).unwrap();
        assert_eq!(c.issuer, "https://thelightbulb.company/network");
        assert_eq!(c.computer, crate::os::hostname());
        assert!(!c.preview_to_workspace && !c.share_paths && c.lan);
        assert_eq!((c.relay, c.folders_off.len()), (None, 0));

        Config::set(d, "computer", "mac-b").unwrap();
        Config::set(d, "lan", "false").unwrap();
        Config::set(d, "relay", "https://relay.example").unwrap();
        Config::set(d, "folders_off", r#"["/tmp/secret"]"#).unwrap();
        let c = Config::load(d).unwrap();
        assert_eq!(c.computer, "mac-b");
        assert!(!c.lan);
        assert_eq!(c.relay.as_deref(), Some("https://relay.example"));
        assert_eq!(c.folders_off, vec![PathBuf::from("/tmp/secret")]);
        let mode = std::fs::metadata(d.join("config.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        assert!(Config::set(d, "nope", "1").is_err());
        assert!(Config::set(d, "lan", "maybe").is_err());
        assert!(
            !Config::load(d).unwrap().lan,
            "a refused set writes nothing"
        );
    }
}
