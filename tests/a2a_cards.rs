//! Agent Cards: a member's signed card travels in the roster, the `card` op reads it, the
//! directory carries it, and an oversized one never replaces it.

use lightbulb::roster::{self, Entry, Kind};
use serde_json::json;

mod common;

use common::{seed_pair, start_pair, until, Env};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_members_card_appears_in_the_roster_and_directory() {
    let (a, b) = (Env::new(), Env::new());
    seed_pair(&a, &b);
    start_pair(&a, &b, "", &[]);
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
    // The directory carries B's card once, by short, though B has no session.
    let d = a.ask(json!({"id": 1, "op": "directory"}));
    assert_eq!(
        d["cards"][&sb]["capabilities"]["extensions"][0]["required"],
        true
    );
    assert_eq!(d["cards"].as_object().unwrap().len(), 2);
    assert!(d["sessions"][0]["card"].is_null());
    // A member that is no string is a bad request.
    assert_eq!(
        a.ask(json!({"id": 1, "op": "card", "member": 7}))["error"],
        "bad_request"
    );
}

#[test]
fn a_joiner_publishes_its_card_and_a_restart_writes_no_second_entry() {
    let (a, b) = (Env::new(), Env::new());
    a.daemon(false, "", &[common::dialog("Allow")]);
    let t = common::invite(&a, "reusable");
    b.daemon(false, "", &[common::peers(&[&a])]);
    let r = common::person(
        &b,
        json!({"id": 1, "op": "team.join", "ticket": t, "name": "bob"}),
    );
    assert_eq!(r["state"], "admitted", "{r}");
    let (sa, sb) = (a.short(), b.short());
    until(10, "A holds B's card and B holds A's", || {
        a.ask(json!({"id": 1, "op": "card", "member": sb}))["ok"] == true
            && b.ask(json!({"id": 1, "op": "card", "member": sa}))["ok"] == true
    });
    // An unchanged card on restart is no new entry.
    let before = roster::entries(&b.store()).unwrap().len();
    {
        use std::io::{Read, Write};
        let mut s = std::os::unix::net::UnixStream::connect(b.state.path().join("sock")).unwrap();
        writeln!(s, r#"{{"id":1,"op":"shutdown"}}"#).unwrap();
        let _ = s.read_to_end(&mut vec![]);
        let lock = std::fs::File::open(b.state.path().join("daemon.lock")).unwrap();
        lock.lock().unwrap();
    }
    b.daemon(false, "", &[common::peers(&[&a])]);
    assert_eq!(b.ask(json!({"id": 1, "op": "card"}))["ok"], true);
    assert_eq!(roster::entries(&b.store()).unwrap().len(), before);
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
    let r = roster::replay(&[g.clone(), admit.clone(), small.clone(), over]);
    assert_eq!(r.cards.get(&ib.key).unwrap()["name"], "bob");
    // A card that is no object does not replace one either.
    let odd = Entry::signed(
        &ib,
        vec![small.hash()],
        Kind::Meta,
        json!({"card": "gone"}),
        22,
    );
    let r = roster::replay(&[g, admit, small, odd]);
    assert_eq!(r.cards.get(&ib.key).unwrap()["name"], "bob");
}
