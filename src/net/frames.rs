//! The wire: one `Frame` per message, a u32 big-endian length then that many bytes of JSON
//! tagged by `t`, at most 1 MiB. The one definition of every frame later tasks send.

use crate::daemon::{ipc, Daemon};
use crate::doing::TEXT_MAX;
use crate::frame::Sender;
use crate::mail::State;
use crate::roster::{Entry, Member};
use crate::sessions::Tier;
use crate::{presence, sessions, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 1 << 20;

/// The reasons a peer may give for `refused`.
pub const REASONS: [&str; 6] = [
    "loop",
    "policy",
    "declined",
    "not_member",
    "request_id_reused",
    "unsupported",
];

/// A session as a peer publishes it (Task 7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    pub session_id: String,
    pub agent: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub folder: String,
    pub status: String,
    pub doing: Option<String>,
    pub tier: String,
    pub title: Option<String>,
}

/// A remote session's `status`: busy (`Status::Live`) or idle.
pub const STATUSES: [&str; 2] = ["busy", "idle"];

impl Remote {
    /// Cut to what a peer accepts (`valid`): text at most `TEXT_MAX` characters.
    pub fn clamp(mut self) -> Remote {
        let cut = |t: &mut String| *t = t.chars().take(TEXT_MAX).collect();
        cut(&mut self.folder);
        let texts = [
            &mut self.doing,
            &mut self.title,
            &mut self.repo,
            &mut self.branch,
        ];
        texts.into_iter().flatten().for_each(cut);
        self
    }

    /// Whether member `peer_short` may publish this: its session id under that prefix, `agent`
    /// the id's provider, a known `status` and `tier`, text at most `TEXT_MAX` characters.
    pub fn valid(&self, peer_short: &str) -> bool {
        let short = |t: &str| t.chars().count() <= TEXT_MAX;
        own(peer_short, &self.session_id)
            && Sender::of(peer_short, &self.session_id) == Sender::Agent(&self.agent)
            && STATUSES.contains(&self.status.as_str())
            && Tier::ALL.iter().any(|t| t.as_str() == self.tier)
            && short(&self.folder)
            && [&self.doing, &self.title, &self.repo, &self.branch]
                .into_iter()
                .all(|t| t.as_deref().is_none_or(short))
    }
}

/// Whether `a` is a well-formed address (`sessions::qualify`, which bounds its parts) under
/// member `short`'s prefix.
fn own(short: &str, a: &str) -> bool {
    sessions::qualify(short, a).as_deref() == Some(a)
        && sessions::split_addr(a).is_some_and(|(m, _)| m == short)
}

/// Transient frames carry no `sig`: the QUIC handshake authenticates the peer.
/// ponytail: sign envelopes if they are ever forwarded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    Hello {
        protocol: u32,
        heads: Vec<String>,
    },
    /// Each entry carries its own `sig`.
    Roster {
        entries: Vec<Entry>,
        more: bool,
    },
    Presence {
        member: String,
        /// Unix milliseconds at send: a receiver ignores one no newer than the last it stored.
        seq: u64,
        sessions: Vec<Remote>,
        known: Vec<String>,
        kept: Vec<String>,
    },
    Envelope {
        id: String,
        /// The sender's id, the one the receiving agent sees.
        request_id: String,
        kind: String,
        from: String,
        to: String,
        reply_to: Option<String>,
        thread: Option<String>,
        hops: u32,
        body: String,
        files: Vec<String>,
        sent_at: i64,
    },
    Ack {
        id: String,
        state: String,
        reason: Option<String>,
    },
    Receipt {
        id: String,
        state: String,
        reason: Option<String>,
    },
    HoldNotice {
        hold_id: String,
        target_session: String,
        from: String,
        from_label_parts: Vec<String>,
        preview: String,
        created_at: i64,
    },
    Decision {
        hold_id: String,
        holding_member: String,
        verdict: String,
        scope: String,
        nonce: String,
        decided_at: i64,
        sig: String,
    },
    JoinRequest {
        ticket: String,
        secret_proof: String,
        candidate: String,
        name: String,
        computer: String,
        kind: String,
        proof: Option<String>,
        sig: String,
    },
    JoinDecision {
        ticket_id: String,
        candidate: String,
        verdict: String,
        reason: Option<String>,
        sig: String,
    },
    ShareGrant {
        grant_id: String,
        session: String,
        viewer: String,
        input: bool,
        expires_at: i64,
        granter: String,
        sig: String,
    },
    Stream {
        grant_id: String,
        session: String,
        seq: u32,
        /// Base64, at most 65 536 bytes decoded.
        bytes: String,
    },
}

