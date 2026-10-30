//! `lightbulb app`: the subcommand, `app.yaml`, which credentials a call carries, and the
//! failure shape. The backend is a stub on loopback; nothing here reaches the real one.

mod common;

use common::Env;
use lightbulb::apps::api::{self, ApiError, Auth};
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
        help.contains("Apps you build:\n  app       Build, check, preview, publish and undo apps: app new|up|publish|rollback|status|destroy|check|evidence\n"),
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
    let o = run(&["app", "destroy"]);
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "not_implemented");
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
        let o = run(&["app", "evidence", "--kind", k, "--note", "ok"]);
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        assert_eq!(v["code"], "not_implemented", "{k}");
    }
    assert_eq!(
        run(&["app", "evidence", "--kind", "recording"])
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
        v["files"].as_u64().unwrap() as usize,
        lightbulb::apps::starter::FILES.len()
    );
    let dir = tmp.path().join("team-lunch");
    assert_eq!(
        std::fs::canonicalize(v["dir"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&dir).unwrap()
    );
    assert!(dir.join("src/app/%5Flightbulb").is_dir());
    let cfg = AppConfig::load(&dir).unwrap();
    assert_eq!(cfg.name, "team-lunch");
    assert_eq!(cfg.open_to, OpenTo::Workspace);
    assert_eq!(cfg.origin.kind, "ask");
    assert_eq!(cfg.services, ["vercel/project", "neon/postgres"]);
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
    assert_eq!(counts, [1, 1, 3, 3]);
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
    assert_eq!(
        v["problems"][0],
        json!({"file": "src/app/page.tsx", "line": 8, "column": null, "code": null,
        "message": "raw hex colour; use a token from design/tokens.css"})
    );
    assert_eq!(v["problems"].as_array().unwrap().len(), 2);
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
