//! Relay fallback: which relays this computer uses. When two members cannot reach each other
//! directly, iroh carries their QUIC connection through a relay server, and hole punching
//! (which upgrades it to a direct path where it can) comes with the relays. QUIC is end to
//! end between the two member keys, so a relay forwards ciphertext only: nothing is ever
//! sent in plaintext, through a relay or otherwise.
//!
//! The list, in this order: `config.toml: relay`, else the team's own (the roster's
//! `meta {relay}`), else the default: the iroh project's public relays
//! (`iroh::defaults::prod`), so a person configures nothing. Every relay URL, from any
//! source, passes `parse` (https only) or is ignored. Nothing is fetched: `list` is a pure
//! function of the two settings. (A signed list of ours returns with real keys at the key
//! ceremony.)

use iroh::RelayUrl;
use serde::Serialize;

/// Where the list in effect came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// `config.toml: relay`.
    Config,
    /// The roster's `meta {relay}`.
    Team,
    /// The iroh project's public relays (`default_urls`).
    Default,
    /// No relay: direct only (only the tests' `LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS`).
    #[default]
    None,
}

/// The relays in effect and where they came from.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct List {
    pub urls: Vec<RelayUrl>,
    pub source: Source,
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

/// The iroh project's public relays, as the iroh crate names them.
pub fn default_urls() -> Vec<RelayUrl> {
    iroh::defaults::prod::default_relay_map().urls()
}

/// The relays for `config_relay` (`config.toml: relay`), then the team's own `team` (the
/// roster's `meta {relay}`), then `default_urls`. An empty or invalid setting is skipped.
pub fn list(config_relay: &Option<String>, team: &Option<String>) -> List {
    let own = |v: &Option<String>| v.as_deref().and_then(parse);
    let (urls, source) = if let Some(u) = own(config_relay) {
        (vec![u], Source::Config)
    } else if let Some(u) = own(team) {
        (vec![u], Source::Team)
    } else {
        (default_urls(), Source::Default)
    };
    List { urls, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_then_team_win_and_no_list_is_the_default() {
        let u = |s: &str| Some(s.to_string());
        let l = list(&u("https://mine.example"), &u("https://team.example"));
        assert_eq!(
            (l.source, l.urls),
            (Source::Config, vec![parse("https://mine.example").unwrap()])
        );
        // An empty value is none (`relay --clear`); an invalid one is skipped.
        assert_eq!(
            list(&u(""), &u("https://team.example")).source,
            Source::Team
        );
        assert_eq!(
            list(&u("ftp://mine.example"), &u("https://t.example")).source,
            Source::Team
        );
        let d = list(&None, &u("garbage"));
        assert_eq!((d.source, d.urls), (Source::Default, default_urls()));
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
