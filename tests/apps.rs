//! `lightbulb app`: the subcommand, `app.yaml`, which credentials a call carries, and the
//! failure shape. The backend is a stub on loopback; nothing here reaches the real one.

mod common;

use common::Env;
use lightbulb::apps::api::{self, ApiError, Auth};
use lightbulb::apps::migrations::{classify, gate};
use lightbulb::apps::{AppConfig, Kind, OpenTo, Origin};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Calls that set process env share it: one at a time.
static ENV: Mutex<()> = Mutex::new(());

/// One request the stub got: the request line, lowercase headers, the raw body.
struct Req {
    line: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// A stub backend answering every request `(status, json)` after `delay_ms`.
fn stub(status: u16, answer: Value, delay_ms: u64) -> (String, Arc<Mutex<Vec<Req>>>) {
    stub_with(move |_| (status, answer.clone()), delay_ms)
}

/// A stub backend that answers each request by its request line, e.g. `POST /api/v1/...`.
fn stub_with(
    answer: impl Fn(&str) -> (u16, Value) + Send + 'static,
    delay_ms: u64,
) -> (String, Arc<Mutex<Vec<Req>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Req>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            let mut s = s.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let mut headers = HashMap::new();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.to_lowercase(), v.trim().to_string());
                }
            }
            let len = headers
                .get("content-length")
                .map_or(0, |v| v.parse().unwrap());
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(Req {
                line: line.trim().to_string(),
                headers,
                body,
            });
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            let line = log.lock().unwrap().last().unwrap().line.clone();
            let (status, answer) = answer(&line);
            let text = answer.to_string();
            let _ = write!(
                s,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (base, seen)
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[test]
fn help_lists_app_under_apps_you_build() {
    let e = Env::new();
    let o = e
        .base_cmd(Command::new(common::EXE))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&o.stdout);
    assert!(
        help.contains("Apps you build:\n  app       Build, check, preview, publish and undo apps: app new|up|publish|rollback|status|destroy|check|evidence|report-broken-tool\n"),
        "{help}"
    );
}

fn sample() -> AppConfig {
    AppConfig {
        name: "team-lunch".into(),
        services: vec!["Stripe, payments".into(), "db".into()],
        open_to: OpenTo::Workspace,
        budget_usd: 20,
        kind: Kind::ContentSite,
        origin: Origin {
            kind: "issue".into(),
            r#ref: Some("LB-12: \"lunch\"".into()),
        },
    }
}

#[test]
fn app_yaml_round_trips_the_six_keys_and_rejects_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let c = sample();
    c.write(dir.path()).unwrap();
    assert_eq!(AppConfig::load(dir.path()).unwrap(), c);
    let mut none = sample();
    none.origin.r#ref = None;
    none.services = vec![];
    none.write(dir.path()).unwrap();
    assert_eq!(AppConfig::load(dir.path()).unwrap(), none);

    let text = std::fs::read_to_string(dir.path().join("app.yaml")).unwrap();
    for bad in [
        format!("{text}colour: red\n"),
        text.replace("kind: content-site", "kind: spaceship"),
        text.replace("open_to: workspace", "open_to: everyone"),
        text.replace("name: team-lunch", "name: Bad Name"),
        text.lines().skip(1).collect::<Vec<_>>().join("\n"),
    ] {
        std::fs::write(dir.path().join("app.yaml"), bad).unwrap();
        assert!(AppConfig::load(dir.path()).is_err());
    }
}

#[test]
fn auth_is_hook_token_only_when_all_three_vars_set() {
    let vars = |set: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            set.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    };
    let all = &[
        ("OPENCLAW_HOOK_TOKEN", "tok"),
        ("ISOL8_API_URL", "https://api.example.com/"),
        ("ISOL8_OWNER_ID", "org_1"),
    ];
    assert_eq!(
        Auth::detect_with(vars(all)),
        Auth::HookToken {
            api: "https://api.example.com/api/v1".into(),
            owner: "org_1".into(),
            token: "tok".into()
        }
    );
    for skip in 0..3 {
        let mut some = all.to_vec();
        some.remove(skip);
        let some: &'static [(&str, &str)] = Box::leak(some.into_boxed_slice());
        assert_eq!(Auth::detect_with(vars(some)), Auth::Proof);
    }
    assert_eq!(
        Auth::detect_with(vars(&[("OPENCLAW_HOOK_TOKEN", "")])),
        Auth::Proof
    );
}

#[test]
fn proof_calls_carry_signed_headers_and_no_token_in_argv() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let e = Env::new();
    e.sign_in("user_1", None);
    let (base, seen) = stub(200, json!({"app_id": "a1"}), 500);
    std::env::set_var("LIGHTBULB_CONNECT_HOME", e.state.path());
    std::env::set_var("LIGHTBULB_API", format!("{base}/api/v1"));
    let proof = lightbulb::login::stored_proof(e.state.path()).unwrap();

    // Watch every process's argv while the (slow) call is in flight.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (flag, secret) = (stop.clone(), proof.clone());
    let watcher = std::thread::spawn(move || {
        let mut curls = 0;
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            let ps = Command::new("ps")
                .args(["-axww", "-o", "args="])
                .output()
                .unwrap();
            for l in String::from_utf8_lossy(&ps.stdout).lines() {
                if l.contains("curl") && l.contains("-K") {
                    curls += 1;
                    assert!(!l.contains(&secret), "credential in argv: {l}");
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        curls
    });

    let body = json!({"name": "x"});
    api::call(&Auth::Proof, "POST", "/up", Some(&body)).unwrap();
    let file = e.state.path().join("src.tgz");
    std::fs::write(&file, b"\x1f\x8bpretend archive").unwrap();
    api::form(
        &Auth::Proof,
        "/source",
        &[("name", "x"), ("commit", "abc")],
        Some(("archive", &file)),
    )
    .unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(watcher.join().unwrap() > 0, "never saw curl run");

    let seen = seen.lock().unwrap();
    let id = e.identity();
    for (req, path) in seen
        .iter()
        .zip(["/api/v1/apps/build/up", "/api/v1/apps/build/source"])
    {
        assert!(
            req.line.starts_with(&format!("POST {path} ")),
            "{}",
            req.line
        );
        assert_eq!(req.headers["x-lightbulb-proof"], proof);
        assert!(!req.headers.contains_key("authorization"));
        let challenge = String::from_utf8(
            data_encoding::BASE64URL_NOPAD
                .decode(req.headers["x-lightbulb-challenge"].as_bytes())
                .unwrap(),
        )
        .unwrap();
        let p: Vec<&str> = challenge.split(' ').collect();
        assert_eq!((p[1], p[2]), ("POST", path), "{challenge}");
        assert_eq!(
            p[3],
            sha(&req.body),
            "the challenge binds the exact bytes sent"
        );
        let signed = lightbulb::login::proof(&id, "tools", &challenge, None);
        assert!(lightbulb::identity::verify(
            &id.key,
            &signed,
            &req.headers["x-lightbulb-sig"]
        ));
    }
    assert!(seen[0].headers["content-type"].starts_with("application/json"));
    assert!(seen[1].headers["content-type"].starts_with("multipart/form-data; boundary="));
    let form = String::from_utf8_lossy(&seen[1].body);
    assert!(form.contains("name=\"commit\"\r\n\r\nabc") && form.contains("filename=\"src.tgz\""));
    assert!(form.contains("pretend archive"));
}

#[test]
fn hook_token_calls_carry_bearer_and_owner_not_a_proof() {
    let (base, seen) = stub(200, json!({"ok": true}), 0);
    let auth = Auth::HookToken {
        api: format!("{base}/api/v1"),
        owner: "org_1".into(),
        token: "tok".into(),
    };
    api::call(&auth, "GET", "/a1/status", None).unwrap();
    let seen = seen.lock().unwrap();
    assert!(seen[0]
        .line
        .starts_with("GET /api/v1/apps/build/a1/status "));
    assert_eq!(seen[0].headers["authorization"], "Bearer tok");
    assert_eq!(seen[0].headers["x-lightbulb-owner"], "org_1");
    assert!(!seen[0].headers.contains_key("x-lightbulb-proof"));
}

#[test]
fn backend_errors_pass_through_and_unreachable_is_named() {
    let auth = |api: String| Auth::HookToken {
        api,
        owner: "o".into(),
        token: "t".into(),
    };
    let (base, _) = stub(
        409,
        json!({"code": "workspace_required", "message": "Join one.", "next": "ask an admin"}),
        0,
    );
    let ApiError {
        code,
        message,
        next,
    } = api::call(&auth(base.clone()), "GET", "/x", None).unwrap_err();
    assert_eq!(
        (code.as_str(), message.as_str(), next.as_str()),
        ("workspace_required", "Join one.", "ask an admin")
    );
    let (base, _) = stub(403, json!({"detail": "Not yours."}), 0);
    let err = api::call(&auth(base), "GET", "/x", None).unwrap_err();
    assert_eq!(err.message, "Not yours.");
    let err = api::call(&auth("http://127.0.0.1:9".into()), "GET", "/x", None).unwrap_err();
    assert_eq!(err.code, "unreachable");
}

#[test]
fn bad_name_is_refused() {
    let e = Env::new();
    for name in ["Bad Name", "a", "1abc", "ab-", "x".repeat(41).as_str()] {
        let o = e
            .base_cmd(Command::new(common::EXE))
            .args(["app", "new", name, "--json"])
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(1), "{name}");
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["code"], "bad_name", "{name}");
        assert!(v["message"].is_string() && v["next"].is_string());
    }
}

#[test]
fn not_signed_in_says_run_lightbulb_login() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let e = Env::new();
    std::env::set_var("LIGHTBULB_CONNECT_HOME", e.state.path());
    let err = api::call(&Auth::Proof, "GET", "/a/status", None).unwrap_err();
    assert_eq!(err.code, "not_signed_in");
    assert_eq!(err.next, "run lightbulb login");
}

