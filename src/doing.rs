//! What a Claude session is doing, read from the tail of its own transcript.

use crate::sessions::Session;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const TAIL_BYTES: u64 = 256 * 1024;
/// The most characters of a `doing` or `title` (or a published folder) a teammate is sent.
pub const TEXT_MAX: usize = 256;

/// `(title, current todo)` from `<home>/.claude/projects/<escaped-cwd>/<conversation>.jsonl`.
/// Only the last 256 KB is read; `(None, None)` for other providers or no transcript.
pub fn title_and_todo(home: &Path, s: &Session) -> (Option<String>, Option<String>) {
    if s.provider != "claude" {
        return (None, None);
    }
    tail(&transcript(home, &s.cwd, &s.conversation)).map_or((None, None), |t| scan(&t))
}

/// A Claude conversation's title from its transcript, its process gone or not.
pub fn title(home: &Path, cwd: &str, conversation: &str) -> Option<String> {
    scan(&tail(&transcript(home, cwd, conversation)).ok()?).0
}

/// When a Claude conversation's transcript was last written (unix seconds), if it has one.
pub fn last_written(home: &Path, cwd: &str, conversation: &str) -> Option<i64> {
    let modified = std::fs::metadata(transcript(home, cwd, conversation))
        .ok()?
        .modified()
        .ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_secs() as i64)
}

fn transcript(home: &Path, cwd: &str, conversation: &str) -> PathBuf {
    // Claude names the project dir after the cwd with every non-alphanumeric as '-'.
    let dir: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.join(".claude/projects")
        .join(dir)
        .join(format!("{conversation}.jsonl"))
}

fn tail(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if len > TAIL_BYTES {
        // The cut landed mid-line: drop the partial first line.
        text = text
            .split_once('\n')
            .map_or_else(String::new, |(_, r)| r.into());
    }
    Ok(text)
}

fn scan(text: &str) -> (Option<String>, Option<String>) {
    let (mut title, mut todo) = (None, None);
    // Task-tool era: TaskCreate subjects by creation order, TaskUpdate sets the id in progress.
    // ponytail: ids are counted from the tail, so a task created before the tail is unnamed.
    let mut tasks: Vec<String> = Vec::new();
    for v in text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        match v["type"].as_str() {
            Some("ai-title") => title = v["aiTitle"].as_str().map(String::from),
            Some("custom-title") => title = v["customTitle"].as_str().map(String::from),
            Some("assistant") => {
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    let input = &b["input"];
                    match b["name"].as_str() {
                        Some("TodoWrite") => {
                            todo = input["todos"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .find(|t| t["status"] == "in_progress")
                                .and_then(|t| t["activeForm"].as_str().or(t["content"].as_str()))
                                .map(String::from);
                        }
                        Some("TaskCreate") => tasks.push(
                            input["activeForm"]
                                .as_str()
                                .or(input["subject"].as_str())
                                .unwrap_or_default()
                                .into(),
                        ),
                        Some("TaskUpdate") => {
                            let id = input["taskId"]
                                .as_str()
                                .and_then(|i| i.parse::<usize>().ok());
                            let name = id.and_then(|i| tasks.get(i.checked_sub(1)?));
                            match input["status"].as_str() {
                                Some("in_progress") => todo = name.cloned().or(todo),
                                Some("completed" | "deleted")
                                    if name.is_some() && todo == name.cloned() =>
                                {
                                    todo = None
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (title, todo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{Status, Tier};

    fn session(cwd: &str, conversation: &str) -> Session {
        Session {
            key: crate::sessions::Key::new("k3f9ab7cd2ef", "claude", conversation),
            provider: "claude".into(),
            conversation: conversation.into(),
            pid: 1,
            birth_micros: 1,
            cwd: cwd.into(),
            title: None,
            doing: None,
            status: Status::Live,
            tier: Tier::Pull,
            push_via: None,
            handle: None,
            repo: None,
            branch: None,
            hidden: false,
            headless: false,
            waiting_for: None,
            kind: None,
            entrypoint: None,
            job: None,
        }
    }

    #[test]
    fn doing_reads_title_and_current_todo() {
        let home = tempfile::tempdir().unwrap();
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-projects");
        let dir = home.path().join(".claude/projects");
        copy_dir(&fixtures, &dir);

        let s = session("/tmp/proj", "00000000-0000-4000-8000-0000000000aa");
        assert_eq!(
            title_and_todo(home.path(), &s),
            (
                Some("Session registry".into()),
                Some("Implementing discovery".into())
            )
        );
        assert_eq!(
            title_and_todo(home.path(), &session("/tmp/other", "x")),
            (None, None)
        );
    }

    #[test]
    fn task_tools_set_and_clear_the_current_todo() {
        let text = [
            r#"{"type":"assistant","message":{"content":[{"name":"TaskCreate","input":{"subject":"A","activeForm":"Doing A"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"name":"TaskUpdate","input":{"taskId":"1","status":"in_progress"}}]}}"#,
        ]
        .join("\n");
        assert_eq!(scan(&text), (None, Some("Doing A".into())));
        let done = format!(
            "{text}\n{}",
            r#"{"type":"assistant","message":{"content":[{"name":"TaskUpdate","input":{"taskId":"1","status":"completed"}}]}}"#
        );
        assert_eq!(scan(&done), (None, None));
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let dest = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &dest);
            } else {
                std::fs::copy(e.path(), dest).unwrap();
            }
        }
    }
}
