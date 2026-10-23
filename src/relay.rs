//! Relay fallback: which relays this computer uses. When two members cannot reach each other
//! directly, iroh carries their QUIC connection through a relay server, and hole punching
//! (which upgrades it to a direct path where it can) comes with the relays. QUIC is end to
//! end between the two member keys, so a relay forwards ciphertext only: nothing is ever
//! sent in plaintext, through a relay or otherwise.
//!
//! The list, in this order: `config.toml: relay`, else the team's own (the roster's
//! `meta {relay}`); either one wins and then nothing of ours is contacted. Else ours: the
//! issuer's `relays.json` and `relays.json.minisig`, verified against one of two compiled-in
//! minisign keys and accepted only whole, cached in the state dir with its signature and
//! refetched at most daily; the cached copy serves while a fetch fails. None at all: direct
//! only.

use crate::config::Config;
use crate::Result;
use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::Duration;

pub const FILE: &str = "relays.json";
pub const SIG_FILE: &str = "relays.json.minisig";
/// A cached list younger than this is used without asking the issuer.
const FRESH: Duration = Duration::from_secs(86_400);
/// The most a relay list (or its signature) may be.
const MAX_BYTES: u64 = 64 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The public halves of the two keys that may sign `relays.json`.
// replaced at the key ceremony (Plan C): two throwaway PLACEHOLDER keys whose secret halves
// were discarded, so this binary refuses every real list (it fails closed, to direct only).
pub const KEYS: [&str; 2] = [
    "RWQqLNqi818V85+6Z3/tjmMWBwnvm8zhYsCoBskzyf2K83kjQjSmI9aL",
    "RWSkWDcmOS8uCHvf3iXxJlcUhjopK2+ZzlCbhxSnwXkyUtsDxqs6EW6T",
];

/// Where the list in effect came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// `config.toml: relay`.
    Config,
    /// The roster's `meta {relay}`.
    Team,
    /// Our signed `relays.json`.
    Lightbulb,
    /// No relay: direct only.
    #[default]
    None,
}

/// The relays in effect, and why our list was refused, when it was.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct List {
    pub urls: Vec<String>,
    pub source: Source,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct Signed {
    v: u32,
    relays: Vec<String>,
    expires_at: i64,
}

/// The relays for `config` and the team's own `team` (the roster's `meta {relay}`; see the
/// module doc). With `fetch`, a cache older than
/// a day (or none) is first refreshed from `config.issuer`; without, the network is not used.
/// `keys` are the trusted public keys (`KEYS`, or the debug knob's).
pub fn list(
    dir: &Path,
    config: &Config,
    team: &Option<String>,
    keys: &[String],
    fetch: bool,
) -> List {
    let own = |v: &Option<String>| {
        let u = v.as_deref().map(str::trim).filter(|u| !u.is_empty());
        u.map(|u| vec![u.to_string()])
    };
    if let Some(urls) = own(&config.relay) {
        return List {
            urls,
            source: Source::Config,
            error: None,
        };
    }
    if let Some(urls) = own(team) {
        return List {
            urls,
            source: Source::Team,
            error: None,
        };
    }
    let cached = cached(dir, keys);
    let fresh = fs::metadata(dir.join(FILE))
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e < FRESH));
    let mut fetched = None;
    if fetch && !fresh {
        let floor = cached.as_ref().map_or(i64::MIN, |c| c.1);
        match refresh(dir, &config.issuer, keys, floor) {
            Ok(got) => fetched = Some(Ok(got)),
            Err(e) => {
                eprintln!("relay: fetching {}: {e}", config.issuer);
                fetched = Some(Err(e));
            }
        }
    }
    let (got, error) = match (fetched, cached) {
        (Some(Ok(got)), _) => (Some(got), None),
        // A failed fetch: the cached copy serves.
        (Some(Err(_)), Ok(c)) => (Some(c), None),
        (Some(Err(e)), Err(_)) => (None, Some(e.to_string())),
        (None, Ok(c)) => (Some(c), None),
        (None, Err(e)) => (None, e),
    };
    match got {
        Some((urls, _)) if !urls.is_empty() => List {
            urls,
            source: Source::Lightbulb,
            error,
        },
        _ => List {
            error,
            ..List::default()
        },
    }
}

