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
//! only. Every relay URL, from any source, passes `parse` (https only) or is ignored.
//!
//! What is known of our list (`State`) is read from disk and verified once, at start; after
//! that only a fetch changes it (and writes the files), so `list` is a pure function.
//!
//! The fetch uses `ureq`: blocking, small and rustls-only; it runs on a blocking thread,
//! where `reqwest::blocking` would start a second runtime per fetch.

use crate::Result;
use iroh::RelayUrl;
use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

pub const FILE: &str = "relays.json";
pub const SIG_FILE: &str = "relays.json.minisig";
/// The `expires_at` of the newest list ever verified here, apart from the cached body: a
/// cache that no longer verifies cannot lower it (no rollback to an older signed list).
pub const FLOOR_FILE: &str = "relays.floor";
/// A signed list with more relays than this is refused whole.
pub const MAX_RELAYS: usize = 8;
/// A cached list younger than this is used without asking the issuer.
const FRESH: Duration = Duration::from_secs(86_400);
/// A failed fetch is tried again after this.
pub const RETRY: Duration = Duration::from_secs(3_600);
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

/// The relays in effect, and (for ours) why our list was refused, until a fetch succeeds.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct List {
    pub urls: Vec<RelayUrl>,
    pub source: Source,
    pub error: Option<String>,
}

/// What is known of our list: the verified cache, the rollback floor, the last fetch.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// The verified list, its `expires_at`, and when it was fetched (the file's mtime).
    pub cache: Option<(Vec<RelayUrl>, i64, SystemTime)>,
    /// The newest `expires_at` verified here (`FLOOR_FILE`, or the cache's).
    pub floor: i64,
    /// Why the cache or the last fetch was refused; cleared by a fetch that succeeds.
    pub last_fetch_error: Option<String>,
    pub last_fetch_at: Option<Instant>,
}

/// `url` as a relay when it may be one: `https` with a host and no userinfo, query or
/// fragment. `http` only in debug builds (the tests' loopback relay): a plain-TCP relay
/// would show member keys and who talks to whom on the path.
pub fn parse(url: &str) -> Option<RelayUrl> {
    let u: RelayUrl = url.trim().parse().ok()?;
    let scheme = u.scheme() == "https" || (cfg!(debug_assertions) && u.scheme() == "http");
    let ok = scheme
        && u.host_str().is_some_and(|h| !h.is_empty())
        && u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none();
    ok.then_some(u)
}

/// The relays for `config_relay` (`config.toml: relay`), then the team's own `team` (the
/// roster's `meta {relay}`), then ours (`state`). An empty or invalid setting is skipped.
pub fn list(config_relay: &Option<String>, team: &Option<String>, state: &State) -> List {
    let own = |v: &Option<String>| v.as_deref().and_then(parse);
    let (urls, source) = if let Some(u) = own(config_relay) {
        (vec![u], Source::Config)
    } else if let Some(u) = own(team) {
        (vec![u], Source::Team)
    } else {
        let urls = state
            .cache
            .as_ref()
            .map(|c| c.0.clone())
            .unwrap_or_default();
        let source = if urls.is_empty() {
            Source::None
        } else {
            Source::Lightbulb
        };
        let error = state.last_fetch_error.clone();
        return List {
            urls,
            source,
            error,
        };
    };
    List {
        urls,
        source,
        error: None,
    }
}

impl State {
    /// The state in `dir`: its cache verified against `keys` once.
    pub fn load(dir: &Path, keys: &[String]) -> State {
        let floor_file = fs::read_to_string(dir.join(FLOOR_FILE)).unwrap_or_default();
        let mut s = State {
            cache: None,
            floor: floor_file.trim().parse().unwrap_or_default(),
            ..State::default()
        };
        let read = (
            fs::read(dir.join(FILE)),
            fs::read_to_string(dir.join(SIG_FILE)),
        );
        if let (Ok(body), Ok(sig)) = read {
            match verify(&body, &sig, keys) {
                Ok((urls, at)) => {
                    let mtime = fs::metadata(dir.join(FILE)).and_then(|m| m.modified());
                    s.floor = s.floor.max(at);
                    s.cache = Some((urls, at, mtime.unwrap_or(SystemTime::UNIX_EPOCH)));
                }
                Err(e) => s.last_fetch_error = Some(format!("{FILE}: {e}")),
            }
        }
        s
    }

    /// Fetches ours from `issuer` when ours is in effect (`config_relay` and `team` unset:
    /// otherwise nothing of ours is contacted) and due: the cache is a day old (or none) and
    /// no fetch was tried this hour. Blocking. Whether it fetched.
    pub fn fetch_if_due(
        &mut self,
        dir: &Path,
        issuer: &str,
        (config_relay, team): (&Option<String>, &Option<String>),
        keys: &[String],
    ) -> bool {
        let ours = [config_relay, team]
            .iter()
            .all(|v| v.as_deref().and_then(parse).is_none());
        let stale = (self.cache.as_ref()).is_none_or(|c| !c.2.elapsed().is_ok_and(|e| e < FRESH));
        let retry = self.last_fetch_at.is_none_or(|t| t.elapsed() >= RETRY);
        if !(ours && stale && retry) {
            return false;
        }
        self.last_fetch_at = Some(Instant::now());
        match self.refresh(dir, issuer, keys) {
            Ok(()) => self.last_fetch_error = None,
            Err(e) => {
                eprintln!("relay: fetching {issuer}: {e}");
                self.last_fetch_error = Some(e.to_string());
            }
        }
        true
    }