impl Frame {
    /// The `t` tag, for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Frame::Hello { .. } => "hello",
            Frame::Roster { .. } => "roster",
            Frame::Presence { .. } => "presence",
            Frame::Envelope { .. } => "envelope",
            Frame::Ack { .. } => "ack",
            Frame::Receipt { .. } => "receipt",
            Frame::HoldNotice { .. } => "hold_notice",
            Frame::Decision { .. } => "decision",
            Frame::JoinRequest { .. } => "join_request",
            Frame::JoinDecision { .. } => "join_decision",
            Frame::ShareGrant { .. } => "share_grant",
            Frame::Stream { .. } => "stream",
        }
    }
}

pub async fn write(send: &mut (impl AsyncWrite + Unpin), f: &Frame) -> Result<()> {
    let body = serde_json::to_vec(f)?;
    if body.len() > MAX_FRAME {
        return Err(format!("a {} frame of {} bytes", f.kind(), body.len()).into());
    }
    send.write_all(&(body.len() as u32).to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

pub async fn read(recv: &mut (impl AsyncRead + Unpin)) -> Result<Frame> {
    let len = recv.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(format!("a frame of {len} bytes").into());
    }
    let mut body = vec![0; len];
    recv.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// The one per-frame check on what a member sends. `roster` entries are checked by
/// `roster::accept` itself. A `presence` is the peer's own: at most `presence::MAX` of each
/// list, every session `Remote::valid` for it and every address under its prefix, a `seq` at
/// most a day ahead of now. An `envelope` is checked by `mail::receive`. An `ack` or `receipt`
/// names a state past `queued`, with a known reason exactly when it is `refused` (that it
/// is bound to this member is `outbox::acked`'s check). Task 14 adds the rules for the frames
/// it serves, and until then every other kind is dropped.
pub fn from_peer_ok(f: &Frame, peer: &Member, _daemon: &Daemon) -> bool {
    match f {
        Frame::Hello { .. } | Frame::Roster { .. } | Frame::Envelope { .. } => true,
        Frame::Ack { state, reason, .. } | Frame::Receipt { state, reason, .. } => {
            let refused = state == "refused";
            State::parse(state).is_ok_and(|s| s != State::Queued)
                && reason
                    .as_deref()
                    .map_or(!refused, |r| refused && REASONS.contains(&r))
        }
        Frame::Presence {
            member,
            seq,
            sessions,
            known,
            kept,
        } => {
            *member == peer.key
                && *seq <= ipc::now_ms() + 86_400_000
                && [sessions.len(), known.len(), kept.len()]
                    .iter()
                    .all(|n| *n <= presence::MAX)
                && sessions.iter().all(|s| s.valid(&peer.short))
                && known.iter().chain(kept).all(|a| own(&peer.short, a))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_cap_at_one_mib() {
        let (mut a, mut b) = tokio::io::duplex(1 << 21);
        let hello = Frame::Hello {
            protocol: 1,
            heads: vec!["h".into()],
        };
        write(&mut a, &hello).await.unwrap();
        assert_eq!(read(&mut b).await.unwrap(), hello);
        let wire = serde_json::to_value(&hello).unwrap();
        assert_eq!(wire["t"], "hello");
        let big = Frame::Stream {
            grant_id: String::new(),
            session: String::new(),
            seq: 0,
            bytes: "x".repeat(MAX_FRAME),
        };
        assert!(write(&mut a, &big).await.is_err());
        // A length past the cap is refused before its body is read.
        a.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        assert!(read(&mut b).await.is_err());
    }
}