#[test]
fn failures_print_the_json_shape_on_stdout_when_piped() {
    let e = Env::new();
    let run = |args: &[&str]| {
        e.base_cmd(Command::new(common::EXE))
            .args(args)
            .output()
            .unwrap()
    };
    // Piped stdout is not a TTY: JSON is the default.
    let o = run(&["app", "evidence", "--kind", "browser", "--file", "x.png"]);
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "not_an_app");
    assert!(v["next"].is_string());
    assert!(!run(&["app", "check"]).stdout.is_empty());
}

#[test]
fn signed_out_multipart_says_run_lightbulb_login_and_a_missing_file_is_named() {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let e = Env::new();
    std::env::set_var("LIGHTBULB_CONNECT_HOME", e.state.path());
    let file = e.state.path().join("src.tgz");
    std::fs::write(&file, b"x").unwrap();
    let err = api::form(
        &Auth::Proof,
        "/source",
        &[("name", "x")],
        Some(("archive", &file)),
    )
    .unwrap_err();
    assert_eq!(
        (err.code.as_str(), err.next.as_str()),
        ("not_signed_in", "run lightbulb login")
    );
    let gone = e.state.path().join("nope.tgz");
    let err = api::form(&Auth::Proof, "/source", &[], Some(("archive", &gone))).unwrap_err();
    assert_eq!(err.code, "bad_file");
}

#[test]
fn evidence_kind_is_a_flag_browser_or_review() {
    let e = Env::new();
    let run = |a: &[&str]| {
        e.base_cmd(Command::new(common::EXE))
            .args(a)
            .output()
            .unwrap()
    };
    for k in ["browser", "review"] {
        let o = run(&["app", "evidence", "--kind", k, "--file", "x.png"]);
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["code"], "not_an_app", "{k}");
    }
    assert_eq!(
        run(&["app", "evidence", "--kind", "recording", "--file", "x"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(run(&["app", "evidence", "browser"]).status.code(), Some(2));
}

#[test]
fn starter_table_contains_agents_md() {
    use lightbulb::apps::starter::FILES;
    let paths: Vec<&str> = FILES.iter().map(|(p, _)| *p).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    let agents = FILES.iter().find(|(p, _)| *p == "AGENTS.md").unwrap().1;
    assert!(std::str::from_utf8(agents).unwrap().contains("done: true"));
    assert_eq!(
        FILES.iter().find(|(p, _)| *p == "CLAUDE.md").unwrap().1,
        b"@AGENTS.md\n"
    );
    for p in [
        "app.yaml",
        "package.json",
        "pnpm-lock.yaml",
        "src/proxy.ts",
        "LICENSE",
        "NOTICE",
    ] {
        assert!(paths.contains(&p), "{p} missing");
    }
    assert!(!paths
        .iter()
        .any(|p| p.starts_with("node_modules/") || p.starts_with(".next/")));
}

/// `app new` in `dir`, with a git identity.
fn app_new(e: &Env, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    e.base_cmd(Command::new(common::EXE))
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(["app", "new"])
        .args(args)
        .arg("--json")
        .output()
        .unwrap()
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).to_string()
}

#[test]
fn new_writes_the_starter_and_one_commit() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    let o = app_new(&e, tmp.path(), &["team-lunch"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["name"], "team-lunch");
    assert_eq!(
        v["next"],
        "cd team-lunch && pnpm install, then lightbulb app check"
    );
    let dir = tmp.path().join("team-lunch");
    assert_eq!(
        v["files"].as_u64().unwrap() as usize,
        git(&dir, &["ls-files"]).lines().count()
    );
    assert_eq!(
        std::fs::canonicalize(v["dir"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&dir).unwrap()
    );
    // A site (the default kind) has the content switch: its pages sit in the (site) group.
    assert!(dir.join("src/app/(site)/%5Flightbulb").is_dir());
    let cfg = AppConfig::load(&dir).unwrap();
    assert_eq!(cfg.name, "team-lunch");
    assert_eq!(cfg.open_to, OpenTo::Workspace);
    assert_eq!(cfg.origin.kind, "ask");
    assert_eq!(cfg.services, ["vercel/project", "neon/postgres", "content"]);
    assert_eq!(cfg.budget_usd, 10);
    let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(!pkg.contains("{{name}}"));
    assert_eq!(git(&dir, &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert_eq!(
        git(&dir, &["log", "-1", "--format=%s"]).trim(),
        "lightbulb app new team-lunch"
    );
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
}

#[test]
fn new_content_site_overlays_payload() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    let o = app_new(&e, tmp.path(), &["news-room", "--kind", "content-site"]);
    assert_eq!(o.status.code(), Some(0));
    let dir = tmp.path().join("news-room");
    let config = std::fs::read_to_string(dir.join("src/payload.config.ts")).unwrap();
    assert!(config.contains("headline: \"news-room\""));
    let cfg = AppConfig::load(&dir).unwrap();
    assert_eq!(cfg.kind, Kind::ContentSite);
    assert_eq!(cfg.services, ["vercel/project", "neon/postgres", "content"]);
    for p in [
        "src/app/(payload)/admin/importMap.js",
        "src/app/(site)/layout.tsx",
        "src/app/(site)/%5Flightbulb/closed/page.tsx",
        "src/app/_lightbulb/caps.ts",
        "src/components/lightbulb/edit-overlay.tsx",
        "scripts/content.mjs",
    ] {
        assert!(dir.join(p).is_file(), "{p} missing");
    }
    assert!(!dir.join("src/app/layout.tsx").exists());
    assert!(!dir.join("addons").exists());
    let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(pkg.contains("\"payload\": \"3.92.1\""));
    assert!(pkg.contains("\"name\": \"news-room\""));
    let layout = std::fs::read_to_string(dir.join("src/app/(site)/layout.tsx")).unwrap();
    assert!(layout.contains("title: \"news-room\""));
    // Nothing from the scratch app the starter was built in.
    for f in git(&dir, &["ls-files"]).lines() {
        let text = String::from_utf8_lossy(&std::fs::read(dir.join(f)).unwrap()).to_string();
        assert!(!text.contains("demo-content"), "{f} names demo-content");
    }
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
}

#[test]
fn new_form_has_no_content_switch() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    let o = app_new(&e, tmp.path(), &["sign-up", "--kind", "form"]);
    assert_eq!(o.status.code(), Some(0));
    let dir = tmp.path().join("sign-up");
    assert!(!dir.join("addons").exists());
    let layout = std::fs::read_to_string(dir.join("src/app/layout.tsx")).unwrap();
    assert!(layout.contains("title: \"sign-up\""));
    assert!(!git(&dir, &["ls-files"]).to_lowercase().contains("payload"));
    let pkg = std::fs::read_to_string(dir.join("package.json")).unwrap();
    assert!(!pkg.contains("payload"));
    let cfg = AppConfig::load(&dir).unwrap();
    assert_eq!(cfg.services, ["vercel/project", "neon/postgres"]);
}

#[test]
fn content_addon_keeps_the_base_package_json_and_workspace() {
    use lightbulb::apps::starter::FILES;
    let file = |p: &str| FILES.iter().find(|(f, _)| *f == p).unwrap().1;
    let json = |p: &str| serde_json::from_slice::<Value>(file(p)).unwrap();
    let (base, content) = (json("package.json"), json("addons/content/package.json"));
    for key in ["dependencies", "devDependencies", "scripts"] {
        for (k, v) in base[key].as_object().unwrap() {
            assert_eq!(
                &content[key][k], v,
                "{key}.{k}: copy it into addons/content/package.json, then pnpm install there"
            );
        }
    }
    let ws = std::str::from_utf8(file("pnpm-workspace.yaml")).unwrap();
    let content_ws = std::str::from_utf8(file("addons/content/pnpm-workspace.yaml")).unwrap();
    assert!(content_ws.starts_with(ws));
}

#[test]
fn new_refuses_a_non_empty_dir() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("team-lunch")).unwrap();
    std::fs::write(tmp.path().join("team-lunch/keep.txt"), "mine").unwrap();
    let o = app_new(&e, tmp.path(), &["team-lunch"]);
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "dir_not_empty");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("team-lunch/keep.txt")).unwrap(),
        "mine"
    );
    assert!(!tmp.path().join("team-lunch/app.yaml").exists());
}

#[test]
fn new_sets_kind_and_plan_from_the_playbook() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    let o = app_new(
        &e,
        tmp.path(),
        &["pay-us", "--kind", "payments", "--room", "r-1"],
    );
    assert_eq!(o.status.code(), Some(0));
    let dir = tmp.path().join("pay-us");
    let cfg = AppConfig::load(&dir).unwrap();
    assert_eq!(cfg.kind, Kind::Payments);
    assert_eq!(cfg.open_to, OpenTo::Room);
    let app: Value =
        serde_json::from_slice(&std::fs::read(dir.join(".lightbulb/app.json")).unwrap()).unwrap();
    assert_eq!(app, json!({"room_id": "r-1"}));
    let playbook = std::fs::read_to_string(dir.join("playbooks/payments.md")).unwrap();
    let plan = std::fs::read_to_string(dir.join(".lightbulb/plan.md")).unwrap();
    assert!(plan.starts_with(&playbook));
    assert!(plan.contains("## What was asked"));
}

#[test]
fn up_outside_an_app_refuses() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    let o = e
        .base_cmd(Command::new(common::EXE))
        .current_dir(tmp.path())
        .args(["app", "up"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "not_an_app");
    let names: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from(".git")]);
}