/// The cached list and its `expires_at`; `Err(None)` when there is none, `Err(Some(why))`
/// when it does not verify.
fn cached(dir: &Path, keys: &[String]) -> std::result::Result<(Vec<String>, i64), Option<String>> {
    let (Ok(body), Ok(sig)) = (
        fs::read(dir.join(FILE)),
        fs::read_to_string(dir.join(SIG_FILE)),
    ) else {
        return Err(None);
    };
    parse(&body, &sig, keys).map_err(|e| Some(format!("{FILE}: {e}")))
}

/// Fetches `{issuer}/relays.json` and its signature; on a list that verifies and expires no
/// earlier than the cached one's (`floor`: no rollback to an older list), caches both.
fn refresh(dir: &Path, issuer: &str, keys: &[String], floor: i64) -> Result<(Vec<String>, i64)> {
    let base = issuer.trim_end_matches('/');
    let text = |url: String| -> Result<String> {
        Ok(String::from_utf8(get(&url)?).map_err(|_| "not text")?)
    };
    let body = text(format!("{base}/{FILE}"))?;
    let sig = text(format!("{base}/{SIG_FILE}"))?;
    let got = parse(body.as_bytes(), &sig, keys)?;
    if got.1 < floor {
        return Err("older than the cached list".into());
    }
    // A crash between the two leaves a pair that does not verify: refetched next time.
    let mut r = crate::setup::AgentReport::default();
    crate::setup::write(&dir.join(FILE), &body, false, &mut r)?;
    crate::setup::write(&dir.join(SIG_FILE), &sig, false, &mut r)?;
    eprintln!("relay: fetched {} relays from {issuer}", got.0.len());
    Ok(got)
}

fn get(url: &str) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .into();
    let mut body = agent
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .into_body();
    Ok(body
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
        .map_err(|e| e.to_string())?)
}

/// The relays of `body` when `sig` is a minisign signature of it by one of `keys`, and the
/// whole body is a valid list: refused whole otherwise, never a partial list.
pub fn verify(body: &[u8], sig: &str, keys: &[String]) -> Result<Vec<String>> {
    parse(body, sig, keys).map(|(urls, _)| urls)
}

fn parse(body: &[u8], sig: &str, keys: &[String]) -> Result<(Vec<String>, i64)> {
    let sig = Signature::decode(sig).map_err(|_| "no signature")?;
    let signed = keys
        .iter()
        .any(|k| PublicKey::from_base64(k).is_ok_and(|pk| pk.verify(body, &sig, false).is_ok()));
    if !signed {
        return Err("bad signature".into());
    }
    let l: Signed = serde_json::from_slice(body).map_err(|_| "not a relay list")?;
    let valid = |u: &String| u.parse::<iroh::RelayUrl>().is_ok();
    if l.v != 1 || !l.relays.iter().all(valid) {
        return Err("not a relay list".into());
    }
    Ok((l.relays, l.expires_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_then_team_win_and_no_list_is_direct_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            issuer: "http://127.0.0.1:9/connect".into(),
            ..Config::default()
        };
        let mut team = Some("https://team.example".into());
        config.relay = Some("https://mine.example".into());
        let l = list(dir.path(), &config, &team, &[], true);
        assert_eq!(
            (l.source, l.urls),
            (Source::Config, vec!["https://mine.example".into()])
        );
        // An empty value is none (`relay --clear`).
        config.relay = Some(String::new());
        let l = list(dir.path(), &config, &team, &[], true);
        assert_eq!(l.source, Source::Team);
        team = Some(" ".into());
        // Ours: the issuer is down and nothing is cached.
        let l = list(dir.path(), &config, &team, &[], true);
        assert_eq!((l.source, l.urls.len()), (Source::None, 0));
        assert!(l.error.is_some());
        // Without `fetch`, nothing is tried and nothing is wrong.
        assert_eq!(
            list(dir.path(), &config, &team, &[], false),
            List::default()
        );
    }
}
