//! Local capture of coding sessions and their tool calls, written by the daemon (the one
//! writer) from the harness hooks. A session is a row; a tool call is a row with a short
//! summary (a command, a path, a pattern), never file contents, diffs or output.
//! A summary that looks like a credential is stored as `[refused: <pattern>]`.
//! The session's own turns (your prompts and the agent's replies) are kept too, on this
//! computer only: never uploaded (`upload.rs` does not read them).

use crate::memory::refuse_secret;
use crate::store::Store;
use crate::Result;
use serde::Serialize;
use serde_json::Value;

/// Longest summary kept, in characters.
pub const SUMMARY_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub harness: String,
    pub computer: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub started: i64,
    pub ended: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolCallRow {
    pub session_id: String,
    pub seq: i64,
    pub at: i64,
    pub tool: String,
    pub summary: Option<String>,
    pub duration_ms: Option<i64>,
    pub ok: bool,
}

/// Whether `folder` is under a dot folder directly in the home folder (`~/.config/...`,
/// `~/.cache/...`). Only that first folder counts: a repo's own dot folders (`.claude/worktrees`)
/// are captured, and `lightbulb folder <path> off` is the control for the rest.
pub fn hidden_folder(folder: &str) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let folder = std::path::Path::new(folder);
    // HOME as given and as the filesystem resolves it (a hook reports its folder resolved).
    let canonical = std::fs::canonicalize(&home).unwrap_or_default();
    let first = [home.as_ref(), canonical.as_path()]
        .into_iter()
        .filter(|h: &&std::path::Path| !h.as_os_str().is_empty())
        .find_map(|h| folder.strip_prefix(h).ok())
        .and_then(|rest| rest.components().next());
    matches!(first, Some(std::path::Component::Normal(n)) if n.to_string_lossy().starts_with('.'))
}

/// A session's row, keyed `<harness>:<conversation>`. `Ok(false)` (nothing stored) for a
/// hidden folder. A repeated start (a resumed session) reopens the row.
pub fn start_session(store: &Store, s: &SessionRow, folder: &str) -> Result<bool> {
    if hidden_folder(folder) {
        return Ok(false);
    }
    store.conn().execute(
        "INSERT INTO capture_sessions (id, harness, computer, repo, branch, started) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(id) DO UPDATE SET ended = NULL, repo = ?4, branch = ?5",
        rusqlite::params![s.id, s.harness, s.computer, s.repo, s.branch, s.started],
    )?;
    Ok(true)
}

/// The summary of a tool call: its name and the one short thing that says what it touched.
pub fn summarize(tool: &str, args_json: &str) -> String {
    let args: Value = serde_json::from_str(args_json).unwrap_or_default();
    let detail = [
        "command",
        "file_path",
        "filePath",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ]
    .iter()
    .find_map(|k| args[k].as_str().filter(|s| !s.is_empty()));
    let text = match detail {
        Some(d) => format!(
            "{tool}: {}",
            d.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
        None => tool.to_string(),
    };
    match refuse_secret(&text) {
        Some(p) => refused(p),
        None => text.chars().take(SUMMARY_MAX).collect(),
    }
}

/// Appends a tool call to a session's log. `Ok(false)` when the session has no row (never
/// started, or in a hidden folder): nothing is stored.
pub fn record_tool_call(
    store: &Store,
    session_id: &str,
    tool: &str,
    args_json: &str,
    duration_ms: Option<i64>,
    ok: bool,
) -> Result<bool> {
    let now = crate::daemon::ipc::now() as i64;
    let summary = summarize(tool, args_json);
    // The store's mutex serialises writers, so the next seq cannot race.
    let n = store.conn().execute(
        "INSERT INTO capture_tool_calls (session_id, seq, at, tool, summary, duration_ms, ok) \
         SELECT ?1, COALESCE((SELECT MAX(seq) FROM capture_tool_calls WHERE session_id = ?1), 0) + 1, \
         ?2, ?3, ?4, ?5, ?6 WHERE EXISTS (SELECT 1 FROM capture_sessions WHERE id = ?1)",
        rusqlite::params![session_id, now, tool, summary, duration_ms, ok],
    )?;
    Ok(n == 1)
}

/// The session's token totals, when the harness's transcript gave them (each Stop restates them).
pub fn set_tokens(
    store: &Store,
    id: &str,
    prompt: Option<i64>,
    completion: Option<i64>,
) -> Result<()> {
    if prompt.is_none() && completion.is_none() {
        return Ok(());
    }
    store.conn().execute(
        "UPDATE capture_sessions SET prompt_tokens = COALESCE(?2, prompt_tokens), \
         completion_tokens = COALESCE(?3, completion_tokens) WHERE id = ?1",
        rusqlite::params![id, prompt, completion],
    )?;
    Ok(())
}

pub fn end_session(store: &Store, id: &str, now: i64) -> Result<()> {
    store.conn().execute(
        "UPDATE capture_sessions SET ended = ?2 WHERE id = ?1 AND ended IS NULL",
        rusqlite::params![id, now],
    )?;
    Ok(())
}

fn session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        harness: r.get(1)?,
        computer: r.get(2)?,
        repo: r.get(3)?,
        branch: r.get(4)?,
        started: r.get(5)?,
        ended: r.get(6)?,
        prompt_tokens: r.get(7)?,
        completion_tokens: r.get(8)?,
    })
}