#[test]
fn status_reads_the_app_id_and_asks_the_backend() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    app_new(&e, tmp.path(), &["team-lunch"]);
    let dir = tmp.path().join("team-lunch");
    let run = |args: &[&str], base: &str| {
        e.base_cmd(Command::new(common::EXE))
            .current_dir(&dir)
            .env("LIGHTBULB_APP_POLL_SCALE_MS", "1")
            .env("OPENCLAW_HOOK_TOKEN", "tok")
            .env("ISOL8_API_URL", base)
            .env("ISOL8_OWNER_ID", "o1")
            .args(["app", "status"])
            .args(args)
            .output()
            .unwrap()
    };
    let o = run(&[], "http://127.0.0.1:9");
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "not_up_yet");
    assert_eq!(v["next"], "run lightbulb app up");

    std::fs::write(dir.join(".lightbulb/app.json"), r#"{"app_id":"a-9"}"#).unwrap();
    let (base, seen) = stub(200, json!({"state": "ready"}), 0);
    let o = run(&["--errors"], &base);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["status"]["state"], "ready");
    assert!(v["errors"].is_object());
    let lines: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.line.clone())
        .collect();
    assert_eq!(
        lines,
        [
            "GET /api/v1/apps/build/a-9/status HTTP/1.1",
            "GET /api/v1/apps/build/a-9/errors HTTP/1.1"
        ]
    );
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/apps")
        .join(name)
}

/// A new app with a fake `pnpm` first on PATH that prints `fixture` and exits `code`.
struct Checked {
    e: Env,
    dir: std::path::PathBuf,
    bin: tempfile::TempDir,
    _tmp: tempfile::TempDir,
}

impl Checked {
    fn new() -> Checked {
        let e = Env::new();
        let tmp = tempfile::tempdir().unwrap();
        app_new(&e, tmp.path(), &["team-lunch"]);
        let bin = tempfile::tempdir().unwrap();
        let pnpm = bin.path().join("pnpm");
        std::fs::write(
            &pnpm,
            "#!/bin/sh\necho \"$@\" >> \"$FAKE_PNPM_ARGS\"\nsed \"s|{ROOT}|$PWD|\" \"$FAKE_PNPM_OUT\"\nexit $FAKE_PNPM_EXIT\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pnpm, std::fs::Permissions::from_mode(0o755)).unwrap();
        // As after `pnpm install` (git ignores it).
        std::fs::create_dir(tmp.path().join("team-lunch/node_modules")).unwrap();
        Checked {
            dir: tmp.path().join("team-lunch"),
            e,
            bin,
            _tmp: tmp,
        }
    }

    fn check(&self, out: &str, code: i32, args: &[&str]) -> (i32, Value) {
        let path = format!(
            "{}:{}",
            self.bin.path().display(),
            std::env::var("PATH").unwrap()
        );
        let o = self
            .e
            .base_cmd(Command::new(common::EXE))
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_PNPM_OUT", fixture(out))
            .env("FAKE_PNPM_EXIT", code.to_string())
            .env("FAKE_PNPM_ARGS", self.bin.path().join("args"))
            .args(["app", "check", "--json"])
            .args(args)
            .output()
            .unwrap();
        let v = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&o.stdout)));
        (o.status.code().unwrap(), v)
    }

    fn progress(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir.join(".lightbulb/progress.json")).unwrap())
            .unwrap()
    }
}

#[test]
fn parses_tsc_errors_into_file_line_code() {
    let c = Checked::new();
    let (code, v) = c.check("tsc.txt", 1, &[]);
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    let p = v["problems"].as_array().unwrap();
    assert_eq!(p.len(), 2, "{v}");
    assert_eq!(
        p[0],
        json!({"file": "src/app/page.tsx", "line": 12, "column": 7, "code": "TS2322",
        "message": "Type 'number' is not assignable to type 'string'.\n  The expected type comes from property 'title'."})
    );
    assert_eq!(p[1]["file"], "src/lib/db.ts");
    assert_eq!(p[1]["code"], "TS2304");
    assert_eq!(v["next"], "fix");
    let args = std::fs::read_to_string(c.bin.path().join("args")).unwrap();
    assert_eq!(args.trim(), "run check");
    c.check("tsc.txt", 1, &["--publish"]);
    let args = std::fs::read_to_string(c.bin.path().join("args")).unwrap();
    assert!(args.contains("run check:publish"), "{args}");
}

#[test]
fn eslint_errors_are_problems_with_the_rule_as_code() {
    let c = Checked::new();
    let (_, v) = c.check("eslint.txt", 1, &[]);
    assert_eq!(
        v["problems"],
        json!([{"file": "src/app/page.tsx", "line": 4, "column": 10,
        "code": "@typescript-eslint/no-unused-vars", "message": "'useState' is defined but never used"}])
    );
}

#[test]
fn same_error_twice_says_diagnose() {
    let c = Checked::new();
    assert_eq!(c.check("tsc.txt", 1, &[]).1["next"], "fix");
    let (_, v) = c.check("tsc.txt", 1, &[]);
    assert_eq!(
        v["next"],
        "diagnose: list what you tried, find the root cause, consider a stronger model for a fresh plan"
    );
}

#[test]
fn same_error_three_times_says_undo() {
    let c = Checked::new();
    // Another key in progress.json survives.
    std::fs::write(
        c.dir.join(".lightbulb/progress.json"),
        r#"{"evidence":{"x":1}}"#,
    )
    .unwrap();
    for _ in 0..2 {
        c.check("tsc.txt", 1, &[]);
    }
    let (_, v) = c.check("tsc.txt", 1, &[]);
    assert_eq!(
        v["next"],
        "undo: run lightbulb app rollback or git revert, then tell the room what failed"
    );
    // A new error does not lower the next step: it follows the highest count.
    let (_, v) = c.check("design.txt", 1, &[]);
    assert_eq!(v["next"], "fix");
    let p = c.progress();
    assert_eq!(p["evidence"], json!({"x": 1}));
    assert_eq!(p["last_check"]["ok"], false);
    let mut counts: Vec<u64> = p["attempts"]
        .as_object()
        .unwrap()
        .values()
        .map(|n| n.as_u64().unwrap())
        .collect();
    counts.sort();
    assert_eq!(counts, [1, 1, 1, 3, 3]);
}

#[test]
fn passing_check_clears_counts() {
    let c = Checked::new();
    c.check("tsc.txt", 1, &[]);
    assert_eq!(c.progress()["attempts"].as_object().unwrap().len(), 2);
    let (code, v) = c.check("design.txt", 0, &[]);
    // A zero exit is a pass even if the output had text.
    assert_eq!((code, &v["ok"]), (0, &json!(true)));
    assert_eq!(v["problems"], json!([]));
    assert_eq!(c.progress()["attempts"], json!({}));
    assert_eq!(c.progress()["last_check"]["ok"], true);
}

#[test]
fn design_lint_raw_hex_is_a_problem() {
    let c = Checked::new();
    let (_, v) = c.check("design.txt", 1, &[]);
    let mut first = v["problems"][0].clone();
    // The shipped known-fixes.md has an entry for design/tokens.css.
    assert!(first.as_object_mut().unwrap().remove("hint").is_some());
    assert_eq!(
        first,
        json!({"file": "src/app/page.tsx", "line": 8, "column": null, "code": null,
        "message": "raw hex colour; use a token from design/tokens.css"})
    );
    assert_eq!(v["problems"].as_array().unwrap().len(), 3);
    assert_eq!(v["problems"][2]["file"], "src/app/(auth)/page.tsx");
}

#[test]
fn unreadable_progress_is_an_error_not_overwritten() {
    let c = Checked::new();
    let path = c.dir.join(".lightbulb/progress.json");
    std::fs::write(&path, "{not json").unwrap();
    let (code, v) = c.check("tsc.txt", 1, &[]);
    assert_eq!((code, &v["code"]), (1, &json!("bad_progress")));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
}

#[test]
fn an_unparsed_failure_repeats_to_undo() {
    let c = Checked::new();
    for _ in 0..2 {
        c.check("unknown.txt", 1, &[]);
    }
    let (_, v) = c.check("unknown.txt", 1, &[]);
    assert!(v["next"].as_str().unwrap().starts_with("undo"), "{v}");
    assert_eq!(v["problems"][0]["message"], "audit: 1 high vulnerability");
}

#[test]
fn missing_pnpm_says_how_to_install_it() {
    let c = Checked::new();
    // A PATH holding git (preflight needs it) and no pnpm.
    let only_git = tempfile::tempdir().unwrap();
    let git = Command::new("which").arg("git").output().unwrap().stdout;
    std::os::unix::fs::symlink(
        String::from_utf8(git).unwrap().trim(),
        only_git.path().join("git"),
    )
    .unwrap();
    let o =
        c.e.base_cmd(Command::new(common::EXE))
            .current_dir(&c.dir)
            .env("PATH", only_git.path())
            .args(["app", "check", "--json"])
            .output()
            .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "tool_missing");
    assert!(v["next"].as_str().unwrap().contains("pnpm"));
}

fn kinds(sql: &str) -> Vec<&'static str> {
    classify(sql).iter().map(|f| f.kind).collect()
}

#[test]
fn rename_column_is_destructive() {
    assert_eq!(
        kinds("ALTER TABLE \"items\" RENAME COLUMN \"name\" TO \"title\";"),
        ["rename_column"]
    );
    assert_eq!(
        kinds("ALTER TABLE items RENAME TO things;"),
        ["rename_table"]
    );
    assert_eq!(
        kinds("ALTER TABLE items RENAME CONSTRAINT a TO b;"),
        [] as [&str; 0]
    );
    assert_eq!(
        kinds("ALTER TABLE items ALTER COLUMN n SET DATA TYPE integer;"),
        ["alter_type"]
    );
    assert_eq!(
        kinds("ALTER TABLE items ALTER COLUMN n SET NOT NULL;"),
        ["set_not_null"]
    );
    assert_eq!(kinds("TRUNCATE items;"), ["truncate"]);
    assert_eq!(kinds("delete from items where id = 1;"), ["delete"]);
    assert_eq!(
        kinds("ALTER TABLE items DROP COLUMN name;"),
        ["drop_column"]
    );
}

