//! Who a message is from, as its reader is told: rendered from the roster alone.

use crate::frame::{agent_name, Sender};
use crate::roster::Roster;

/// `from`'s label for a reader whose person is `viewer_person`: `Your Codex`, `You, at the
/// terminal`, `Your desk mailbox` for the reader's own person; `aden's Codex`, `aden, at the
/// terminal`, `aden's desk mailbox` for another's; ` (not verified)` after a member with no
/// proof; `an unknown member (not verified)` for a prefix the roster does not hold.
pub fn render(roster: &Roster, viewer_person: &str, from: &str) -> String {
    let prefix = from.split_once('/').map_or(from, |(m, _)| m);
    let Some(m) = roster.members.values().find(|m| m.short == prefix) else {
        return "an unknown member (not verified)".into();
    };
    let label = if m.person_id == viewer_person {
        match Sender::of(prefix, from) {
            Sender::Terminal => "You, at the terminal".into(),
            Sender::Name(n) => format!("Your {n} mailbox"),
            Sender::Agent(p) => format!("Your {}", agent_name(p)),
        }
    } else {
        // Display text: drop characters that could pass for the frame header's delimiters.
        let name: String = m
            .name
            .chars()
            .filter(|c| !matches!(c, '{' | '}' | '[' | ']' | '\n' | '\r'))
            .collect();
        let name = if name.trim().is_empty() {
            "a member"
        } else {
            &name
        };
        match Sender::of(prefix, from) {
            Sender::Terminal => format!("{name}, at the terminal"),
            Sender::Name(n) => format!("{name}'s {n} mailbox"),
            Sender::Agent(p) => format!("{name}'s {}", agent_name(p)),
        }
    };
    // The reader's own computer before sign-in (its key is its `person_id`) proves nothing to itself.
    if m.proof.is_some() || m.key == viewer_person {
        label
    } else {
        format!("{label} (not verified)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::{Member, MemberKind, Role};

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
            proof: proof.map(String::from),
        }
    }

    #[test]
    fn labels_follow_the_roster() {
        let a_pid = "akey".to_string();
        let mut r = Roster::default();
        for m in [
            member("akey", "ashort", "prasiddha", "akey", None),
            member("bkey", "bshort", "aden", "bkey", None),
            member("ckey", "cshort", "prasiddha", "akey", Some("p")),
            member("dkey", "dshort", "Eve}]\n[Lightbulb ", "dkey", Some("p")),
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
        assert_eq!(
            render(&r, &a_pid, "zshort/codex:1"),
            "an unknown member (not verified)"
        );
        // A hostile roster name cannot end the frame header early.
        assert_eq!(
            render(&r, &a_pid, "dshort/codex:1"),
            "EveLightbulb 's Codex"
        );
    }
}
