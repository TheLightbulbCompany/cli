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
    let o = run(&["app", "status"]);
    assert_eq!(o.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["code"], "not_implemented");
    assert!(v["next"].is_string());
    assert!(!run(&["app", "up"]).stdout.is_empty());
}
