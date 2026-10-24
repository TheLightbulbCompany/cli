//! Sign-in proofs: `signin` refuses (no issuer keys yet), a person's second computer admitted
//! by their first, and the workspace member admitted with a workspace proof.

mod common;

use common::issuer::{computer_proof, workspace_proof};
use common::{
    call, computer, dialog, err, invite, members, out, peers, person, seed_pair, until, Env, EXE,
};
use lightbulb::daemon::ipc;
use lightbulb::embed::Instance;
use serde_json::{json, Value};
use std::path::Path;

fn identity_of(e: &Env) -> Value {
    e.ask(json!({"id": 1, "op": "identity.get"}))
}

/// `e` signs in as `sub` with a proof minted here (`identity.proof`).
fn sign_in(e: &Env, sub: &str, ws: Option<(&str, &str)>) {
    let token = computer_proof(&e.identity().key, sub, "Prasiddha", ws, ipc::now() as i64);
    let r = person(e, json!({"id": 1, "op": "identity.proof", "proof": token}));
    assert_eq!(r["ok"], true, "{r}");
}

/// A fake `osascript` that notes each dialog's text in `dir/texts` and answers `Allow`.
fn allowing(dir: &Path) -> (&'static str, String) {
    let d = dir.display();
    let p = common::script(
        dir,
        &format!(
            "while [ \"$1\" != -- ]; do shift; done\nprintf '%s\\n' \"$3\" >>{d}/texts\n\
             echo 'button returned:Allow, gave up:false'"
        ),
    );
    ("LIGHTBULB_CONNECT_DEBUG_OSASCRIPT", p.display().to_string())
}

fn texts(dir: &Path) -> Vec<String> {
    let t = std::fs::read_to_string(dir.join("texts")).unwrap_or_default();
    t.lines().map(String::from).collect()
}

fn member<'a>(ms: &'a [Value], key: &str) -> &'a Value {
    ms.iter()
        .find(|m| m["key"] == key)
        .unwrap_or_else(|| panic!("{ms:?}"))
}

#[test]
fn signin_says_verified_names_are_not_available_yet() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let o = e.detached(&[EXE, "signin"], "");
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(err(&o), "Verified names are not available yet.\n");
    let id = identity_of(&e);
    assert_eq!(id["verified"], false, "{id}");
    // Hidden from the top-level help.
    let o = e.detached(&[EXE, "--help"], "");
    assert!(!out(&o).contains("signin"), "{}", out(&o));
    assert!(out(&o).contains("\n  relay     "), "{}", out(&o));
}

#[test]
fn a_proof_for_another_computer_or_person_is_refused() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let other = common::identity();
    let r = person(
        &e,
        json!({"id": 1, "op": "identity.proof",
        "proof": computer_proof(&other.key, "user_1", "P", None, ipc::now() as i64)}),
    );
    assert_eq!(r["error"], "bad_request", "{r}");
    // A person is never a `ws:` id (it would make this computer the workspace's).
    let ws = computer_proof(&e.identity().key, "ws:W1", "P", None, ipc::now() as i64);
    let r = person(&e, json!({"id": 1, "op": "identity.proof", "proof": ws}));
    assert_eq!(r["error"], "bad_request", "{r}");
    sign_in(&e, "user_1", None);
    let r = person(
        &e,
        json!({"id": 1, "op": "identity.proof",
        "proof": computer_proof(&e.identity().key, "user_2", "P", None, ipc::now() as i64)}),
    );
    assert_eq!(
        (&r["error"], &r["reason"]),
        (&json!("bad_request"), &json!("other_person")),
        "{r}"
    );
    assert_eq!(identity_of(&e)["person_id"], "user_1");
}

#[test]
fn an_agent_cannot_sign_this_computer_in() {
    // The daemon takes this test process for an agent.
    let e = Env::new();
    let me = std::process::id();
    let birth = lightbulb::os::process(me).unwrap().birth_micros;
    let pin = ("LIGHTBULB_CONNECT_HOOK_CALLER", format!("{me}:{birth}"));
    e.daemon(false, "", &[pin]);
    let token = computer_proof(
        &e.identity().key,
        "user_evil",
        "Mallory",
        None,
        ipc::now() as i64,
    );
    let r = e.ask(json!({"id": 1, "op": "identity.proof", "proof": token}));
    assert_eq!(
        (&r["error"], &r["reason"]),
        (&json!("bad_request"), &json!("not_a_person")),
        "{r}"
    );
    assert_eq!(identity_of(&e)["person_id"], e.identity().key);
}