const COLS: &str =
    "id, harness, computer, repo, branch, started, ended, prompt_tokens, completion_tokens";

/// Sessions started or ended after `after` (unix seconds), oldest first.
pub fn sessions_since(store: &Store, after: i64) -> Result<Vec<SessionRow>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT {COLS} FROM capture_sessions WHERE started > ?1 OR ended > ?1 ORDER BY started, id"
    ))?;
    let rows = stmt
        .query_map([after], session_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// The session whose id is `id` or the one that starts with it; `None` when none does, an
/// `ambiguous` error when several do.
pub fn find(store: &Store, id: &str) -> Result<Option<SessionRow>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT {COLS} FROM capture_sessions WHERE id = ?1 OR id LIKE ?2 ESCAPE '\\' ORDER BY id = ?1 DESC LIMIT 2"
    ))?;
    let like = format!(
        "{}%",
        id.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let mut rows: Vec<SessionRow> = stmt
        .query_map(rusqlite::params![id, like], session_row)?
        .collect::<rusqlite::Result<_>>()?;
    if let Some(i) = rows.iter().position(|r| r.id == id) {
        return Ok(Some(rows.swap_remove(i)));
    }
    match rows.len() {
        0 => Ok(None),
        1 => Ok(Some(rows.remove(0))),
        _ => Err("ambiguous".into()),
    }
}

