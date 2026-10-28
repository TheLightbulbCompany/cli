//! `config.toml` in the state dir: this computer's settings, edited by hand or by `set`.

use crate::setup::{self, AgentReport};
use crate::{os, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

pub const FILE: &str = "config.toml";

/// A setting's type, and the field it fills.
#[derive(Clone, Copy)]
enum Kind {
    Str(fn(&mut Config) -> &mut String),
    Bool(fn(&mut Config) -> &mut bool),
    OptStr(fn(&mut Config) -> &mut Option<String>),
    Paths(fn(&mut Config) -> &mut Vec<PathBuf>),
}

/// Every setting: the one list `load` and `set` read.
const KEYS: [(&str, Kind); 9] = [
    ("computer", Kind::Str(|c| &mut c.computer)),
    (
        "preview_to_workspace",
        Kind::Bool(|c| &mut c.preview_to_workspace),
    ),
    ("share_paths", Kind::Bool(|c| &mut c.share_paths)),
    ("lan", Kind::Bool(|c| &mut c.lan)),
    ("relay", Kind::OptStr(|c| &mut c.relay)),
    ("folders_off", Kind::Paths(|c| &mut c.folders_off)),
    ("auto_update", Kind::Bool(|c| &mut c.auto_update)),
    ("telemetry", Kind::Bool(|c| &mut c.telemetry)),
    ("memory_team", Kind::Bool(|c| &mut c.memory_team)),
];

fn kind(key: &str) -> Option<Kind> {
    KEYS.iter().find(|(k, _)| *k == key).map(|&(_, kind)| kind)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub computer: String,
    pub preview_to_workspace: bool,
    pub share_paths: bool,
    pub lan: bool,
    pub relay: Option<String>,
    pub folders_off: Vec<PathBuf>,
    /// The daemon installs a newer release once found (`cli::auto_update`).
    pub auto_update: bool,
    /// Anonymous usage counts (`telemetry`).
    pub telemetry: bool,
    /// `remember` and `memory share` may write `team` entries (`memory team on|off`).
    pub memory_team: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            computer: os::hostname(),
            preview_to_workspace: false,
            share_paths: false,
            lan: true,
            relay: None,
            folders_off: vec![],
            auto_update: true,
            telemetry: true,
            memory_team: true,
        }
    }
}

impl Config {
    /// The settings in `dir/config.toml`; a missing file (or key) is the default.
    pub fn load(dir: &Path) -> Result<Config> {
        from_doc(&read(dir)?)
    }

    /// Sets one key, keeping the file's other text and comments. `value` is read by the
    /// key's type: a string verbatim, `true`/`false`, or a TOML array of paths. Refuses an
    /// unknown key or a value of the wrong type.
    pub fn set(dir: &Path, key: &str, value: &str) -> Result<()> {
        let kind = kind(key).ok_or_else(|| format!("{FILE}: no setting {key}"))?;
        let v: toml_edit::Value = match kind {
            Kind::Str(_) | Kind::OptStr(_) => value.into(),
            Kind::Bool(_) | Kind::Paths(_) => value
                .parse()
                .map_err(|_| format!("{FILE}: {key} has the wrong type"))?,
        };
        let mut doc = read(dir)?;
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

/// `p` with a leading `~` (alone or before `/`) as the home directory.
pub(crate) fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix('~'), dirs::home_dir()) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(p),
    }
}

fn from_doc(doc: &DocumentMut) -> Result<Config> {
    let mut c = Config::default();
    for (k, item) in doc.iter() {
        let Some(kind) = kind(k) else {
            continue; // a setting from a newer version
        };
        let bad = || format!("{FILE}: {k} has the wrong type");
        let text = || item.as_str().map(String::from).ok_or_else(bad);
        match kind {
            Kind::Str(f) => *f(&mut c) = text()?,
            Kind::OptStr(f) => *f(&mut c) = Some(text()?),
            Kind::Bool(f) => *f(&mut c) = item.as_bool().ok_or_else(bad)?,
            Kind::Paths(f) => {
                *f(&mut c) = item
                    .as_array()
                    .ok_or_else(bad)?
                    .iter()
                    .map(|v| v.as_str().map(expand_home).ok_or_else(bad))
                    .collect::<std::result::Result<_, _>>()?
            }
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_leading_tilde_is_the_home_dir() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~/secret"), home.join("secret"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~bob/x"), PathBuf::from("~bob/x"));
        assert_eq!(expand_home("/a/~"), PathBuf::from("/a/~"));
    }

    #[test]
    fn config_defaults_and_set_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let c = Config::load(d).unwrap();
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

        Config::set(d, "computer", "2024").unwrap();
        assert_eq!(Config::load(d).unwrap().computer, "2024");

        assert!(Config::set(d, "nope", "1").is_err());
        assert!(Config::set(d, "lan", "maybe").is_err());
        assert!(
            !Config::load(d).unwrap().lan,
            "a refused set writes nothing"
        );
    }
}
