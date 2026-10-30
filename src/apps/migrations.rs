//! The destructive-migration gate: `up` and `publish` read every `drizzle/*.sql` the app has not
//! shipped yet and stop on anything that can lose data, unless the owner approved it (spec §5).
//!
//! `.lightbulb/migrations.json` is `{"applied": ["0000_init.sql", ...]}`: the files a successful
//! publish has shipped (`record`). Files in it are never looked at again. `seed.sql` is sample
//! data, not schema, so it is not a migration.
//!
//! A site with the content switch also has Payload's migrations, `src/migrations/*.ts`, which run
//! when the app starts in production. Their `up` SQL (the `` sql`...` `` bodies) is gated the same
//! way and recorded as `src/migrations/<file>.ts`.

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

/// The names of `dir`'s files that `keep` accepts, sorted; none when `dir` is missing.
fn list(dir: &Path, keep: impl Fn(&str) -> bool) -> Result<Vec<String>, ApiError> {
    let mut names = vec![];
    if dir.is_dir() {
        for e in std::fs::read_dir(dir).map_err(|e| bad(e.to_string()))? {
            let name = e.map_err(|e| bad(e.to_string()))?.file_name();
            let name = name.to_string_lossy().into_owned();
            if keep(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    Ok(names)
}

/// The SQL a Payload migration runs going up: every `` sql`...` `` body in `up`, not in `down`.
/// `None` (fail closed) when `up` is anything else: missing, an arrow function, or code besides
/// `await db.execute(sql`...`)` (query builders, `sql.raw`, `payload.*`, `${...}` in the SQL).
fn payload_up_sql(ts: &str) -> Option<String> {
    let up = &ts[ts.find("export async function up")?..];
    let up = up
        .find("export async function down")
        .map_or(up, |i| &up[..i]);
    // The body starts at the first `{` after the parameters.
    let up = &up[up.find(')')?..];
    let up = &up[up.find('{')? + 1..];
    let (mut sql, mut rest, mut at) = (String::new(), String::new(), 0);
    while let Some(i) = up[at..].find("sql`") {
        let start = at + i + 4;
        let mut end = None;
        let mut escaped = false;
        for (j, c) in up[start..].char_indices() {
            match c {
                '\\' if !escaped => escaped = true,
                '`' if !escaped => {
                    end = Some(start + j);
                    break;
                }
                _ => escaped = false,
            }
        }
        let end = end?;
        let body = &up[start..end];
        if body.contains("${") {
            return None;
        }
        rest.push_str(&up[at..at + i]);
        sql.push_str(body);
        sql.push_str(";\n");
        at = end + 1;
    }
    rest.push_str(&up[at..]);
    // What is left must be only the `await db.execute(...)` wrappers.
    let rest = rest.replace("await db.execute(", "");
    rest.chars()
        .all(|c| c.is_whitespace() || "(){};".contains(c))
        .then_some(sql)
}

/// Looks at every `drizzle/*.sql` (not `seed.sql`) and Payload `src/migrations/*.ts` (not
/// `index.ts`) that is not in `migrations.json`.
pub fn gate(root: &Path) -> Result<GateResult, ApiError> {
    let (_, done) = applied(root)?;
    let read = |p: &Path, f: &str| std::fs::read_to_string(p).map_err(|e| bad(format!("{f}: {e}")));
    let mut files = vec![];
    let mut findings = vec![];
    let drizzle = root.join("drizzle");
    for f in list(&drizzle, |n| n.ends_with(".sql") && n != "seed.sql")? {
        if !done.contains(&f) {
            let sql = read(&drizzle.join(&f), &f)?;
            findings.extend(classify(&sql).into_iter().map(|x| (f.clone(), x)));
            files.push(f);
        }
    }
    let payload = root.join("src/migrations");
    for name in list(&payload, |n| n.ends_with(".ts") && n != "index.ts")? {
        let f = format!("src/migrations/{name}");
        if !done.contains(&f) {
            let ts = read(&payload.join(&name), &f)?;
            match payload_up_sql(&ts) {
                Some(sql) => findings.extend(classify(&sql).into_iter().map(|x| (f.clone(), x))),
                None => findings.push((
                    f.clone(),
                    Finding {
                        statement: ts,
                        kind: "dynamic_sql",
                    },
                )),
            }
            files.push(f);
        }
    }
    Ok(GateResult {
        unrecorded: files,
        findings,
    })
}

/// Marks `files` as shipped. Called by `publish` once the backend accepted them.
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
    fn payload_migrations_are_gated_by_their_up_sql_only() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("src/migrations");
        std::fs::create_dir_all(&dir).unwrap();
        let migration = |up: &str, down: &str| {
            format!(
                "import {{ sql }} from '@payloadcms/db-postgres'\n\
                 export async function up({{ db }}) {{\n  await db.execute(sql`\n   {up}`)\n}}\n\
                 export async function down({{ db }}) {{\n  await db.execute(sql`\n   {down}`)\n}}\n"
            )
        };
        let drop = "ALTER TABLE \"pages\" DROP COLUMN \"title\";";
        let add = "ALTER TABLE \"pages\" ADD COLUMN \"title\" varchar;";
        std::fs::write(dir.join("1_drop.ts"), migration(drop, add)).unwrap();
        std::fs::write(dir.join("2_add.ts"), migration(add, drop)).unwrap();
        std::fs::write(dir.join("index.ts"), "export const migrations = []").unwrap();
        let g = gate(d.path()).unwrap();
        assert_eq!(
            g.unrecorded,
            ["src/migrations/1_drop.ts", "src/migrations/2_add.ts"]
        );
        let files: Vec<&str> = g.findings.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(files, ["src/migrations/1_drop.ts"]);
        assert_eq!(g.findings[0].1.kind, "drop_column");
        record(d.path(), &g.unrecorded).unwrap();
        assert!(gate(d.path()).unwrap().findings.is_empty());
    }

    #[test]
    fn payload_migrations_that_are_not_plain_sql_fail_closed() {
        let kinds = |ts: &str| {
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(d.path().join("src/migrations")).unwrap();
            std::fs::write(d.path().join("src/migrations/1_x.ts"), ts).unwrap();
            let g = gate(d.path()).unwrap();
            g.findings.iter().map(|(_, f)| f.kind).collect::<Vec<_>>()
        };
        let arrow =
            "export const up = async ({ db }) => {\n  await db.execute(sql`DROP TABLE t`)\n}\n";
        assert_eq!(kinds(arrow), ["dynamic_sql"]);
        let builder = "export async function up({ payload }) {\n  await payload.delete({ collection: 'pages', where: {} })\n}\n";
        assert_eq!(kinds(builder), ["dynamic_sql"]);
        let raw = "export async function up({ db }) {\n  await db.execute(sql.raw(q))\n}\n";
        assert_eq!(kinds(raw), ["dynamic_sql"]);
        let interpolated =
            "export async function up({ db }) {\n  await db.execute(sql`DELETE FROM ${t}`)\n}\n";
        assert_eq!(kinds(interpolated), ["dynamic_sql"]);
        let plain = "export async function up({ db }: MigrateUpArgs): Promise<void> {\n  await db.execute(sql`\n  CREATE TABLE t (id int);`)\n}\n";
        assert!(kinds(plain).is_empty());
    }

    #[test]
    fn the_shipped_payload_migration_passes_the_gate() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("src/migrations");
        std::fs::create_dir_all(&dir).unwrap();
        for (p, b) in crate::apps::starter::FILES {
            if let Some(name) = p.strip_prefix("addons/content/src/migrations/") {
                std::fs::write(dir.join(name), b).unwrap();
            }
        }
        let g = gate(d.path()).unwrap();
        assert_eq!(g.unrecorded.len(), 1, "{:?}", g.unrecorded);
        assert!(g.findings.is_empty(), "{:?}", g.findings);
    }

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
