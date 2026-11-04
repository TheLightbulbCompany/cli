//! Workspace proofs: an embedded instance shows its own sign-in in its `hello`.

use lightbulb::identity::Identity;
use lightbulb::net::frames::Frame;
use lightbulb::proof;
use lightbulb::roster::{self, Entry, Kind};
use lightbulb::store::Store;
use serde_json::json;

mod common;

use common::{issuer, now_s};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_instance_presents_its_login_json_in_its_hello() {
    let home = tempfile::tempdir().unwrap();
    let w = Identity::load_or_create(home.path()).unwrap();
    let who = common::identity();
    let g = Entry::signed(
        &who,
        vec![],
        Kind::Admit,
        json!({"member": who.key, "name": "bob", "computer": "mac",
            "role": "founder", "ticket_id": "", "team_name": "acme"}),
        1,
    );
    let aw = common::admit(&who, &g, &w, "workspace", json!({}));
    let store = Store::open(&home.path().join("state.db")).unwrap();
    for e in [&g, &aw] {
        roster::accept(&store, e, 10).unwrap();
    }
    drop(store);
    let key = w.key.clone();
    let token = issuer::workspace_proof(&key, "org_1", now_s());
    let v = json!({"proof": token, "route_url": null, "name": "workspace"});
    std::fs::write(home.path().join("login.json"), v.to_string()).unwrap();
    let i = common::instance(home.path(), &[], "workspace", None, &[])
        .await
        .unwrap();
    let (mut peer, _ep) = common::dial_instance(&i, &who).await;
    let Frame::Hello { proof: shown, .. } =
        peer.recv_where(|f| matches!(f, Frame::Hello { .. })).await
    else {
        unreachable!()
    };
    let p = proof::verify(&shown.expect("a proof"), &key, now_s()).unwrap();
    assert_eq!(p.sub, "org_1");
    i.shutdown().await;
}
