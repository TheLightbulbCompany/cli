//! `lightbulb app verify`: the check that needs no browser and no model. It signs in through the
//! door with a test link, opens every page the app has, and records a pass for HEAD.

use super::api::{self, Auth};
use super::ship::{self, git_ok};
use super::{evidence, fail};
use crate::Result;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What Next.js renders when a page throws after the response has started: the status stays 200.
const CRASHED: &[&str] = &[
    "Application error: a server-side exception",
    "__next_error__",
];

/// Every page under `src/app` that has a fixed address. `[id]` pages need a real id, so they are
/// returned apart: the caller says they were not opened instead of implying they passed.
pub(crate) fn routes(root: &Path) -> (Vec<String>, Vec<String>) {
    let (mut fixed, mut dynamic) = (vec![], vec![]);
    walk(&root.join("src/app"), "", &mut fixed, &mut dynamic);
    fixed.sort();
    dynamic.sort();
    (fixed, dynamic)
}

fn walk(dir: &Path, at: &str, fixed: &mut Vec<String>, dynamic: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if e.path().is_dir() {
            // `_x` and `%5Fx` are private or the door's own; `@x` are parallel slots.
            if name.starts_with('_') || name.starts_with("%5F") || name.starts_with('@') {
                continue;
            }
            let next = if name.starts_with('(') {
                at.to_string()
            } else {
                format!("{at}/{name}")
            };
            walk(&e.path(), &next, fixed, dynamic);
        } else if name == "page.tsx" {
            let route = if at.is_empty() {
                "/".to_string()
            } else {
                at.to_string()
            };
            if route.contains('[') {
                dynamic.push(route);
            } else {
                fixed.push(route);
            }
        }
    }
}

/// The problem with a page's answer, if any.
pub(crate) fn page_problem(status: u16, body: &str) -> Option<String> {
    if !(200..400).contains(&status) {
        return Some(format!("answered {status}"));
    }
    CRASHED
        .iter()
        .any(|m| body.contains(m))
        .then(|| "crashed while rendering".to_string())
}