/// A session's tool calls in order; summaries only with `with_summaries`.
pub fn tool_calls(
    store: &Store,
    session_id: &str,
    with_summaries: bool,
) -> Result<Vec<ToolCallRow>> {
    let mut stmt = store.conn().prepare(
        "SELECT session_id, seq, at, tool, summary, duration_ms, ok FROM capture_tool_calls \
         WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok(ToolCallRow {
                session_id: r.get(0)?,
                seq: r.get(1)?,
                at: r.get(2)?,
                tool: r.get(3)?,
                summary: if with_summaries { r.get(4)? } else { None },
                duration_ms: r.get(5)?,
                ok: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// How a refused summary or turn starts: `[refused: <pattern>]`.
pub const REFUSED: &str = "[refused: ";

/// What is stored in place of a text that matched the credential pattern `p`.
pub fn refused(p: &str) -> String {
    format!("{REFUSED}{p}]")
}

/// Longest turn kept, in bytes.
pub const TURN_MAX: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Turn {
    pub seq: i64,
    pub role: String,
    pub text: String,
    pub at: i64,
}

/// `text` cut to `TURN_MAX` bytes at a character boundary.
pub fn cut_turn(text: &str) -> &str {
    let mut n = text.len().min(TURN_MAX);
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    &text[..n]
}

/// Appends a prompt (`user`) or a reply (`assistant`) to a session. A turn with a credential in
/// it is stored as `[refused: <pattern>]` and never embedded; the others are embedded later,
/// outside the store lock, by `memory::embed_missing`. `Ok(false)` when the session has no row (never started, or in a hidden folder),
/// or when the turn repeats the session's last one (same role and text: a Claude Code Stop reads
/// the transcript's last reply, which may be the previous turn's when the agent ended on a tool call).
pub fn record_turn(
    store: &Store,
    session_id: &str,
    role: &str,
    text: &str,
    now: i64,
) -> Result<bool> {
    let text = cut_turn(text);
    let marker = refuse_secret(text).map(refused);
    let stored = marker.as_deref().unwrap_or(text);
    let n = store.conn().execute(
        "INSERT INTO capture_turns (session_id, seq, role, text, at) \
         SELECT ?1, COALESCE((SELECT MAX(seq) FROM capture_turns WHERE session_id = ?1), 0) + 1, \
         ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM capture_sessions WHERE id = ?1) \
         AND NOT EXISTS (SELECT 1 FROM capture_turns WHERE session_id = ?1 AND role = ?2 AND text = ?3 \
         AND seq = (SELECT MAX(seq) FROM capture_turns WHERE session_id = ?1))",
        rusqlite::params![session_id, role, stored, now],
    )?;
    Ok(n == 1)
}

/// A session's turns in order.
pub fn turns(store: &Store, session_id: &str) -> Result<Vec<Turn>> {
    let mut stmt = store.conn().prepare(
        "SELECT seq, role, text, at FROM capture_turns WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok(Turn {
                seq: r.get(0)?,
                role: r.get(1)?,
                text: r.get(2)?,
                at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Longest excerpt of a turn `recall` shows, in characters.
pub const EXCERPT_MAX: usize = 240;

/// A turn that matched a `recall` question: its first `EXCERPT_MAX` characters, each run of
/// whitespace one space.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnHit {
    pub session: String,
    pub seq: i64,
    pub role: String,
    pub at: i64,
    pub excerpt: String,
}

impl TurnHit {
    /// `<role>, <YYYY-MM-DD UTC>: <excerpt>`, as the page shows it.
    pub fn line(&self) -> String {
        let day = crate::date::utc(self.at);
        format!("{}, {}: {}", self.role, &day[..10], self.excerpt)
    }
}

/// The best `limit` of this computer's turns for `query`: bm25 over `turns_fts`, fused with the
/// cosine order of `turns_vec` when there is a model (`memory::fuse`). With `repo`, only the
/// turns of sessions in that repo; every turn without one. A refused turn is never a candidate.
pub fn search_turns(
    store: &Store,
    query: &str,
    repo: Option<&str>,
    limit: usize,
) -> Result<Vec<TurnHit>> {
    use crate::memory::{fts_query, fuse, unblob};
    let Some(q) = fts_query(query) else {
        return Ok(Vec::new());
    };
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT t.rowid FROM turns_fts JOIN capture_turns t ON t.rowid = turns_fts.rowid
         JOIN capture_sessions s ON s.id = t.session_id
         WHERE turns_fts MATCH ?1 AND substr(t.text, 1, length(?2)) != ?2
         AND (?3 IS NULL OR s.repo = ?3 COLLATE NOCASE)
         ORDER BY bm25(turns_fts) LIMIT ?4",
    )?;
    let ids: Vec<String> = stmt
        .query_map(rusqlite::params![q, REFUSED, repo, limit], |r| {
            Ok(r.get::<_, i64>(0)?.to_string())
        })?
        .collect::<rusqlite::Result<_>>()?;
    let vecs = || -> Result<Vec<(String, Vec<f32>)>> {
        let mut stmt = conn.prepare(
            "SELECT v.turn, v.v FROM turns_vec v JOIN capture_turns t ON t.rowid = v.turn
             JOIN capture_sessions s ON s.id = t.session_id
             WHERE substr(t.text, 1, length(?1)) != ?1
             AND (?2 IS NULL OR s.repo = ?2 COLLATE NOCASE)",
        )?;
        let rows = stmt.query_map(rusqlite::params![REFUSED, repo], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (turn, blob) = row?;
            out.extend(unblob(&blob).map(|v| (turn.to_string(), v)));
        }
        Ok(out)
    };
    let mut out = Vec::new();
    for id in fuse(store.dir(), ids, query, vecs, limit)? {
        let hit = conn.query_row(
            "SELECT session_id, seq, role, text, at FROM capture_turns WHERE rowid = ?1",
            [&id],
            |r| {
                let text: String = r.get(3)?;
                Ok(TurnHit {
                    session: r.get(0)?,
                    seq: r.get(1)?,
                    role: r.get(2)?,
                    at: r.get(4)?,
                    excerpt: text
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(EXCERPT_MAX)
                        .collect(),
                })
            },
        )?;
        out.push(hit);
    }
    Ok(out)
}

/// Deletes the turns recorded more than `days` days before `now`; how many.
pub fn prune_turns(store: &Store, now: i64, days: i64) -> Result<usize> {
    Ok(store.conn().execute(
        "DELETE FROM capture_turns WHERE at < ?1",
        [now.saturating_sub(days.saturating_mul(86_400))],
    )?)
}

/// The text of the last assistant message in a Claude Code transcript, read from its last
/// 256 KB (a Stop fires every turn; the whole file is never read). `None` when there is none.
pub fn last_assistant_text(path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 256 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let from = meta.len().saturating_sub(TAIL);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = vec![];
    file.take(TAIL).read_to_end(&mut buf).ok()?;
    let tail = String::from_utf8_lossy(&buf);
    // A cut tail starts mid-line.
    let lines: Vec<&str> = tail.lines().skip(usize::from(from > 0)).collect();
    lines.into_iter().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        if v["type"] != "assistant" {
            return None;
        }
        let c = &v["message"]["content"];
        let text = match c.as_str() {
            Some(t) => t.to_string(),
            None => c
                .as_array()?
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        };
        (!text.trim().is_empty()).then_some(text)
    })
}