#[test]
fn add_column_is_not() {
    let sql = "CREATE TABLE a (id int);\n--> statement-breakpoint\n\
        ALTER TABLE items ADD COLUMN note text NOT NULL DEFAULT '';\n\
        ALTER TABLE items ALTER COLUMN n DROP NOT NULL;\n\
        ALTER TABLE items DROP CONSTRAINT items_pkey;\n\
        CREATE INDEX i ON items (name);\n\
        -- drop table nothing;\n\
        /* delete from x; */\n\
        INSERT INTO items (name) VALUES ('drop table x; delete from y');";
    assert!(classify(sql).is_empty(), "{:?}", classify(sql));
}

#[test]
fn drop_table_lists_the_statement() {
    let f = classify("CREATE TABLE a (id int);\n--> statement-breakpoint\nDROP TABLE \"items\" CASCADE;\nSELECT 1;");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].kind, "drop_table");
    assert_eq!(f[0].statement, "DROP TABLE \"items\" CASCADE");
    // A semicolon inside a string or dollar quote does not split.
    assert_eq!(
        kinds("DO $$ BEGIN DELETE FROM t; END $$; SELECT ';'; DROP TABLE t;"),
        ["dynamic_sql", "drop_table"]
    );
}

fn app_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("drizzle")).unwrap();
    for (n, s) in files {
        std::fs::write(d.path().join("drizzle").join(n), s).unwrap();
    }
    d
}

#[test]
fn recorded_migrations_are_not_rechecked() {
    let d = app_dir(&[
        ("0000_init.sql", "DROP TABLE old;"),
        ("0001_more.sql", "ALTER TABLE a ADD COLUMN b text;"),
        ("seed.sql", "DELETE FROM items;"),
    ]);
    // Nothing recorded: the destructive file is flagged; seed.sql is data and is ignored.
    let g = gate(d.path()).unwrap();
    assert_eq!(g.unrecorded, ["0000_init.sql", "0001_more.sql"]);
    assert_eq!(g.findings.len(), 1);
    assert_eq!(g.findings[0].0, "0000_init.sql");
    // Once recorded, it is not looked at again.
    std::fs::create_dir_all(d.path().join(".lightbulb")).unwrap();
    std::fs::write(
        d.path().join(".lightbulb/migrations.json"),
        r#"{"applied": ["0000_init.sql"]}"#,
    )
    .unwrap();
    let g = gate(d.path()).unwrap();
    assert_eq!(g.unrecorded, ["0001_more.sql"]);
    assert!(g.findings.is_empty());
    // An unreadable record is an error, never "nothing recorded".
    std::fs::write(d.path().join(".lightbulb/migrations.json"), "nope").unwrap();
    assert_eq!(gate(d.path()).unwrap_err().code, "bad_migrations");
}

#[test]
fn up_stops_on_a_destructive_migration_unless_approved() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(app_new(&e, tmp.path(), &["gated"]).status.code(), Some(0));
    let dir = tmp.path().join("gated");
    // The starter's own migration is add-only.
    let run = |extra: &[&str]| {
        let o = e
            .base_cmd(Command::new(common::EXE))
            .current_dir(&dir)
            .args(["app", "up", "--json"])
            .args(extra)
            .output()
            .unwrap();
        serde_json::from_slice::<Value>(&o.stdout).unwrap()
    };
    // Past the gate: whatever fails next, it is not the migration.
    assert_ne!(run(&[])["code"], "destructive_migration");
    std::fs::write(dir.join("drizzle/0001_drop.sql"), "DROP TABLE \"items\";").unwrap();
    let v = run(&[]);
    assert_eq!(v["code"], "destructive_migration");
    assert_eq!(v["message"], "DROP TABLE \"items\"");
    assert_eq!(
        v["next"],
        "rewrite add-only (add a column, copy data), or ask the owner and pass --approved <card id>"
    );
    assert_ne!(
        run(&["--approved", "card-1"])["code"],
        "destructive_migration"
    );
}

#[test]
fn code_the_gate_cannot_read_fails_closed() {
    for sql in ["CALL wipe();", "EXECUTE p;", "PREPARE p AS DELETE FROM t;"] {
        assert_eq!(kinds(sql), ["dynamic_sql"], "{sql}");
    }
    assert_eq!(
        kinds("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d;"),
        ["delete"]
    );
    assert_eq!(
        kinds("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE;"),
        ["delete"]
    );
    assert_eq!(kinds("EXPLAIN ANALYZE DELETE FROM t;"), ["delete"]);
    assert_eq!(
        kinds("EXPLAIN (ANALYZE, VERBOSE) DROP TABLE t;"),
        ["drop_table"]
    );
    assert!(kinds("EXPLAIN DELETE FROM t;").is_empty());
    assert!(kinds("WITH x AS (SELECT 1) SELECT * FROM x;").is_empty());
}

#[test]
fn drop_database_owned_and_cascade_are_drop_table() {
    for sql in [
        "DROP DATABASE d;",
        "DROP OWNED BY u;",
        "DROP VIEW v CASCADE;",
        "DROP TYPE t CASCADE;",
    ] {
        assert_eq!(kinds(sql), ["drop_table"], "{sql}");
    }
    assert!(kinds("DROP INDEX i;").is_empty());
}

#[test]
fn e_string_escapes_do_not_hide_a_statement() {
    assert_eq!(
        kinds("SELECT E'\\'' ; DROP TABLE x; SELECT 'a';"),
        ["drop_table"]
    );
    // A name ending in e is not an E-string: the quote closes normally.
    assert_eq!(kinds("SELECT name'\\'; DROP TABLE x;"), ["drop_table"]);
}

#[test]
fn alter_type_without_the_word_column() {
    assert_eq!(kinds("ALTER TABLE t ALTER c TYPE int;"), ["alter_type"]);
    assert_eq!(
        kinds("ALTER TABLE t ALTER c SET DATA TYPE int;"),
        ["alter_type"]
    );
}

#[test]
fn an_empty_approval_is_refused() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(app_new(&e, tmp.path(), &["empty"]).status.code(), Some(0));
    let o = e
        .base_cmd(Command::new(common::EXE))
        .current_dir(tmp.path().join("empty"))
        .args(["app", "up", "--json", "--approved", "  "])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "bad_args");
}

// ---- up, publish, rollback, destroy ----

const BUILD: &str = "/api/v1/apps/build";

fn waiting() -> Value {
    json!({"app_id": "a-1", "state": "awaiting_approval", "canvas_url": "https://x/chat?panel=canvas&app=a-1",
        "cards": [{"workflow_id": "w1", "provider": "stripe", "service": "payments", "state": "pending",
            "token": "sk_live_SECRET123"}],
        "sample_data": true, "token": "sk_live_SECRET123",
        "DATABASE_URL": "postgres://u:SECRET@h/db"})
}

fn ship_stub(up: Value) -> (String, Arc<Mutex<Vec<Req>>>) {
    stub_with(
        move |line| {
            let route = line.trim_end_matches(" HTTP/1.1");
            match route.strip_prefix(&format!("POST {BUILD}")).unwrap_or("") {
                "/source" => (200, json!({"app_id": "a-1", "source_id": "s-1"})),
                "/up" | "/a-1/publish" => (200, up.clone()),
                "/a-1/destroy" => (
                    200,
                    json!({"state": "awaiting_owner", "next": "the owner removes it from Canvas",
                    "DATABASE_URL": "postgres://u:SECRET@h/db"}),
                ),
                _ => (
                    404,
                    json!({"code": "no_route", "message": route, "next": ""}),
                ),
            }
        },
        0,
    )
}

impl Checked {
    /// `lightbulb app <args> --json` against `base`, with a check that passes or fails.
    fn app(&self, args: &[&str], base: &str, check_ok: bool) -> (i32, Value) {
        let path = format!(
            "{}:{}",
            self.bin.path().display(),
            std::env::var("PATH").unwrap()
        );
        let (out, code) = if check_ok {
            ("design.txt", "0")
        } else {
            ("tsc.txt", "1")
        };
        let o = self
            .e
            .base_cmd(Command::new(common::EXE))
            .current_dir(&self.dir)
            .env("PATH", path)
            .env("FAKE_PNPM_OUT", fixture(out))
            .env("FAKE_PNPM_EXIT", code)
            .env("FAKE_PNPM_ARGS", self.bin.path().join("args"))
            .env("LIGHTBULB_APP_POLL_SCALE_MS", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("OPENCLAW_HOOK_TOKEN", "tok")
            .env("ISOL8_API_URL", base)
            .env("ISOL8_OWNER_ID", "o1")
            .arg("app")
            .args(args)
            .arg("--json")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(!text.contains("SECRET"), "a credential was printed: {text}");
        let v = serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{text}"));
        (o.status.code().unwrap(), v)
    }

    fn head(&self) -> String {
        git(&self.dir, &["rev-parse", "HEAD"]).trim().to_string()
    }

    fn progress_file(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir.join(".lightbulb/progress.json")).unwrap())
            .unwrap()
    }

    fn app_json(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.dir.join(".lightbulb/app.json")).unwrap())
            .unwrap()
    }
}

fn lines(seen: &Arc<Mutex<Vec<Req>>>) -> Vec<String> {
    let seen = seen.lock().unwrap();
    seen.iter()
        .map(|r| r.line.trim_end_matches(" HTTP/1.1").to_string())
        .collect()
}

fn body(r: &Req) -> Value {
    serde_json::from_slice(&r.body).unwrap()
}