/// The fields a form would send, filled with harmless test values. Forms that ask for a password
/// are left alone: those sign people in, and a guess would only fail or lock someone out.
pub(crate) fn forms(html: &str) -> Vec<Vec<(String, String)>> {
    let attr = |tag: &str, name: &str| {
        regex::Regex::new(&format!(r#"(?i)\b{name}\s*=\s*"([^"]*)""#))
            .ok()
            .and_then(|r| r.captures(tag).map(|c| c[1].to_string()))
    };
    let form = regex::Regex::new(r"(?is)<form\b[^>]*>(.*?)</form>").unwrap();
    let input = regex::Regex::new(r"(?is)<input\b[^>]*>").unwrap();
    let area = regex::Regex::new(r"(?is)<textarea\b[^>]*>").unwrap();
    let select = regex::Regex::new(r"(?is)(<select\b[^>]*>)(.*?)</select>").unwrap();
    let option = regex::Regex::new(r"(?is)<option\b[^>]*>").unwrap();
    let mut out = vec![];
    for f in form.captures_iter(html) {
        let body = &f[1];
        let mut fields = vec![];
        let mut password = false;
        for i in input.find_iter(body) {
            let tag = i.as_str();
            let Some(name) = attr(tag, "name") else {
                continue;
            };
            let kind = attr(tag, "type")
                .unwrap_or_else(|| "text".into())
                .to_lowercase();
            let value = match kind.as_str() {
                "password" => {
                    password = true;
                    continue;
                }
                "submit" | "button" | "image" | "reset" | "file" | "checkbox" | "radio" => continue,
                "hidden" => attr(tag, "value").unwrap_or_default(),
                "email" => "verify@example.com".into(),
                "number" | "range" => attr(tag, "min").unwrap_or_else(|| "1".into()),
                "date" => "2030-01-15".into(),
                "time" => "10:00".into(),
                "url" => "https://example.com".into(),
                "tel" => "5550100".into(),
                _ => attr(tag, "value")
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "Verify test".into()),
            };
            fields.push((name, value));
        }
        for a in area.find_iter(body) {
            if let Some(name) = attr(a.as_str(), "name") {
                fields.push((name, "Verify test".into()));
            }
        }
        for s in select.captures_iter(body) {
            let Some(name) = attr(&s[1], "name") else {
                continue;
            };
            let first = option
                .find_iter(&s[2])
                .filter_map(|o| attr(o.as_str(), "value"))
                .find(|v| !v.is_empty());
            fields.push((name, first.unwrap_or_default()));
        }
        // A server action carries its id in a hidden field; a form without one has nowhere to send.
        if !password && fields.iter().any(|(n, _)| n.starts_with("$ACTION")) {
            out.push(fields);
        }
    }
    out
}

/// Submits one form the way a browser without scripts would.
fn submit(jar: &Path, url: &str, fields: &[(String, String)]) -> Option<(u16, String)> {
    let mut c = Command::new("curl");
    c.args([
        "-sS",
        "-L",
        "--max-time",
        "30",
        "-o",
        "-",
        "-w",
        "\n%{http_code}",
    ])
    .arg("-b")
    .arg(jar)
    .arg("-c")
    .arg(jar)
    .stdin(Stdio::null())
    .stderr(Stdio::null());
    for (k, v) in fields {
        c.arg("--form-string").arg(format!("{k}={v}"));
    }
    let out = c.arg(url).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n')?;
    Some((code.trim().parse().ok()?, body.to_string()))
}

/// Splits a test link into its address and pass.
pub(crate) fn split_link(link: &str) -> Option<(String, String)> {
    let (base, token) = link.split_once("#token=")?;
    let origin = base.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
    (origin.starts_with("https://") || origin.starts_with("http://"))
        .then(|| (origin, token.to_string()))
}

/// One request through curl. The pass goes on stdin and the session in a private jar, so neither
/// reaches a process list or a log.
fn curl(jar: &Path, url: &str, post: Option<&str>) -> Option<(u16, String)> {
    let mut c = Command::new("curl");
    c.args([
        "-sS",
        "-L",
        "--max-time",
        "30",
        "-o",
        "-",
        "-w",
        "\n%{http_code}",
    ])
    .arg("-b")
    .arg(jar)
    .arg("-c")
    .arg(jar)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    if post.is_some() {
        c.args([
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-H",
            "sec-fetch-site: same-origin",
            "--data-binary",
            "@-",
        ]);
    }
    let mut child = c.arg(url).spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    if let Some(body) = post {
        stdin.write_all(body.as_bytes()).ok()?;
    }
    drop(stdin);
    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n')?;
    Some((code.trim().parse().ok()?, body.to_string()))
}

struct Jar(PathBuf);

impl Jar {
    fn new() -> std::io::Result<Jar> {
        let path = std::env::temp_dir().join(format!("lightbulb-verify-{}", std::process::id()));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        o.open(&path)?;
        Ok(Jar(path))
    }
}

impl Drop for Jar {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `lightbulb app verify`.
pub(crate) fn run(root: &Path, auth: &Auth, json: bool) -> Result<i32> {
    let id = match ship::app_id(root, json) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    let link = match api::call(auth, "POST", &format!("/{id}/test-link"), Some(&json!({}))) {
        Ok(r) => r["url"].as_str().map(str::to_string),
        Err(e) => return Ok(fail(json, &e.code, &e.message, &e.next)),
    };
    let Some((origin, token)) = link.as_deref().and_then(split_link) else {
        return Ok(fail(
            json,
            "no_url",
            "Lightbulb did not return a link.",
            "try again in a minute",
        ));
    };
    let head = match git_ok(root, &["rev-parse", "HEAD"]) {
        Ok(h) => h,
        Err(e) => return Ok(fail(json, &e.code, &e.message, &e.next)),
    };
    let jar = Jar::new()?;
    let mut problems: Vec<Value> = vec![];
    let (fixed, dynamic) = routes(root);
    // Before signing in: a closed app must not open for a stranger.
    let open = api::call(auth, "GET", &format!("/{id}/status"), None)
        .map(|s| s["open_to"] == "public")
        .unwrap_or(false);
    if !open && matches!(curl(&jar.0, &format!("{origin}/"), None), Some((s, _)) if s < 400) {
        problems.push(json!({"page": "/", "problem": "opened without signing in"}));
    }
    let body = json!({ "token": token }).to_string();
    match curl(
        &jar.0,
        &format!("{origin}/api/_lightbulb/session"),
        Some(&body),
    ) {
        Some((s, _)) if s < 300 => {}
        Some((s, _)) => problems.push(
            json!({"page": "the door", "problem": format!("refused the test link with {s}")}),
        ),
        None => problems.push(json!({"page": "the door", "problem": "did not answer"})),
    }
    let (mut submitted, mut tried) = (0, 0);
    if problems.iter().all(|p| p["page"] != "the door") {
        for page in &fixed {
            match curl(&jar.0, &format!("{origin}{page}"), None) {
                Some((s, b)) => {
                    if let Some(p) = page_problem(s, &b) {
                        problems.push(json!({"page": page, "problem": p}));
                        continue;
                    }
                    for (n, fields) in forms(&b).iter().enumerate() {
                        tried += 1;
                        let answer = submit(&jar.0, &format!("{origin}{page}"), fields);
                        let problem = match answer {
                            Some((s, b)) => page_problem(s, &b),
                            None => Some("did not answer".into()),
                        };
                        if let Some(p) = problem {
                            problems.push(json!({"page": page, "problem": format!("form {} {p} when sent", n + 1)}));
                        } else {
                            submitted += 1;
                        }
                    }
                }
                None => problems.push(json!({"page": page, "problem": "did not answer"})),
            }
        }
    }
    // The forms wrote test rows into the preview's database, where the person would see them. Put
    // it back to its schema and sample data; `false` means another read is busy and the next does it.
    let preview_data = if tried == 0 {
        "untouched"
    } else {
        match api::call(
            auth,
            "POST",
            &format!("/{id}/reset-preview-data"),
            Some(&json!({})),
        ) {
            Ok(r) if r["reset"] == true => "reset",
            Ok(_) => "resets on the next status read",
            Err(e) => {
                problems.push(json!({"page": "the preview data", "problem": format!("test entries were not taken back out: {}", e.message)}));
                "kept"
            }
        }
    };
    let pass = problems.is_empty() && !fixed.is_empty();
    if pass {
        let saved = evidence::record_verify(root, &head, &fixed);
        if let Err(e) = saved {
            return Ok(fail(json, &e.code, &e.message, &e.next));
        }
    }
    let next = if pass {
        "run lightbulb app status; this sent each form once with test values, then put the preview back to its sample data, but does not press other buttons, so try the main flow too"
    } else {
        "fix what is listed, run lightbulb app up, then verify again"
    };
    let out = json!({"pass": pass, "opened": fixed, "forms_sent": submitted, "not_opened": dynamic, "preview_data": preview_data, "problems": problems, "next": next});
    if json {
        println!("{out}");
    } else {
        println!(
            "{}: opened {} page(s), sent {submitted} form(s); preview data {preview_data}",
            if pass { "Pass" } else { "Fail" },
            fixed.len()
        );
        for p in &problems {
            println!(
                "  {}: {}",
                p["page"].as_str().unwrap_or(""),
                p["problem"].as_str().unwrap_or("")
            );
        }
        for d in &dynamic {
            println!("  not opened (needs a real id): {d}");
        }
        println!("Next: {next}");
    }
    Ok(i32::from(!pass))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_skip_the_door_and_set_aside_pages_that_need_an_id() {
        let t = tempfile::tempdir().unwrap();
        for p in [
            "",
            "(shop)/cart",
            "%5Flightbulb/closed",
            "_private",
            "accounts/[id]",
            "about",
        ] {
            let d = t.path().join("src/app").join(p);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("page.tsx"), "").unwrap();
        }
        let (fixed, dynamic) = routes(t.path());
        assert_eq!(fixed, ["/", "/about", "/cart"]);
        assert_eq!(dynamic, ["/accounts/[id]"]);
    }

    #[test]
    fn a_page_fails_on_an_error_status_or_a_crash_behind_a_200() {
        assert_eq!(page_problem(200, "<h1>Accounts</h1>"), None);
        assert_eq!(page_problem(500, "").as_deref(), Some("answered 500"));
        assert_eq!(page_problem(401, "").as_deref(), Some("answered 401"));
        assert!(page_problem(
            200,
            "Application error: a server-side exception has occurred"
        )
        .is_some());
    }

    #[test]
    fn forms_are_filled_with_test_values_and_sign_in_forms_are_left_alone() {
        let html = r#"<form action="" method="POST"><input type="hidden" name="$ACTION_ID_abc" value="">
            <input name="name" required><input type="email" name="email">
            <input type="number" name="seats" min="2"><textarea name="note"></textarea>
            <select name="stage"><option value="">Pick</option><option value="trial">Trial</option></select>
            <button type="submit">Send</button></form>
            <form><input type="hidden" name="$ACTION_ID_x" value=""><input type="password" name="pw"></form>
            <form><input name="q"></form>"#;
        let got = forms(html);
        assert_eq!(got.len(), 1);
        let f: std::collections::HashMap<_, _> = got[0].iter().cloned().collect();
        assert_eq!(f["email"], "verify@example.com");
        assert_eq!(f["seats"], "2");
        assert_eq!(f["note"], "Verify test");
        assert_eq!(f["stage"], "trial");
        assert_eq!(f["name"], "Verify test");
        assert!(f.contains_key("$ACTION_ID_abc"));
    }

    #[test]
    fn a_link_splits_into_origin_and_pass() {
        let (o, t) = split_link("https://a.vercel.app/_lightbulb/session#token=abc").unwrap();
        assert_eq!((o.as_str(), t.as_str()), ("https://a.vercel.app", "abc"));
        assert!(split_link("https://a.vercel.app/").is_none());
        assert!(split_link("file:///etc/passwd#token=abc").is_none());
    }
}
