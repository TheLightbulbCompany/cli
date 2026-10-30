//! The destructive-migration gate: `up` and `publish` read every `drizzle/*.sql` the app has not
//! shipped yet and stop on anything that can lose data, unless the owner approved it (spec §5).
//!
//! `.lightbulb/migrations.json` is `{"applied": ["0000_init.sql", ...]}`: the files a successful
//! publish has shipped (`record`). Files in it are never looked at again. `seed.sql` is sample
//! data, not schema, so it is not a migration.

// Derived from Dyad (https://github.com/dyad-sh/dyad), commit 0ee4a524, src/ipc/utils/migration_utils.ts. Copyright (c) Dyad contributors. Apache-2.0. Modified by The Lightbulb Company.
// (The drop table / truncate / drop column / alter column type patterns, as Rust regexes.)
//
// The statement splitter and the rules for delete, rename, set not null and column drops are a
// port of packages/pg-schema-classifier (dyad-sh/dyad, commit 0ee4a524), which declares the MIT
// license in its package.json. The package ships no copyright line, so the notice names Dyad's:
//
//   MIT License. Copyright (c) Dyad contributors.
//   Permission is hereby granted, free of charge, to any person obtaining a copy of this software
//   and associated documentation files (the "Software"), to deal in the Software without
//   restriction, including without limitation the rights to use, copy, modify, merge, publish,
//   distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
//   Software is furnished to do so, subject to the following conditions: The above copyright
//   notice and this permission notice shall be included in all copies or substantial portions of
//   the Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//   IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
//   PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
//   LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
//   OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
//   DEALINGS IN THE SOFTWARE.

use super::api::ApiError;
use regex::Regex;
use serde_json::{json, Value};
use std::path::Path;

/// What `fail` prints when the gate stops `up` or `publish`.
pub const NEXT: &str =
    "rewrite add-only (add a column, copy data), or ask the owner and pass --approved <card id>";

/// One statement that can lose data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub statement: String,
    /// `drop_table`, `drop_column`, `rename_table`, `rename_column`, `alter_type`,
    /// `set_not_null`, `truncate`, `delete` or `dynamic_sql`.
    pub kind: &'static str,
}

#[derive(Debug)]
pub struct GateResult {
    /// Migration files not yet recorded, in name order.
    pub unrecorded: Vec<String>,
    /// `(file, finding)` for each destructive statement in them.
    pub findings: Vec<(String, Finding)>,
}

/// The statements of `sql` as `(text, masked)`: comments removed, and in `masked` the bodies of
/// string and dollar quotes blanked so the rules cannot match inside data. Split on `;` outside
/// quotes and comments, and on drizzle's `--> statement-breakpoint`.
fn split(sql: &str) -> Vec<(String, String)> {
    let sql = sql.replace("--> statement-breakpoint", ";");
    let c: Vec<char> = sql.chars().collect();
    let (mut out, mut text, mut mask) = (vec![], String::new(), String::new());
    let mut i = 0;
    let mut push = |text: &mut String, mask: &mut String| {
        if !text.trim().is_empty() {
            out.push((text.trim().to_string(), mask.trim().to_string()));
        }
        text.clear();
        mask.clear();
    };
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        if ch == '-' && next == Some('-') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
            text.push(' ');
            mask.push(' ');
        } else if ch == '/' && next == Some('*') {
            let mut depth = 0;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            text.push(' ');
            mask.push(' ');
        } else if ch == '\'' || ch == '"' {
            // Quoted text; a doubled quote is an escaped one. Identifiers stay visible in the
            // mask (a column called "delete" is still not a DELETE: rules look at keywords only).
            let start = i;
            // E'...' strings treat a backslash as an escape (E is not the end of a name).
            let escapes = ch == '\''
                && i > 0
                && matches!(c[i - 1], 'E' | 'e')
                && (i < 2 || !(c[i - 2].is_alphanumeric() || matches!(c[i - 2], '_' | '$')));
            i += 1;
            while i < c.len() {
                if escapes && c[i] == '\\' {
                    i += 2;
                    continue;
                }
                if c[i] == ch {
                    if c.get(i + 1) == Some(&ch) {
                        i += 1;
                    } else {
                        break;
                    }
                }
                i += 1;
            }
            let end = (i + 1).min(c.len());
            text.extend(&c[start..end]);
            if ch == '"' {
                mask.push_str("\"x\"");
            } else {
                mask.push_str("''");
            }
            i = end;
            continue;
        } else if ch == '$' && dollar_tag(&c, i).is_some() {
            let tag = dollar_tag(&c, i).unwrap();
            let start = i;
            i += tag.len();
            while i < c.len() && !c[i..].starts_with(&tag) {
                i += 1;
            }
            let end = (i + tag.len()).min(c.len());
            text.extend(&c[start..end]);
            mask.push_str("''");
            i = end;
            continue;
        } else if ch == ';' {
            push(&mut text, &mut mask);
        } else {
            text.push(ch);
            mask.push(ch);
        }
        i += 1;
    }
    push(&mut text, &mut mask);
    out
}