    /// Fetches `{issuer}/relays.json` and its signature; on a list that verifies and expires no
    /// earlier than the floor (no rollback), caches both and raises the floor.
    fn refresh(&mut self, dir: &Path, issuer: &str, keys: &[String]) -> Result<()> {
        let mut r = crate::setup::AgentReport::default();
        // A cache placed by hand raised the floor only in memory: kept, whatever the fetch.
        if self.cache.is_some() {
            crate::setup::write(
                &dir.join(FLOOR_FILE),
                &self.floor.to_string(),
                false,
                &mut r,
            )?;
        }
        let base = issuer.trim_end_matches('/');
        let text = |url: String| -> Result<String> {
            Ok(String::from_utf8(get(&url)?).map_err(|_| "not text")?)
        };
        let body = text(format!("{base}/{FILE}"))?;
        let sig = text(format!("{base}/{SIG_FILE}"))?;
        let (urls, at) = verify(body.as_bytes(), &sig, keys)?;
        if at < self.floor {
            return Err("older than the cached list".into());
        }
        // A crash between the two leaves a pair that does not verify: refetched next time.
        crate::setup::write(&dir.join(FILE), &body, false, &mut r)?;
        crate::setup::write(&dir.join(SIG_FILE), &sig, false, &mut r)?;
        crate::setup::write(&dir.join(FLOOR_FILE), &at.to_string(), false, &mut r)?;
        eprintln!("relay: fetched {} relays from {issuer}", urls.len());
        self.floor = at;
        self.cache = Some((urls, at, SystemTime::now()));
        Ok(())
    }
}

fn get(url: &str) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        // No redirect (to http, or anywhere); https only, but for the tests' loopback issuer.
        .max_redirects(0)
        .https_only(!cfg!(debug_assertions))
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

/// The relays of `body` and its `expires_at`, when `sig` is a minisign signature of it by one
/// of `keys` and the whole body is a valid list (at most `MAX_RELAYS`, each one `parse`s):
/// refused whole otherwise, never a partial list.
pub fn verify(body: &[u8], sig: &str, keys: &[String]) -> Result<(Vec<RelayUrl>, i64)> {
    let sig = Signature::decode(sig).map_err(|_| "no signature")?;
    let signed = keys
        .iter()
        .any(|k| PublicKey::from_base64(k).is_ok_and(|pk| pk.verify(body, &sig, false).is_ok()));
    if !signed {
        return Err("bad signature".into());
    }
    let l: Signed = serde_json::from_slice(body).map_err(|_| "not a relay list")?;
    let urls: Option<Vec<RelayUrl>> = l.relays.iter().map(|u| parse(u)).collect();
    match urls {
        Some(urls) if l.v == 1 && urls.len() <= MAX_RELAYS => Ok((urls, l.expires_at)),
        _ => Err("not a relay list".into()),
    }
}

#[derive(Deserialize)]
struct Signed {
    v: u32,
    relays: Vec<String>,
    expires_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_then_team_win_and_no_list_is_direct_only() {
        let u = |s: &str| Some(s.to_string());
        let mut s = State::default();
        let l = list(&u("https://mine.example"), &u("https://team.example"), &s);
        assert_eq!(
            (l.source, l.urls),
            (Source::Config, vec![parse("https://mine.example").unwrap()])
        );
        // An empty value is none (`relay --clear`); an invalid one is skipped.
        assert_eq!(
            list(&u(""), &u("https://team.example"), &s).source,
            Source::Team
        );
        assert_eq!(
            list(&u("ftp://mine.example"), &u("https://t.example"), &s).source,
            Source::Team
        );
        assert_eq!(list(&None, &u("garbage"), &s), List::default());
        // Ours: the issuer is down and nothing is cached; the error stays until a fetch ends it.
        let dir = tempfile::tempdir().unwrap();
        assert!(s.fetch_if_due(
            dir.path(),
            "http://127.0.0.1:9/connect",
            (&None, &None),
            &[]
        ));
        let l = list(&None, &None, &s);
        assert_eq!((l.source, l.urls.len()), (Source::None, 0));
        assert!(l.error.is_some());
        // Retried only after an hour; with a relay of our own, never.
        assert!(!s.fetch_if_due(
            dir.path(),
            "http://127.0.0.1:9/connect",
            (&None, &None),
            &[]
        ));
        let mut s = State::default();
        assert!(!s.fetch_if_due(
            dir.path(),
            "http://127.0.0.1:9/connect",
            (&u("https://t.example"), &None),
            &[]
        ));
        assert_eq!(list(&u("https://t.example"), &None, &s).error, None);
    }

    #[test]
    fn only_https_relay_urls_are_valid() {
        assert!(parse("https://relay.example").is_some());
        assert!(parse("https://relay.example:443/").is_some());
        assert_eq!(
            parse("http://127.0.0.1:3340").is_some(),
            cfg!(debug_assertions)
        );
        for bad in [
            "ftp://relay.example",
            "file:///etc/passwd",
            "ws://relay.example",
            "https://evil@relay.example",
            "https://u:p@relay.example",
            "https://relay.example/?x=1",
            "https://relay.example/#f",
            "relay.example",
            "",
        ] {
            assert!(parse(bad).is_none(), "{bad}");
        }
    }
}
