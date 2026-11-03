//! Agent Cards: a member's signed card travels in the roster, the `card` op reads it, the
//! directory carries it, and an oversized one never replaces it.

use lightbulb::roster::{self, Entry, Kind};
use serde_json::json;

mod common;

use common::{seed_pair, until, Env};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_members_card_appears_in_the_roster_and_directory() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    let on = [("LIGHTBULB_CONNECT_DEBUG_NO_CARD", String::new())];
    b.daemon(false, "", &on);
    a.daemon(false, "", &[common::peers(&[&b]), on[0].clone()]);
    until(10, "both online", || {
        a.status()["peers"]["online"] == 1 && b.status()["peers"]["online"] == 1
    });
    let sb = b.short();
    until(15, "A holds B's card", || {
        a.ask(json!({"id": 1, "op": "card", "member": sb}))["card"]["supportedInterfaces"][0]["url"]
            == format!("lightbulb://{sb}").as_str()
    });
    // Default is the asker's own; B's card names B's roster name.
    let mine = a.ask(json!({"id": 1, "op": "card"}));
    assert_eq!(
        mine["card"]["supportedInterfaces"][0]["url"],
        format!("lightbulb://{}", a.short()).as_str()
    );
    assert_eq!(
        a.ask(json!({"id": 1, "op": "card", "member": "nobody"}))["ok"],
        false
    );
    // B's session, seen from A, carries the card.
    let pid = std::process::id();
    let birth = lightbulb::os::process(pid).unwrap().birth_micros;
    b.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "codex",
        "conversation": "b1", "pid": pid, "birth_micros": birth, "cwd": "/tmp/work/web"}}),
    );
    until(10, "A's directory shows B's session with its card", || {
        a.ask(json!({"id": 1, "op": "directory"}))["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| {
                r["session_id"].as_str().unwrap().starts_with(&sb)
                    && r["card"]["capabilities"]["extensions"][0]["required"] == true
            })
    });
}

#[test]
fn a_card_over_16_kib_is_rejected() {
    let (a, b) = (Env::new(), Env::new());
    let [g, admit] = seed_pair(&a, &b);
    let store = a.store();
    let ib = b.identity();
    let small = Entry::signed(
        &ib,
        vec![admit.hash()],
        Kind::Meta,
        json!({"card": {"name": "bob"}}),
        20,
    );
    roster::accept(&store, &small, 30).unwrap();
    let big = json!({"card": {"name": "x".repeat(roster::MAX_ENTRY_BYTES)}});
    let over = Entry::signed(&ib, vec![small.hash()], Kind::Meta, big, 21);
    assert!(roster::accept(&store, &over, 30).is_err());
    // Even replayed as if stored, it does not replace the card.
    let r = roster::replay(&[g, admit, small, over]);
    assert_eq!(r.cards.get(&ib.key).unwrap()["name"], "bob");
}