/// `$$` or `$tag$` at `i`.
fn dollar_tag(c: &[char], i: usize) -> Option<Vec<char>> {
    let mut j = i + 1;
    if c.get(j) != Some(&'$') {
        if !c.get(j).is_some_and(|x| x.is_alphabetic() || *x == '_') {
            return None;
        }
        while c.get(j).is_some_and(|x| x.is_alphanumeric() || *x == '_') {
            j += 1;
        }
        if c.get(j) != Some(&'$') {
            return None;
        }
    }
    Some(c[i..=j].to_vec())
}

/// Every statement in `sql` that can lose data. Fails closed: code the gate cannot read
/// (`DO`, `CALL`, `EXECUTE`, `PREPARE`) is `dynamic_sql`.
pub fn classify(sql: &str) -> Vec<Finding> {
    let re = |p: &str| Regex::new(&format!("(?is){p}")).unwrap();
    let r = Rules {
        drop_table: re(r"^DROP\s+(?:TABLE|SCHEMA|DATABASE|OWNED)\b|^DROP\b.*\bCASCADE\b"),
        dynamic: re(r"^(?:DO|CALL|EXECUTE|PREPARE)\b"),
        truncate: re(r"^TRUNCATE\b"),
        delete: re(r"^DELETE\b|^(?:WITH|MERGE)\b.*\bDELETE\b"),
        explain: re(
            r"^EXPLAIN\s+(?:\((?:[^)]*\bANALY[SZ]E\b[^)]*)\)|ANALY[SZ]E\b(?:\s+VERBOSE\b)?)\s*",
        ),
        alter_table: re(r"^ALTER\s+TABLE\b"),
        drop_target: re(r"\bDROP\s+(?:IF\s+EXISTS\s+)?(\w+|\x22)"),
        rename_table: re(r"\bRENAME\s+TO\b"),
        rename_column: re(r"\bRENAME\s+(?:COLUMN\s+)?(?:\w+|\x22x\x22)\s+TO\b"),
        alter_type: re(r"\bALTER\s+(?:COLUMN\s+)?(?:\w+|\x22x\x22)\s+(?:SET\s+DATA\s+)?TYPE\b"),
        set_not_null: re(r"\bSET\s+NOT\s+NULL\b"),
    };
    let mut found = vec![];
    for (text, mask) in split(sql) {
        found.extend(r.kinds(&mask).into_iter().map(|kind| Finding {
            statement: text.clone(),
            kind,
        }));
    }
    found
}

struct Rules {
    drop_table: Regex,
    dynamic: Regex,
    truncate: Regex,
    delete: Regex,
    explain: Regex,
    alter_table: Regex,
    drop_target: Regex,
    rename_table: Regex,
    rename_column: Regex,
    alter_type: Regex,
    set_not_null: Regex,
}

