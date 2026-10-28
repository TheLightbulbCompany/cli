//! Local capture of coding sessions and their tool calls, written by the daemon (the one
//! writer) from the harness hooks. A session is a row; a tool call is a row with a short
//! summary (a command, a path, a pattern), never file contents, diffs, output or prompts.
//! A summary that looks like a credential is stored as `[refused: <pattern>]`.

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

/// Whether `folder` has a component starting with `.` below the home folder.
pub fn hidden_folder(folder: &str) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let folder = std::path::Path::new(folder);
    // HOME as given and as the filesystem resolves it (a hook reports its folder resolved).
    let canonical = std::fs::canonicalize(&home).unwrap_or_default();
    let rest = [home.as_ref(), canonical.as_path()]
        .into_iter()
        .filter(|h: &&std::path::Path| !h.as_os_str().is_empty())
        .find_map(|h| folder.strip_prefix(h).ok())
        .unwrap_or(folder);
    rest.components().any(
        |c| matches!(c, std::path::Component::Normal(n) if n.to_string_lossy().starts_with('.')),
    )
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
        Some(p) => format!("[refused: {p}]"),
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

/// A turn's tokens, added to the session's, when the harness reported them.
pub fn add_tokens(
    store: &Store,
    id: &str,
    prompt: Option<i64>,
    completion: Option<i64>,
) -> Result<()> {
    if prompt.is_none() && completion.is_none() {
        return Ok(());
    }
    store.conn().execute(
        "UPDATE capture_sessions SET prompt_tokens = COALESCE(prompt_tokens, 0) + COALESCE(?2, 0), \
         completion_tokens = COALESCE(completion_tokens, 0) + COALESCE(?3, 0) WHERE id = ?1",
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

/// The one session whose id is `id` or starts with it; `None` when none or several do.
pub fn find(store: &Store, id: &str) -> Result<Option<SessionRow>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT {COLS} FROM capture_sessions WHERE id = ?1 OR id LIKE ?2 ESCAPE '\\' LIMIT 2"
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
    Ok((rows.len() == 1).then(|| rows.remove(0)))
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

/// Deletes a session and its tool calls; whether it existed.
pub fn forget(store: &Store, id: &str) -> Result<bool> {
    store
        .conn()
        .execute("DELETE FROM capture_tool_calls WHERE session_id = ?1", [id])?;
    Ok(store
        .conn()
        .execute("DELETE FROM capture_sessions WHERE id = ?1", [id])?
        == 1)
}
