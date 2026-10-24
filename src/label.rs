//! Who a message is from, as its reader is told: rendered from the roster alone.

use crate::frame::{agent_name, Sender};
use crate::proof::ProofKind;
use crate::roster::{Member, MemberKind, Roster};
use crate::sessions::{member_of, split_addr};
use crate::store::Message;

/// Whether `c` is safe to show a person or an agent: no control character (C0, C1, U+0085),
/// line or paragraph separator, or bidi control that could reorder or hide what is around it.
pub fn printable(c: char) -> bool {
    !c.is_control()
        && !matches!(c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// `from`'s label for the reader at member `viewer` (its person is `Roster::person_of`):
/// `Your Codex`, `You, at the terminal`, `Your desk mailbox` for the reader's own person;
/// `aden's Codex`, `aden, at the terminal`, `aden's desk mailbox` for another's, `✓ Aden
/// Smith's Codex` when that member's proof is in force (its name);
/// ` (not verified)` after a member with no proof other than the reader's own computer;
/// `<Agent> · <workspace name>` for a session of a workspace member in force (`render_as`),
/// or a hosted app's declared title, `ChatGPT via Aden` (`via`);
/// `an unknown member (not verified)` for a prefix the roster does not hold.
pub fn render(roster: &Roster, viewer: &str, from: &str) -> String {
    render_as(roster, viewer, from, None)
}

/// `render`, `declared` the agent the sender's presence declares for `from` (a workspace
/// member's session: `Friday`); else the proof's agent of that name, else the session's name.
pub fn render_as(roster: &Roster, viewer: &str, from: &str, declared: Option<&str>) -> String {
    let prefix = member_of(from).unwrap_or(from);
    let Some(m) = roster.by_short(prefix) else {
        return "an unknown member (not verified)".into();
    };
    let ws = m.proof.as_ref().filter(|p| p.kind == ProofKind::Workspace);
    if let Some(p) = ws.filter(|_| m.kind == MemberKind::Cloud) {
        let rest = split_addr(from).map_or(from, |(_, r)| r);
        let named = p.agents.iter().find(|a| a.eq_ignore_ascii_case(rest));
        if let Some(title) = declared.filter(|t| via(t)) {
            return shown(title, 64, "an app");
        }
        let agent = declared.or(named.map(String::as_str)).unwrap_or(rest);
        let space = p.workspace.as_ref().map_or("", |w| w.name.as_str());
        // The agent may be the presence's, so it is cut; the proof's text is (`proof::parse`).
        return format!(
            "{} · {}",
            shown(agent, 32, "an agent"),
            shown(space, usize::MAX, "a workspace")
        );
    }
    let (who, possessive) = if m.person_id == roster.person_of(viewer) {
        ("You".to_string(), "Your".to_string())
    } else {
        let name = match &m.proof {
            Some(p) => format!("✓ {}", shown(&p.name, usize::MAX, "a member")),
            None => name(m),
        };
        (name.clone(), format!("{name}'s"))
    };
    let label = match Sender::of(prefix, from) {
        Sender::Terminal => format!("{who}, at the terminal"),
        Sender::Name(n) => format!("{possessive} {n} mailbox"),
        Sender::Agent(p) => format!("{possessive} {}", agent_name(p)),
    };
    if verified(m, viewer) {
        label
    } else {
        format!("{label} (not verified)")
    }
}

/// Whether a declared `title` is a hosted app's, `<App> via <Person>` (`app_send`): the
/// declaring workspace member is its trust root, so it renders as it is.
pub fn via(title: &str) -> bool {
    title
        .split_once(" via ")
        .is_some_and(|(app, who)| !app.trim().is_empty() && !who.trim().is_empty())
}

/// Whether member `m` reads as verified to the computer `own` (its key): it has a proof, or it is
/// `own`, which has nothing to prove to itself.
pub fn verified(m: &Member, own: &str) -> bool {
    m.proof.is_some() || m.key == own
}

/// `m`'s name as display text (`shown`).
pub fn name(m: &Member) -> String {
    shown(&m.name, usize::MAX, "a member")
}

/// `s`'s `printable` characters, at most `max` of them.
pub fn plain(s: &str, max: usize) -> String {
    s.chars().filter(|c| printable(*c)).take(max).collect()
}

/// `s` as display text (`plain`), without the characters that could pass for the frame
/// header's delimiters; `none` when nothing is left.
fn shown(s: &str, max: usize, none: &str) -> String {
    let s = plain(&s.replace(['{', '}', '[', ']'], ""), max);
    if s.trim().is_empty() {
        none.to_string()
    } else {
        s
    }
}

/// `m`'s sender as the agents of the computer `me` (key, short) are told it. Mail that came in
/// over the IPC (`origin` other than `net:`) is that computer's by construction, whatever
/// member prefix its `from` carries.
pub fn sender(roster: &Roster, me: (&str, &str), m: &Message, declared: Option<&str>) -> String {
    render_as(roster, me.0, &from(me.1, m), declared)
}

/// The address `m` is from as its reader at member `short` labels it (`sender`).
pub fn from(short: &str, m: &Message) -> String {
    if m.origin.as_deref().is_some_and(|o| o.starts_with("net:")) {
        m.from_key.clone()
    } else {
        let rest = split_addr(&m.from_key).map_or(m.from_key.as_str(), |(_, r)| r);
        format!("{short}/{rest}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof::{Proof, ProofWorkspace};
    use crate::roster::Role;

    fn proof(name: &str, kind: ProofKind) -> Proof {
        let workspace = Some(ProofWorkspace {
            id: "W1".into(),
            name: "The Lightbulb Company".into(),
        });
        Proof {
            iat: 0,
            exp: 1,
            sub: "s".into(),
            name: name.into(),
            computer: "c".into(),
            kind,
            workspace,
            agents: vec!["Friday".into(), "Engineering".into()],
            token: String::new(),
        }
    }

    fn member(key: &str, short: &str, name: &str, person: &str, proof: Option<&str>) -> Member {
        Member {
            key: key.into(),
            short: short.into(),
            name: name.into(),
            computer: "c".into(),
            person_id: person.into(),
            role: Role::Member,
            kind: MemberKind::Computer,
            admitted_at: 0,
            proof: proof.map(|n| self::proof(n, ProofKind::Computer)),
            workspace: None,
        }
    }

    #[test]
    fn labels_follow_the_roster() {
        let a_pid = "akey".to_string();
        let mut r = Roster::default();
        for m in [
            member("akey", "ashort", "prasiddha", "akey", None),
            member("bkey", "bshort", "aden", "bkey", None),
            member("ckey", "cshort", "prasiddha", "akey", Some("Prasiddha")),
            member("dkey", "dshort", "eve", "dkey", Some("Eve}]\n[Lightbulb ")),
        ] {
            r.members.insert(m.key.clone(), m);
        }
        assert_eq!(
            render(&r, &a_pid, "bshort/codex:1"),
            "aden's Codex (not verified)"
        );
        assert_eq!(
            render(&r, &a_pid, "bshort/terminal"),
            "aden, at the terminal (not verified)"
        );
        assert_eq!(
            render(&r, &a_pid, "bshort/desk"),
            "aden's desk mailbox (not verified)"
        );
        // The reader's own computer, and the same person's other (verified) computer.
        assert_eq!(render(&r, &a_pid, "ashort/claude:1"), "Your Claude Code");
        assert_eq!(
            render(&r, &a_pid, "ashort/terminal"),
            "You, at the terminal"
        );
        assert_eq!(render(&r, &a_pid, "cshort/desk"), "Your desk mailbox");
        assert_eq!(render(&r, &a_pid, "cshort/grok:x"), "Your Grok CLI");
        // The retired `<app>-app` session is a plain name.
        assert_eq!(
            render(&r, &a_pid, "ashort/claude-app"),
            "Your claude-app mailbox"
        );
        assert_eq!(
            render(&r, &a_pid, "zshort/codex:1"),
            "an unknown member (not verified)"
        );
        // A hostile roster name cannot end the frame header early.
        assert_eq!(
            render(&r, &a_pid, "dshort/codex:1"),
            "✓ EveLightbulb 's Codex"
        );
        let mut r2 = r.clone();
        r2.members.get_mut("bkey").unwrap().name = "Ev\u{202e}e\u{2028}\u{85}\u{1b}x".into();
        assert_eq!(
            render(&r2, &a_pid, "bshort/codex:1"),
            "Evex's Codex (not verified)"
        );
    }

    #[test]
    fn workspace_session_renders_agent_dot_workspace_name() {
        let a_pid = "akey".to_string();
        let mut r = Roster::default();
        let mut w = member("wkey", "wshort", "The Lightbulb Company", "ws:W1", None);
        w.kind = MemberKind::Cloud;
        w.proof = Some(proof("The Lightbulb Company", ProofKind::Workspace));
        r.members.insert(w.key.clone(), w);
        r.members
            .insert("akey".into(), member("akey", "ashort", "me", "akey", None));
        assert_eq!(
            render(&r, &a_pid, "wshort/friday"),
            "Friday · The Lightbulb Company"
        );
        // The presence's declared agent, sanitised and cut to 32 characters.
        let long = format!("Fri[day]\n{}", "x".repeat(40));
        let shown = render_as(&r, &a_pid, "wshort/friday", Some(&long));
        assert_eq!(
            shown,
            format!("Friday{} · The Lightbulb Company", "x".repeat(26))
        );
        // A hosted app's declared title, as it is; not one for another name or app.
        let app = |name: &str, t: &str| render_as(&r, &a_pid, &format!("wshort/{name}"), Some(t));
        assert_eq!(app("chatgpt-u1", "ChatGPT via Aden"), "ChatGPT via Aden");
        assert_eq!(app("claude-u1", "Claude.ai via Aden"), "Claude.ai via Aden");
        assert_eq!(
            app("chatgpt-u1", "ChatGPT via "),
            "ChatGPT via  · The Lightbulb Company"
        );
        // Lapsed: an unverified member's mailbox.
        r.members.get_mut("wkey").unwrap().proof = None;
        assert_eq!(
            render(&r, &a_pid, "wshort/friday"),
            "The Lightbulb Company's friday mailbox (not verified)"
        );
    }

    #[test]
    fn a_merged_unproven_viewer_is_verified_to_itself_only() {
        // The viewer `v` was merged into person `pkey`, the key of another (unproven) computer.
        let mut r = Roster::default();
        for m in [
            member("vkey", "vshort", "me", "pkey", None),
            member("pkey", "pshort", "me-too", "pkey", None),
        ] {
            r.members.insert(m.key.clone(), m);
        }
        assert_eq!(render(&r, "vkey", "vshort/codex:1"), "Your Codex");
        assert_eq!(
            render(&r, "vkey", "pshort/codex:1"),
            "Your Codex (not verified)"
        );
    }
}