impl Rules {
    fn kinds(&self, mask: &str) -> Vec<&'static str> {
        // After DROP, these are not a column (ALTER TABLE DROP CONSTRAINT / DEFAULT / NOT NULL ...).
        const NOT_COLUMNS: [&str; 8] = [
            "CONSTRAINT",
            "DEFAULT",
            "EXPRESSION",
            "IDENTITY",
            "INHERIT",
            "NOT",
            "OF",
            "REPLICA",
        ];
        // EXPLAIN ANALYZE runs the statement: look at the statement.
        if let Some(m) = self.explain.find(mask) {
            return self.kinds(&mask[m.end()..]);
        }
        if self.drop_table.is_match(mask) {
            return vec!["drop_table"];
        }
        if self.dynamic.is_match(mask) {
            return vec!["dynamic_sql"];
        }
        if self.truncate.is_match(mask) {
            return vec!["truncate"];
        }
        if self.delete.is_match(mask) {
            return vec!["delete"];
        }
        let mut kinds = vec![];
        if self.alter_table.is_match(mask) {
            if self.drop_target.captures_iter(mask).any(|c| {
                let w = c[1].to_uppercase();
                w == "COLUMN" || !NOT_COLUMNS.contains(&w.as_str())
            }) {
                kinds.push("drop_column");
            }
            if self.rename_table.is_match(mask) {
                kinds.push("rename_table");
            } else if self.rename_column.is_match(mask) {
                kinds.push("rename_column");
            }
            if self.alter_type.is_match(mask) {
                kinds.push("alter_type");
            }
            if self.set_not_null.is_match(mask) {
                kinds.push("set_not_null");
            }
        }
        kinds
    }
}

fn bad(why: String) -> ApiError {
    ApiError::new(
        "bad_migrations",
        &format!("Could not use .lightbulb/migrations.json or the drizzle folder: {why}"),
        "fix or delete .lightbulb/migrations.json, then try again",
    )
}

/// The applied list; absent is empty, unreadable is an error (never "nothing applied").
fn applied(root: &Path) -> Result<(Value, Vec<String>), ApiError> {
    let v: Value = match std::fs::read(root.join(".lightbulb/migrations.json")) {
        Ok(b) => serde_json::from_slice(&b)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| bad("it is not a JSON object".into()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(bad(e.to_string())),
    };
    let list = match &v["applied"] {
        Value::Null => vec![],
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(String::from))
            .collect::<Option<_>>()
            .ok_or_else(|| bad("applied is a list of file names".into()))?,
        _ => return Err(bad("applied is a list of file names".into())),
    };
    Ok((v, list))
}

/// Looks at every `drizzle/*.sql` (not `seed.sql`) that is not in `migrations.json`.
pub fn gate(root: &Path) -> Result<GateResult, ApiError> {
    let (_, done) = applied(root)?;
    let mut files = vec![];
    let dir = root.join("drizzle");
    if dir.is_dir() {
        for e in std::fs::read_dir(&dir).map_err(|e| bad(e.to_string()))? {
            let name = e.map_err(|e| bad(e.to_string()))?.file_name();
            let name = name.to_string_lossy().into_owned();
            if name.ends_with(".sql") && name != "seed.sql" && !done.contains(&name) {
                files.push(name);
            }
        }
    }
    files.sort();
    let mut findings = vec![];
    for f in &files {
        let sql = std::fs::read_to_string(dir.join(f)).map_err(|e| bad(format!("{f}: {e}")))?;
        findings.extend(classify(&sql).into_iter().map(|x| (f.clone(), x)));
    }
    Ok(GateResult {
        unrecorded: files,
        findings,
    })
}

/// Marks `files` as shipped. Called by `publish` once the backend accepted them.
#[allow(dead_code)] // Task 6 (publish) is the caller.
pub(crate) fn record(root: &Path, files: &[String]) -> Result<(), ApiError> {
    let (mut v, mut list) = applied(root)?;
    for f in files {
        if !list.contains(f) {
            list.push(f.clone());
        }
    }
    v["applied"] = json!(list);
    let path = root.join(".lightbulb/migrations.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::create_dir_all(root.join(".lightbulb"))
        .and_then(|_| std::fs::write(&tmp, v.to_string()))
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| bad(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_merges_and_keeps_other_keys() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(".lightbulb")).unwrap();
        std::fs::write(
            d.path().join(".lightbulb/migrations.json"),
            r#"{"applied":["a.sql"],"x":1}"#,
        )
        .unwrap();
        record(d.path(), &["b.sql".into(), "a.sql".into()]).unwrap();
        let (v, list) = applied(d.path()).unwrap();
        assert_eq!(list, ["a.sql", "b.sql"]);
        assert_eq!(v["x"], 1);
    }
}