#[test]
fn archive_excludes_env_and_build_dirs() {
    let c = Checked::new();
    for f in [
        ".env",
        ".env.local",
        "sub/.env.production",
        "node_modules/x/a.js",
        "sub/node_modules/b.js",
        ".next/b",
        ".vercel/c",
        "keep.txt",
        "src/ok.ts",
    ] {
        let p = c.dir.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
    }
    // Even forced into git, they stay out of the archive.
    git(&c.dir, &["add", "-f", "-A"]);
    git(
        &c.dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "forced",
        ],
    );
    let (tgz, sha) = lightbulb::apps::archive(&c.dir).unwrap();
    assert_eq!(sha, c.head());
    let o = Command::new("tar").arg("-tzf").arg(&tgz).output().unwrap();
    let names = String::from_utf8_lossy(&o.stdout).to_string();
    for want in ["keep.txt", "src/ok.ts", "app.yaml"] {
        assert!(names.lines().any(|n| n == want), "{want} missing: {names}");
    }
    for bad in [".env", "node_modules", ".next", ".vercel", ".git/"] {
        assert!(!names.contains(bad), "{bad} shipped: {names}");
    }
    std::fs::remove_file(tgz).unwrap();
}

#[test]
fn up_commits_a_dirty_tree_first() {
    let c = Checked::new();
    std::fs::write(c.dir.join("notes.txt"), "hi").unwrap();
    let (base, seen) = ship_stub(waiting());
    let (code, _) = c.app(&["up"], &base, true);
    assert_eq!(code, 0);
    assert_eq!(git(&c.dir, &["rev-list", "--count", "HEAD"]).trim(), "2");
    let subject = git(&c.dir, &["log", "-1", "--format=%s"]);
    assert!(
        subject.starts_with("lightbulb: ") && subject.contains("notes.txt"),
        "{subject}"
    );
    assert_eq!(git(&c.dir, &["status", "--porcelain"]), "");
    let seen = seen.lock().unwrap();
    let form = String::from_utf8_lossy(&seen[0].body).to_string();
    assert!(form.contains(&c.head()), "the commit is sent");
    assert!(form.contains("name=\"archive\""));
}

#[test]
fn up_prints_cards_and_exits_zero_when_waiting() {
    let c = Checked::new();
    let (base, seen) = ship_stub(waiting());
    let (code, v) = c.app(&["up", "--room", "r-9"], &base, true);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["state"], "awaiting_approval");
    assert_eq!(v["app_id"], "a-1");
    assert_eq!(v["cards"][0]["provider"], "stripe");
    assert_eq!(v["done"], false);
    assert!(v["next"].as_str().unwrap().contains("Inbox"), "{v}");
    assert_eq!(c.app_json(), json!({"app_id": "a-1", "room_id": "r-9"}));
    assert_eq!(
        lines(&seen),
        [format!("POST {BUILD}/source"), format!("POST {BUILD}/up")]
    );
    let seen = seen.lock().unwrap();
    assert!(String::from_utf8_lossy(&seen[0].body).contains("r-9"));
    let up = body(&seen[1]);
    assert_eq!(up["name"], "team-lunch");
    assert_eq!(up["room_id"], "r-9");
    assert_eq!(up["source_id"], "s-1");
    // A site (the default kind) has the content switch.
    assert_eq!(
        up["services"],
        json!(["vercel/project", "neon/postgres", "content"])
    );
    assert_eq!(up["open_to"], "workspace");
    assert_eq!(up["budget_usd"], 10);
    assert_eq!(up["origin"]["kind"], "ask");
    assert!(up.get("approved_destructive").is_none());
    drop(seen);
    // A failed build is a failure: the backend's error as {code, message, next}, exit 1.
    let (base, _) = ship_stub(json!({"app_id": "a-1", "state": "failed",
        "error": {"code": "build", "message": "it broke", "next": "fix the build"}}));
    let (code, v) = c.app(&["up", "--approved", "card-1"], &base, true);
    assert_eq!(code, 1);
    assert_eq!(
        v,
        json!({"code": "build", "message": "it broke", "next": "fix the build"})
    );
    // No room: null.
    std::fs::remove_file(c.dir.join(".lightbulb/app.json")).unwrap();
    let (base, seen) = ship_stub(waiting());
    c.app(&["up", "--approved", "card-1"], &base, true);
    let up = body(&seen.lock().unwrap()[1]);
    assert_eq!(up["room_id"], Value::Null);
    assert_eq!(up["approved_destructive"], "card-1");
}

#[test]
fn up_refuses_when_check_fails() {
    let c = Checked::new();
    let (base, seen) = ship_stub(waiting());
    let (code, v) = c.app(&["up"], &base, false);
    assert_eq!((code, &v["code"]), (1, &json!("check_failed")));
    assert!(v["message"].as_str().unwrap().contains("src/app/page.tsx"));
    assert_eq!(v["next"], "fix");
    assert!(lines(&seen).is_empty());
}

#[test]
fn publish_requires_review_evidence() {
    let c = Checked::new();
    std::fs::write(c.dir.join(".lightbulb/app.json"), r#"{"app_id":"a-1"}"#).unwrap();
    let (base, seen) = ship_stub(json!({"app_id": "a-1", "state": "ready", "version": 2}));
    let review = |commit: &str, verdict: &str, kind: &str| {
        std::fs::write(
            c.dir.join(".lightbulb/progress.json"),
            json!({"evidence": [{"kind": kind, "commit": commit, "verdict": verdict}]}).to_string(),
        )
        .unwrap();
    };
    for (commit, verdict, kind) in [
        ("old", "pass", "review"),
        (c.head().as_str(), "fail", "review"),
        (c.head().as_str(), "pass", "browser"),
    ] {
        review(commit, verdict, kind);
        let (code, v) = c.app(&["publish"], &base, true);
        assert_eq!((code, &v["code"]), (1, &json!("review_required")));
        assert!(v["next"].as_str().unwrap().contains("--kind review"));
    }
    assert!(lines(&seen).is_empty());

    review(&c.head(), "pass", "review");
    let (code, v) = c.app(&["publish", "--reason", "launch"], &base, true);
    assert_eq!((code, &v["state"]), (0, &json!("ready")), "{v}");
    assert_eq!(v["version"], 2);
    assert_eq!(
        lines(&seen),
        [
            format!("POST {BUILD}/source"),
            format!("POST {BUILD}/a-1/publish")
        ]
    );
    let p = body(&seen.lock().unwrap()[1]);
    assert_eq!(
        (&p["source_id"], &p["reason"]),
        (&json!("s-1"), &json!("launch"))
    );
    // The migrations in this source are now recorded.
    let m: Value =
        serde_json::from_slice(&std::fs::read(c.dir.join(".lightbulb/migrations.json")).unwrap())
            .unwrap();
    assert!(!m["applied"].as_array().unwrap().is_empty(), "{m}");
}

#[test]
fn publish_before_up_says_not_up_yet() {
    let c = Checked::new();
    let (base, _) = ship_stub(waiting());
    let (code, v) = c.app(&["publish"], &base, true);
    assert_eq!((code, &v["code"]), (1, &json!("not_up_yet")));
}

#[test]
fn rollback_is_a_new_commit_not_a_reset() {
    let c = Checked::new();
    std::fs::write(c.dir.join(".lightbulb/app.json"), r#"{"app_id":"a-1"}"#).unwrap();
    let c1 = c.head();
    let mut commits = vec![c1.clone()];
    for n in 2..=3 {
        std::fs::write(c.dir.join("f.txt"), format!("v{n}")).unwrap();
        std::fs::write(c.dir.join(format!("added{n}.txt")), "x").unwrap();
        git(&c.dir, &["add", "-A"]);
        git(
            &c.dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                &format!("v{n}"),
            ],
        );
        commits.push(c.head());
    }
    let versions: Vec<Value> = commits
        .iter()
        .enumerate()
        .map(|(i, c)| json!({"n": i + 1, "commit": c}))
        .collect();
    let rolled = json!({"app_id": "a-1", "state": "ready", "version": 1,
        "error": {"code": "data_unchanged", "message": "Code only.", "next": "Data is unchanged: the app's data stays as it is."}});
    let (base, seen) = stub_with(
        move |line| {
            if line.starts_with("GET") {
                (200, json!({"live_version": 3, "versions": versions}))
            } else {
                (200, rolled.clone())
            }
        },
        0,
    );
    let (code, v) = c.app(&["rollback", "1"], &base, true);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        v["next"],
        "Data is unchanged: the app's data stays as it is."
    );
    assert_eq!(
        body(&seen.lock().unwrap()[1]),
        json!({"version": 1}),
        "{:?}",
        lines(&seen)
    );
    // The tree is version 1's (files added since are gone) in exactly one new commit.
    assert_eq!(git(&c.dir, &["diff", &c1, "HEAD"]), "");
    assert!(!c.dir.join("added3.txt").exists());
    assert_eq!(git(&c.dir, &["rev-list", "--count", "HEAD"]).trim(), "4");
    assert_eq!(
        git(&c.dir, &["log", "-1", "--format=%s"]).trim(),
        "lightbulb: roll back to version 1"
    );
    assert_ne!(c.head(), c1);
    for old in &commits {
        let ok = Command::new("git")
            .current_dir(&c.dir)
            .args(["merge-base", "--is-ancestor", old, "HEAD"])
            .status()
            .unwrap();
        assert!(ok.success(), "{old} is no longer in history");
    }
    assert_eq!(git(&c.dir, &["status", "--porcelain"]), "");
}