#[test]
fn second_computer_of_same_person_is_admitted_by_the_first() {
    // F founds; A is bob's first computer (signed in as user_bob); B is bob's new Mac.
    let (f, a, b, c) = (Env::new(), Env::new(), Env::new(), Env::new());
    seed_pair(&f, &a);
    let dir = tempfile::tempdir().unwrap();
    a.daemon(false, "", &[allowing(dir.path())]);
    // F's own person would decline: only A may answer.
    f.daemon(false, "", &[peers(&[&a]), dialog("Decline")]);
    until(10, "F and A online", || f.status()["peers"]["online"] == 1);
    sign_in(&a, "user_bob", None);
    let t = invite(&f, "reusable");
    b.daemon(false, &computer("mbp"), &[peers(&[&f])]);
    sign_in(&b, "user_bob", None);
    // `bob` is A's name: the same person's computer may go by it.
    let o = b.detached(&[EXE, "join", &t, "--name", "bob"], "");
    assert!(o.status.success(), "{}{}", out(&o), err(&o));
    assert_eq!(
        texts(dir.path()),
        [
            "Your new Mac mbp wants to join. Allow?",
            "Or run: lightbulb approve J1"
        ]
    );
    let ms = members(&f);
    let mb = member(&ms, &b.identity().key);
    assert_eq!(
        (&mb["person_id"], &mb["name"]),
        (&json!("user_bob"), &json!("bob"))
    );
    assert_eq!(mb["proof"]["sub"], "user_bob");
    // Both of bob's computers offline: a third waits for one of them.
    for e in [&a, &b] {
        e.ask(json!({"id": 1, "op": "shutdown"}));
    }
    until(10, "F sees them gone", || {
        f.status()["peers"]["online"] == 0
    });
    c.daemon(false, &computer("air"), &[peers(&[&f])]);
    sign_in(&c, "user_bob", None);
    let o = c.detached(&[EXE, "join", &t, "--name", "bob"], "");
    assert!(!o.status.success());
    assert_eq!(
        err(&o),
        "Open your other computer, then run the same command again.\n"
    );
    assert_eq!(members(&f).len(), 3);
}

const WS: &str = "The Lightbulb Company";

/// F founds a solo network whose dialogs `allowing` answers; its link. The workspace member is
/// an embedded instance (only one joins as `cloud`).
fn founder() -> (Env, tempfile::TempDir, String) {
    let f = Env::new();
    let dir = tempfile::tempdir().unwrap();
    f.daemon(false, &computer("mac"), &[allowing(dir.path())]);
    let t = invite(&f, "reusable");
    (f, dir, t)
}

/// An instance of workspace `WS` on `home`, dialing `f`, with `knobs`.
async fn workspace(home: &Path, f: &Env, knobs: &[(&str, String)]) -> (Instance, String) {
    let i = common::instance(home, &[f], WS, None, knobs).await.unwrap();
    let me = call(&i, "identity.get", json!({})).await;
    let key = me["member_key"].as_str().unwrap().to_string();
    (i, key)
}

/// `i` joins link `t` as the workspace member with `proof`.
async fn join_cloud(i: &Instance, t: &str, proof: &str) -> Value {
    let body = json!({"ticket": t, "name": WS, "kind": "cloud", "proof": proof});
    let r = call(i, "team.join", body).await;
    assert_eq!(r["ok"], true, "{r}");
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_member_joins_with_a_workspace_proof() {
    let (f, dir, t) = founder();
    let home = tempfile::tempdir().unwrap();
    let (w, wk) = workspace(home.path(), &f, &[]).await;
    let agents = ["Friday", "Engineering"];
    let proof = workspace_proof(&wk, "W1", WS, &agents, ipc::now() as i64);
    let r = join_cloud(&w, &t, &proof).await;
    assert_eq!(r["state"], "admitted", "{r}");
    assert_eq!(
        texts(dir.path()),
        [
            "Lightbulb workspace \"The Lightbulb Company\" wants to join as a member. Its agents (Friday, Engineering) will appear in your directory, and it will see this network's directory.",
            "Or run: lightbulb approve J1"
        ]
    );
    let ms = members(&f);
    let m = member(&ms, &wk);
    assert_eq!(
        (&m["person_id"], &m["name"], &m["kind"], &m["workspace"]),
        (
            &json!("ws:W1"),
            &json!("The Lightbulb Company"),
            &json!("cloud"),
            &json!("W1")
        )
    );
    w.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_member_for_one_workspace_is_refused() {
    let (f, _dir, t) = founder();
    // Another name, so only the one-member-per-`sub` rule can refuse the second.
    let proof = |k: &str, name: &str| workspace_proof(k, "W1", name, &[], ipc::now() as i64);
    let (h1, h2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (w, wk) = workspace(h1.path(), &f, &[]).await;
    let r = join_cloud(&w, &t, &proof(&wk, WS)).await;
    assert_eq!(r["state"], "admitted", "{r}");
    let (w2, wk2) = workspace(h2.path(), &f, &[]).await;
    let r = join_cloud(&w2, &t, &proof(&wk2, "Lightbulb, renamed")).await;
    assert_eq!(
        (&r["state"], &r["reason"]),
        (&json!("refused"), &json!("name_taken")),
        "{r}"
    );
    w.shutdown().await;
    w2.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn computer_proof_cannot_join_as_cloud() {
    let (f, dir, t) = founder();
    let home = tempfile::tempdir().unwrap();
    let (w, wk) = workspace(home.path(), &f, &[]).await;
    let ws = Some(("W1", "The Lightbulb Company"));
    let proof = computer_proof(&wk, "user_1", "Prasiddha", ws, ipc::now() as i64);
    let r = join_cloud(&w, &t, &proof).await;
    assert_eq!(
        (&r["state"], &r["reason"]),
        (&json!("refused"), &json!("unsupported")),
        "{r}"
    );
    assert!(texts(dir.path()).is_empty(), "the person was asked");
    assert_eq!(members(&f).len(), 1);
    w.shutdown().await;
}

#[test]
fn unlink_is_unsupported() {
    let e = Env::new();
    e.daemon(false, "", &[]);
    let r = person(&e, json!({"id": 1, "op": "team.unlink", "member": "x"}));
    assert_eq!(r["error"], "unsupported", "{r}");
}
