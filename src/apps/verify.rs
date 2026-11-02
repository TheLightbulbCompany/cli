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
    if problems.iter().all(|p| p["page"] != "the door") {
        for page in &fixed {
            match curl(&jar.0, &format!("{origin}{page}"), None) {
                Some((s, b)) => {
                    if let Some(p) = page_problem(s, &b) {
                        problems.push(json!({"page": page, "problem": p}));
                    }
                }
                None => problems.push(json!({"page": page, "problem": "did not answer"})),
            }
        }
    }
    let pass = problems.is_empty() && !fixed.is_empty();
    if pass {
        let saved = evidence::record_verify(root, &head, &fixed);
        if let Err(e) = saved {
            return Ok(fail(json, &e.code, &e.message, &e.next));
        }
    }
    let next = if pass {
        "run lightbulb app status; this does not test buttons or forms, so try the main flow too"
    } else {
        "fix what is listed, run lightbulb app up, then verify again"
    };
    let out = json!({"pass": pass, "opened": fixed, "not_opened": dynamic, "problems": problems, "next": next});
    if json {
        println!("{out}");
    } else {
        println!(
            "{}: opened {} page(s)",
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
    fn a_link_splits_into_origin_and_pass() {
        let (o, t) = split_link("https://a.vercel.app/_lightbulb/session#token=abc").unwrap();
        assert_eq!((o.as_str(), t.as_str()), ("https://a.vercel.app", "abc"));
        assert!(split_link("https://a.vercel.app/").is_none());
        assert!(split_link("file:///etc/passwd#token=abc").is_none());
    }
}