#[test]
fn destroy_prints_next_and_no_secret_in_any_output() {
    let c = Checked::new();
    std::fs::write(c.dir.join(".lightbulb/app.json"), r#"{"app_id":"a-1"}"#).unwrap();
    // `app` fails the test if SECRET shows up in stdout or stderr.
    let (base, _) = ship_stub(waiting());
    let (code, v) = c.app(&["destroy"], &base, true);
    assert_eq!(code, 0);
    assert_eq!(v["state"], "awaiting_owner");
    assert_eq!(v["next"], "the owner removes it from Canvas");
    let (code, v) = c.app(&["up"], &base, true);
    assert_eq!(code, 0);
    assert!(v.get("token").is_none() && v["cards"][0].get("token").is_none());
}

#[test]
fn rollback_refuses_a_dirty_tree_and_sends_nothing() {
    let c = Checked::new();
    std::fs::write(c.dir.join(".lightbulb/app.json"), r#"{"app_id":"a-1"}"#).unwrap();
    std::fs::write(c.dir.join("app.yaml.bak"), "mine").unwrap();
    let (base, seen) = ship_stub(waiting());
    let (code, v) = c.app(&["rollback", "1"], &base, true);
    assert_eq!((code, &v["code"]), (1, &json!("dirty_tree")));
    assert_eq!(v["next"], "commit or discard your changes, then roll back");
    assert!(lines(&seen).is_empty());
}

/// A backend whose `POST /up` and `/publish` answer `building` and whose status is `status`.
fn building_stub(status: Value) -> (String, Arc<Mutex<Vec<Req>>>) {
    stub_with(
        move |line| {
            let route = line.trim_end_matches(" HTTP/1.1");
            match route.split_once(BUILD).map_or("", |(_, r)| r) {
                "/source" => (200, json!({"app_id": "a-1", "source_id": "s-1"})),
                "/up" | "/a-1/publish" => (200, json!({"app_id": "a-1", "state": "building"})),
                "/a-1/status" => (200, status.clone()),
                _ => (
                    404,
                    json!({"code": "no_route", "message": route, "next": ""}),
                ),
            }
        },
        0,
    )
}

#[test]
fn a_building_publish_records_no_migrations() {
    let c = Checked::new();
    std::fs::write(c.dir.join(".lightbulb/app.json"), r#"{"app_id":"a-1"}"#).unwrap();
    std::fs::write(
        c.dir.join(".lightbulb/progress.json"),
        json!({"evidence": [{"kind": "review", "commit": c.head(), "verdict": "pass"}]})
            .to_string(),
    )
    .unwrap();
    // Still building when the wait gives up: nothing recorded, and the next step is status.
    let (base, seen) = building_stub(json!({"app_id": "a-1", "state": "building"}));
    let (code, v) = c.app(&["publish"], &base, true);
    assert_eq!((code, &v["state"]), (0, &json!("building")), "{v}");
    assert!(!c.dir.join(".lightbulb/migrations.json").exists());
    let next = v["next"].as_str().unwrap();
    assert!(
        next.contains("lightbulb app status") && !next.contains("again"),
        "{next}"
    );
    assert!(
        lines(&seen)
            .iter()
            .filter(|l| l.ends_with("/status"))
            .count()
            > 3
    );
    // Live after the wait (R21): recorded, and the next step names the live address.
    let (base, _) = building_stub(
        json!({"app_id": "a-1", "state": "live", "live_url": "https://team-lunch.example"}),
    );
    let (code, v) = c.app(&["publish"], &base, true);
    assert_eq!((code, &v["state"]), (0, &json!("live")), "{v}");
    assert_eq!(
        v["next"],
        "it is live at https://team-lunch.example; tell the room what changed"
    );
    let m: Value =
        serde_json::from_slice(&std::fs::read(c.dir.join(".lightbulb/migrations.json")).unwrap())
            .unwrap();
    assert!(!m["applied"].as_array().unwrap().is_empty(), "{m}");
}

#[test]
fn an_up_still_building_says_status_not_up_again() {
    let c = Checked::new();
    let (base, _) = building_stub(json!({"app_id": "a-1", "state": "provisioning"}));
    let (code, v) = c.app(&["up"], &base, true);
    assert_eq!(
        (code, &v["state"], &v["done"]),
        (0, &json!("provisioning"), &json!(false))
    );
    assert_eq!(
        v["next"],
        "it is still building; run lightbulb app status to see when it is done"
    );
}

#[test]
fn check_without_node_modules_says_install_and_counts_nothing() {
    let c = Checked::new();
    std::fs::remove_dir(c.dir.join("node_modules")).unwrap();
    for _ in 0..3 {
        let (code, v) = c.check("tsc.txt", 1, &[]);
        assert_eq!(code, 1);
        assert_eq!(
            v,
            json!({"code": "not_installed", "message": "The app's packages are not installed.",
                "next": "run pnpm install, then try again"})
        );
    }
    assert!(!c.dir.join(".lightbulb/progress.json").exists());
    assert!(!c.bin.path().join("args").exists(), "pnpm ran");
}

#[test]
fn an_unexpected_error_has_the_failure_shape() {
    let e = Env::new();
    let tmp = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let o = app_new(&e, tmp.path(), &["team-lunch"]);
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&o.stdout)));
    assert_eq!(
        (&v["code"], &v["next"]),
        (&json!("failed"), &json!("try again"))
    );
    assert!(!v["message"].as_str().unwrap().is_empty());
}