/// Deletes a session and its tool calls; whether it existed.
pub fn forget(store: &Store, id: &str) -> Result<bool> {
    store
        .conn()
        .execute("DELETE FROM capture_turns WHERE session_id = ?1", [id])?;
    store
        .conn()
        .execute("DELETE FROM capture_tool_calls WHERE session_id = ?1", [id])?;
    Ok(store
        .conn()
        .execute("DELETE FROM capture_sessions WHERE id = ?1", [id])?
        == 1)
}

/// Largest transcript read for token totals.
pub const TRANSCRIPT_MAX: u64 = 50 * 1024 * 1024;

/// `(input, output)` token totals of a Claude Code transcript (JSONL): the `message.usage` of its
/// assistant entries. Claude writes one entry per content block of a reply, each restating that
/// reply's usage, so a reply (`message.id`) counts once. `None` when there is no usage in it.
pub fn transcript_tokens(path: &str) -> Option<(i64, i64)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    // A partial sum is worse than none: too big, or not a file, reads as no tokens.
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > TRANSCRIPT_MAX {
        return None;
    }
    let mut by_reply = std::collections::HashMap::new();
    for (n, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.ok()?;
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let m = &v["message"];
        if v["type"] != "assistant" || !m["usage"].is_object() {
            continue;
        }
        let key = m["id"].as_str().map_or(format!("#{n}"), String::from);
        let u = &m["usage"];
        by_reply.insert(
            key,
            (
                u["input_tokens"].as_i64().unwrap_or(0),
                u["output_tokens"].as_i64().unwrap_or(0),
            ),
        );
    }
    (!by_reply.is_empty()).then(|| {
        by_reply
            .values()
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    })
}