#[test]
fn no_embedded_starter_file_is_one_git_ignores() {
    use lightbulb::apps::starter::FILES;
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    let ignore = FILES.iter().find(|(p, _)| *p == ".gitignore").unwrap().1;
    std::fs::write(repo.path().join(".gitignore"), ignore).unwrap();
    // The addon's files are the app's paths.
    let paths: Vec<&str> = FILES
        .iter()
        .map(|(p, _)| p.strip_prefix("addons/content/").unwrap_or(p))
        .collect();
    let mut child = Command::new("git")
        .current_dir(repo.path())
        .args(["check-ignore", "--no-index", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // A sanity line that must come back, so an empty answer means none matched.
    let input = format!("{}\n.lightbulb/progress.json\n", paths.join("\n"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    assert_eq!(out, ".lightbulb/progress.json\n");
}

#[test]
fn publish_refuses_when_archive_had_to_commit() {
    let c = Checked::new();
    std::fs::write(c.dir.join(".lightbulb/app.json"), r#"{"app_id":"a-1"}"#).unwrap();
    std::fs::write(
        c.dir.join(".lightbulb/progress.json"),
        json!({"evidence": [{"kind": "review", "commit": c.head(), "verdict": "pass"}]})
            .to_string(),
    )
    .unwrap();
    // A first-line change: porcelain starts with a space (" M app.yaml").
    let yaml = std::fs::read_to_string(c.dir.join("app.yaml")).unwrap();
    std::fs::write(c.dir.join("app.yaml"), format!("{yaml}\n")).unwrap();
    let (base, seen) = ship_stub(json!({"app_id": "a-1", "state": "ready"}));
    let (code, v) = c.app(&["publish"], &base, true);
    assert_eq!((code, &v["code"]), (1, &json!("review_required")));
    assert!(lines(&seen).is_empty());
    assert!(git(&c.dir, &["log", "-1", "--format=%s"]).contains("app.yaml"));
}

#[test]
fn an_untracked_env_file_is_refused_not_committed() {
    let c = Checked::new();
    std::fs::write(c.dir.join("app.yaml.keep"), "x").unwrap();
    std::fs::create_dir_all(c.dir.join("sub")).unwrap();
    std::fs::write(c.dir.join("sub/.env.prod"), "SECRET=1").unwrap();
    std::fs::create_dir_all(c.dir.join("node_modules")).unwrap();
    std::fs::write(c.dir.join("node_modules/m.js"), "x").unwrap();
    // The starter ignores these; a user's .gitignore may not.
    std::fs::write(c.dir.join(".gitignore"), "").unwrap();
    let before = c.head();
    let e = lightbulb::apps::archive(&c.dir).unwrap_err();
    assert_eq!(e.code, "secret_file");
    assert!(e.message.contains("sub/.env.prod"));
    assert_eq!(e.next, "add them to .gitignore");
    assert_eq!(c.head(), before);
    // Build output alone is unstaged, not an error.
    std::fs::remove_file(c.dir.join("sub/.env.prod")).unwrap();
    let (tgz, _) = lightbulb::apps::archive(&c.dir).unwrap();
    assert!(!git(&c.dir, &["ls-files"]).contains("node_modules"));
    std::fs::remove_file(tgz).unwrap();
}

#[test]
fn status_prints_contract_fields_only() {
    let c = Checked::new();
    std::fs::write(c.dir.join(".lightbulb/app.json"), r#"{"app_id":"a-1"}"#).unwrap();
    let (base, _) = stub_with(
        |line| {
            if line.contains("/errors") {
                (
                    200,
                    json!({"client": [{"at": "t", "message": "boom", "stack": "SECRET"}], "server": []}),
                )
            } else {
                (
                    200,
                    json!({"app_id": "a-1", "state": "ready", "live_version": 2,
                    "DATABASE_URL": "postgres://u:SECRET@h/db",
                    "versions": [{"n": 1, "commit": "c", "token": "SECRET"}],
                    "resources": [{"provider": "neon", "service": "pg", "state": "ready", "url": "SECRET"}]}),
                )
            }
        },
        0,
    );
    let (code, v) = c.app(&["status", "--errors"], &base, true);
    assert_eq!(code, 0);
    assert_eq!(v["status"]["versions"][0], json!({"n": 1, "commit": "c"}));
    assert_eq!(
        v["errors"]["client"][0],
        json!({"at": "t", "message": "boom"})
    );
}

/// A backend whose status is `status()` and whose preview (itself) answers health with `health`.
fn gate_stub(
    up: Value,
    status: impl Fn(usize) -> Value + Send + 'static,
    health: Value,
) -> (String, Arc<Mutex<Vec<Req>>>) {
    let polls = std::sync::atomic::AtomicUsize::new(0);
    stub_with(
        move |line| {
            let route = line.trim_end_matches(" HTTP/1.1");
            if route == "GET /api/_lightbulb/health" {
                return (200, health.clone());
            }
            match route.strip_prefix(&format!("POST {BUILD}")).unwrap_or("") {
                "/source" => return (200, json!({"app_id": "a-1", "source_id": "s-1"})),
                "/up" => return (200, up.clone()),
                _ => {}
            }
            if route == format!("GET {BUILD}/a-1/status") {
                let n = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return (200, status(n));
            }
            (
                404,
                json!({"code": "no_route", "message": route, "next": ""}),
            )
        },
        0,
    )
}

fn ready_status(base: &str, commit: &str, ready_at: &str) -> Value {
    json!({"app_id": "a-1", "state": "ready", "preview_url": base,
        "preview": {"deployment_id": "d-1", "commit": commit, "ready_at": ready_at}})
}

impl Checked {
    fn shot(&self) -> std::path::PathBuf {
        let p = self.bin.path().join("shot.png");
        std::fs::write(&p, b"png bytes").unwrap();
        p
    }

    fn evidence(&self, base: &str, extra: &[&str]) -> (i32, Value) {
        self.evidence_as(base, "browser", extra)
    }

    fn evidence_as(&self, base: &str, kind: &str, extra: &[&str]) -> (i32, Value) {
        let shot = self.shot();
        let mut a = vec!["evidence", "--kind", kind, "--file", shot.to_str().unwrap()];
        a.extend(extra);
        self.app(&a, base, true)
    }
}

#[test]
fn done_needs_ready_health_and_browser_evidence_for_head() {
    let c = Checked::new();
    let head = c.head();
    let (base, seen) = {
        let (h1, h2) = (head.clone(), head.clone());
        let cell = Arc::new(Mutex::new(String::new()));
        let c2 = cell.clone();
        let (b, s) = gate_stub(
            json!({"app_id": "a-1", "state": "ready"}),
            move |_| ready_status(&c2.lock().unwrap(), &h1, "2020-01-01T00:00:00Z"),
            json!({"commit": h2}),
        );
        *cell.lock().unwrap() = b.clone();
        (b, s)
    };
    // Ready, healthy, but nobody has looked at it.
    let (code, v) = c.app(&["up"], &base, true);
    assert_eq!((code, &v["done"]), (0, &json!(false)), "{v}");
    assert_eq!(v["missing"], json!(["browser_evidence"]));
    let want = format!(
        "open {base} in the browser, check the main flow, then lightbulb app evidence --kind browser --file …"
    );
    assert_eq!(v["next"], want.as_str());
    assert!(lines(&seen).contains(&"GET /api/_lightbulb/health".to_string()));
    {
        let seen = seen.lock().unwrap();
        let health = seen.iter().find(|r| r.line.contains("/health")).unwrap();
        assert!(!health.headers.contains_key("authorization"));
    }

    // Record it.
    let (code, v) = c.evidence(&base, &["--note", "main flow ok"]);
    assert_eq!(code, 0, "{v}");
    let ev = &c.progress_file()["evidence"][0];
    assert_eq!(ev["kind"], "browser");
    assert_eq!(ev["verdict"], "pass");
    assert_eq!(ev["commit"], head.as_str());
    assert_eq!(ev["deployment_id"], "d-1");
    assert_eq!(ev["sha256"], sha(b"png bytes").as_str());
    assert_eq!(ev["note"], "main flow ok");
    assert!(ev["file"].as_str().unwrap().ends_with("shot.png"));
    assert!(ev["at"].as_str().unwrap().ends_with('Z'));
    // Now it is done, and a review record from the same command lets publish through.
    let (code, v) = c.app(&["up"], &base, true);
    assert_eq!(
        (code, &v["done"], &v["missing"]),
        (0, &json!(true), &json!([])),
        "{v}"
    );
    let (code, v) = c.evidence_as(&base, "review", &[]);
    assert_eq!((code, &v["code"]), (1, &json!("verdict_required")));
    let (code, _) = c.evidence_as(&base, "review", &["--verdict", "pass"]);
    assert_eq!(code, 0);
    let p = c.progress_file();
    assert!(lightbulb::apps::has_passing_review(&c.dir, &head), "{p}");
}

#[test]
fn evidence_for_an_old_commit_does_not_satisfy_the_gate() {
    let c = Checked::new();
    let head = c.head();
    let make = |ready_at: &'static str| {
        let cell = Arc::new(Mutex::new(String::new()));
        let c2 = cell.clone();
        let h = head.clone();
        let (b, _) = gate_stub(
            json!({"app_id": "a-1", "state": "ready"}),
            move |_| ready_status(&c2.lock().unwrap(), &h, ready_at),
            json!({"commit": head}),
        );
        *cell.lock().unwrap() = b.clone();
        b
    };
    let base = make("2020-01-01T00:00:00Z");
    let put = |e: Value| {
        std::fs::write(
            c.dir.join(".lightbulb/progress.json"),
            json!({"evidence": [e]}).to_string(),
        )
        .unwrap()
    };
    // Old commit, failing verdict, wrong kind, and recorded before the deployment was ready.
    for e in [
        json!({"kind": "browser", "verdict": "pass", "commit": "old", "at": "2030-01-01T00:00:00Z"}),
        json!({"kind": "browser", "verdict": "fail", "commit": head, "at": "2030-01-01T00:00:00Z"}),
        json!({"kind": "review", "verdict": "pass", "commit": head, "at": "2030-01-01T00:00:00Z"}),
        json!({"kind": "browser", "verdict": "pass", "commit": head, "at": "2019-12-31T23:59:59Z"}),
    ] {
        put(e.clone());
        let (_, v) = c.app(&["up"], &base, true);
        assert_eq!(v["done"], false, "{e}");
        assert_eq!(v["missing"], json!(["browser_evidence"]), "{e}");
    }
    put(
        json!({"kind": "browser", "verdict": "pass", "commit": head, "at": "2020-01-01T00:00:00Z"}),
    );
    let (_, v) = c.app(&["up"], &base, true);
    assert_eq!(v["done"], true, "{v}");
    // A preview for another commit is not done, whatever the evidence.
    let cell = Arc::new(Mutex::new(String::new()));
    let c2 = cell.clone();
    let (b, _) = gate_stub(
        json!({"app_id": "a-1", "state": "ready"}),
        move |_| ready_status(&c2.lock().unwrap(), "older", "2020-01-01T00:00:00Z"),
        json!({"commit": "older"}),
    );
    *cell.lock().unwrap() = b.clone();
    let (_, v) = c.app(&["up"], &b, true);
    assert_eq!(
        (&v["done"], &v["missing"]),
        (&json!(false), &json!(["preview_commit"]))
    );
    assert!(v["next"].as_str().unwrap().contains("lightbulb app up"));
}

#[test]
fn health_commit_mismatch_is_not_done() {
    let c = Checked::new();
    let head = c.head();
    std::fs::write(
        c.dir.join(".lightbulb/progress.json"),
        json!({"evidence": [{"kind": "browser", "verdict": "pass", "commit": head, "at": "2030-01-01T00:00:00Z"}]}).to_string(),
    )
    .unwrap();
    let cell = Arc::new(Mutex::new(String::new()));
    let c2 = cell.clone();
    let h = head.clone();
    let (base, _) = gate_stub(
        json!({"app_id": "a-1", "state": "ready"}),
        move |_| ready_status(&c2.lock().unwrap(), &h, "2020-01-01T00:00:00Z"),
        json!({"commit": "somethingelse"}),
    );
    *cell.lock().unwrap() = base.clone();
    let (code, v) = c.app(&["up"], &base, true);
    assert_eq!(
        (code, &v["done"], &v["missing"]),
        (0, &json!(false), &json!(["health"])),
        "{v}"
    );
    let next = v["next"].as_str().unwrap();
    assert!(next.contains("/api/_lightbulb/health"), "{next}");
    assert!(next.contains("lightbulb app status --errors") && !next.contains("app up"));
}

#[test]
fn next_names_the_missing_piece() {
    let c = Checked::new();
    let head = c.head();
    let head2 = head.clone();
    // Building twice, then ready: up waits, with no preview reported at all.
    let (base, seen) = gate_stub(
        json!({"app_id": "a-1", "state": "building"}),
        |n| {
            json!({"app_id": "a-1", "state": if n < 2 { "building" } else { "ready" },
                "preview_url": "http://127.0.0.1:1"})
        },
        json!({"commit": "x"}),
    );
    let (code, v) = c.app(&["up"], &base, true);
    assert_eq!(
        (code, &v["state"], &v["done"]),
        (0, &json!("ready"), &json!(false)),
        "{v}"
    );
    assert_eq!(v["missing"], json!(["preview"]));
    assert_eq!(
        v["next"],
        "the backend did not report the preview deployment"
    );
    let status_gets = lines(&seen)
        .iter()
        .filter(|l| l.ends_with("/a-1/status"))
        .count();
    assert_eq!(status_gets, 4, "{:?}", lines(&seen));
    // Failed stops the wait.
    let (base, seen) = gate_stub(
        json!({"app_id": "a-1", "state": "building"}),
        |_| json!({"app_id": "a-1", "state": "failed"}),
        json!({}),
    );
    let (code, v) = c.app(&["up"], &base, true);
    assert_eq!((code, &v["code"]), (1, &json!("build_failed")), "{v}");
    assert_eq!(
        v["next"],
        "run lightbulb app status --errors to see why the build failed"
    );
    assert_eq!(
        lines(&seen)
            .iter()
            .filter(|l| l.ends_with("/status"))
            .count(),
        1
    );
    // Ready with no ready_at: the time up first saw ready stands in, and is kept.
    let cell = Arc::new(Mutex::new(String::new()));
    let c2 = cell.clone();
    let (base, _) = gate_stub(
        json!({"app_id": "a-1", "state": "ready"}),
        move |_| {
            json!({"app_id": "a-1", "state": "ready", "preview_url": *c2.lock().unwrap(),
                "preview": {"deployment_id": "d-1", "commit": head2}})
        },
        json!({"commit": head}),
    );
    *cell.lock().unwrap() = base.clone();
    let (_, v) = c.app(&["up"], &base, true);
    assert_eq!(v["missing"], json!(["browser_evidence"]), "{v}");
    let seen_at = c.progress_file()["preview_seen"].clone();
    assert_eq!(seen_at["commit"], c.head().as_str());
    let (code, _) = c.evidence(&base, &[]);
    assert_eq!(code, 0);
    let (_, v) = c.app(&["up"], &base, true);
    assert_eq!(v["done"], true, "{v}");
    assert_eq!(c.progress_file()["preview_seen"], seen_at);
}

#[test]
fn evidence_needs_a_real_file_and_works_offline() {
    let c = Checked::new();
    let (code, v) = c.app(
        &[
            "evidence",
            "--kind",
            "browser",
            "--file",
            "/no/such/file.png",
        ],
        "http://127.0.0.1:1",
        true,
    );
    assert_eq!((code, &v["code"]), (1, &json!("bad_file")));
    // The backend is down: the record is still made, with no deployment.
    let (code, v) = c.evidence("http://127.0.0.1:1", &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        c.progress_file()["evidence"][0]["deployment_id"],
        Value::Null
    );
}

#[test]
fn a_bad_preview_address_is_never_fetched() {
    let c = Checked::new();
    let head = c.head();
    for bad in ["file:///etc/passwd", "http://x\noutput = \"/tmp/pwn\""] {
        let h = head.clone();
        let (base, seen) = gate_stub(
            json!({"app_id": "a-1", "state": "ready"}),
            move |_| ready_status(bad, &h, "2020-01-01T00:00:00Z"),
            json!({"commit": "x"}),
        );
        let (code, v) = c.app(&["up"], &base, true);
        assert_eq!(
            (code, &v["done"], &v["missing"]),
            (0, &json!(false), &json!(["preview"])),
            "{bad}"
        );
        assert!(!lines(&seen).iter().any(|l| l.contains("health")));
    }
    assert!(!std::path::Path::new("/tmp/pwn").exists());
}

#[test]
fn evidence_inside_the_app_must_be_ignored_by_git() {
    let c = Checked::new();
    let (base, _) = (String::from("http://127.0.0.1:1"), ());
    let loose = c.dir.join("shot.png");
    std::fs::write(&loose, b"x").unwrap();
    let (code, v) = c.app(
        &[
            "evidence",
            "--kind",
            "browser",
            "--file",
            loose.to_str().unwrap(),
        ],
        &base,
        true,
    );
    assert_eq!((code, &v["code"]), (1, &json!("file_in_repo")));
    assert!(v["next"].as_str().unwrap().contains(".lightbulb/evidence/"));
    let ok = c.dir.join(".lightbulb/evidence/shot.png");
    std::fs::create_dir_all(ok.parent().unwrap()).unwrap();
    std::fs::write(&ok, b"x").unwrap();
    let (code, v) = c.app(
        &[
            "evidence",
            "--kind",
            "browser",
            "--file",
            ok.to_str().unwrap(),
        ],
        &base,
        true,
    );
    assert_eq!(code, 0, "{v}");
}

#[test]
fn status_prints_done_and_a_failed_wait_says_where_to_look() {
    let c = Checked::new();
    let head = c.head();
    let cell = Arc::new(Mutex::new(String::new()));
    let c2 = cell.clone();
    let (base, _) = gate_stub(
        json!({"app_id": "a-1", "state": "ready"}),
        move |_| ready_status(&c2.lock().unwrap(), &head, "2020-01-01T00:00:00Z"),
        json!({"commit": c.head()}),
    );
    *cell.lock().unwrap() = base.clone();
    c.app(&["up"], &base, true);
    let (code, v) = c.app(&["status"], &base, true);
    assert_eq!(
        (code, &v["done"], &v["missing"]),
        (0, &json!(false), &json!(["browser_evidence"])),
        "{v}"
    );
    assert!(v["next"]
        .as_str()
        .unwrap()
        .contains("evidence --kind browser"));
    assert_eq!(v["state"], "ready");
    let (_, v) = c.evidence(&base, &[]);
    assert_eq!(v["deployment_id"], "d-1");
    let (_, v) = c.app(&["status"], &base, true);
    assert_eq!(v["done"], true, "{v}");

    let (base, _) = gate_stub(
        json!({"app_id": "a-1", "state": "building"}),
        |_| json!({"app_id": "a-1", "state": "failed"}),
        json!({}),
    );
    let (_, v) = c.app(&["up"], &base, true);
    assert_eq!(
        v["next"],
        "run lightbulb app status --errors to see why the build failed"
    );
}

#[test]
fn a_preview_for_another_commit_is_not_named_in_evidence() {
    let c = Checked::new();
    let cell = Arc::new(Mutex::new(String::new()));
    let c2 = cell.clone();
    let (base, _) = gate_stub(
        json!({"app_id": "a-1", "state": "ready"}),
        move |_| ready_status(&c2.lock().unwrap(), "older", "2020-01-01T00:00:00Z"),
        json!({"commit": "older"}),
    );
    *cell.lock().unwrap() = base.clone();
    c.app(&["up"], &base, true);
    let (code, v) = c.evidence(&base, &[]);
    assert_eq!((code, &v["deployment_id"]), (0, &Value::Null));
}

#[test]
fn known_fix_is_printed_as_hint() {
    let c = Checked::new();
    std::fs::write(
        c.dir.join(".lightbulb/known-fixes.md"),
        "# Known fixes\n\n- **Cannot find name 'sql'.** Import it from drizzle-orm.\n  Then rerun.\n- **Unrelated thing.** nope\n",
    )
    .unwrap();
    std::fs::write(
        c.dir.join(".lightbulb/lessons.md"),
        "# Lessons\n\n## TS2322 wrong prop type\nCast at the boundary.\n\n## Other\nx\n",
    )
    .unwrap();
    let (_, v) = c.check("tsc.txt", 1, &[]);
    let p = v["problems"].as_array().unwrap();
    assert!(
        p[0]["hint"]
            .as_str()
            .unwrap()
            .contains("Cast at the boundary"),
        "{v}"
    );
    assert!(!p[0]["hint"].as_str().unwrap().contains("Other"), "{v}");
    assert!(
        p[1]["hint"].as_str().unwrap().contains("Then rerun."),
        "{v}"
    );
    assert!(!p[1]["hint"].as_str().unwrap().contains("nope"), "{v}");
    // No files: no hint, no error.
    std::fs::remove_file(c.dir.join(".lightbulb/known-fixes.md")).unwrap();
    std::fs::remove_file(c.dir.join(".lightbulb/lessons.md")).unwrap();
    let (_, v) = c.check("tsc.txt", 1, &[]);
    assert!(v["problems"][0].get("hint").is_none(), "{v}");
}

#[test]
fn broken_tool_report_is_rate_limited_to_one_per_cycle() {
    let c = Checked::new();
    let report = |tool: &str, what: &str| {
        let o =
            c.e.base_cmd(Command::new(common::EXE))
                .current_dir(&c.dir)
                .args(["app", "report-broken-tool", tool, what, "--json"])
                .output()
                .unwrap();
        (
            o.status.code().unwrap(),
            serde_json::from_slice::<Value>(&o.stdout).unwrap(),
        )
    };
    let (code, _) = report("browser", "timed out\n## Fake heading");
    assert_eq!(code, 0);
    let lessons = std::fs::read_to_string(c.dir.join(".lightbulb/lessons.md")).unwrap();
    assert_eq!(lessons.matches("## Broken tools").count(), 1, "{lessons}");
    assert!(
        lessons.contains(" browser: timed out ## Fake heading\n"),
        "{lessons}"
    );
    assert!(!lessons.contains("\n## Fake"), "{lessons}");
    let (code, v) = report("browser", "again");
    assert_eq!(code, 1);
    assert_eq!(v["code"], "already_reported");
    assert_eq!(
        v["next"],
        "run lightbulb app up first; one report per cycle"
    );
    // A new cycle (up increments up_cycle) allows another.
    let mut p = c.progress();
    p["up_cycle"] = 1.into();
    std::fs::write(c.dir.join(".lightbulb/progress.json"), p.to_string()).unwrap();
    assert_eq!(report("browser", "again").0, 0);
    let lessons = std::fs::read_to_string(c.dir.join(".lightbulb/lessons.md")).unwrap();
    assert_eq!(lessons.matches("## Broken tools").count(), 1, "{lessons}");
}

#[test]
fn shipped_known_fixes_give_a_hint_and_short_or_broken_tools_titles_do_not() {
    let c = Checked::new();
    let (_, v) = c.check("server-only.txt", 1, &[]);
    let hint = v["problems"][0]["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("Load the data in a server component"), "{v}");
    let lessons = std::fs::read_to_string(c.dir.join(".lightbulb/lessons.md")).unwrap();
    assert!(lessons.contains("## Broken tools"), "{lessons}");
    std::fs::remove_file(c.dir.join(".lightbulb/known-fixes.md")).unwrap();
    // "ver" occurs in "server" but is under 4 characters.
    std::fs::write(
        c.dir.join(".lightbulb/lessons.md"),
        format!("{lessons}\n## ver\nshort title body\n"),
    )
    .unwrap();
    let (_, v) = c.check("server-only.txt", 1, &[]);
    assert!(v["problems"][0].get("hint").is_none(), "{v}");
    // A populated Broken tools section whose heading occurs in the message is not a hint.
    std::fs::write(
        c.dir.join(".lightbulb/lessons.md"),
        "# Lessons\n\n## Broken tools\n- 2026-10-30 browser: timed out\n",
    )
    .unwrap();
    let (_, v) = c.check("broken-tools.txt", 1, &[]);
    assert!(v["problems"][0].get("hint").is_none(), "{v}");
}
