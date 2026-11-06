//! Candidate binding over existing governed admission, using real daemons and synthetic keys.
mod common;
use common::{computer, dialog, members, peers, person, until, Env};
use serde_json::json;

#[test]
fn bound_ticket_rejects_wrong_key_and_unbound_copy_then_admits_expected_member() {
    use lightbulb::{daemon::net_ops, roster, ticket};
    let (host, expected, intruder) = (Env::new(), Env::new(), Env::new());
    let id = host.identity();
    let now = common::now_s();
    let mut body = net_ops::genesis(&id, "host", "job-host", "job", now - 1).body;
    body["governance"] = json!("founder");
    let genesis = roster::Entry::signed(&id, vec![], roster::Kind::Admit, body, now - 1);
    let store = host.store();
    roster::accept(&store, &genesis, now).unwrap();
    let r = roster::replay(&roster::entries(&store).unwrap());
    let (bound, hashed) = ticket::mint_for(
        &id,
        &r,
        &expected.identity().key,
        now,
        (String::new(), vec![]),
        None,
    )
    .unwrap();
    // A hypothetical original unbound ticket, even correctly signed by the inviter,
    // must not match the signed roster link's intended key.
    let mut unbound = bound.clone();
    unbound.for_key = None;
    unbound.sig = id.sign(&serde_json::to_value(&unbound).unwrap());
    let mut altered = bound.clone();
    altered.for_key = Some(intruder.identity().key);
    assert!(!ticket::signed_by_inviter(&altered));
    let link = roster::Entry::signed(
        &id,
        vec![genesis.hash()],
        roster::Kind::Link,
        ticket::link_body(&bound, Some(&hashed)),
        now,
    );
    roster::accept(&store, &link, now).unwrap();
    store
        .put_link(
            &bound.ticket_id,
            &bound.secret,
            Some(&hashed),
            &bound.mode,
            bound.expires_at,
        )
        .unwrap();
    let r = roster::replay(&roster::entries(&store).unwrap());
    assert!(ticket::verify(&bound, &r).is_ok());
    assert!(ticket::verify(&unbound, &r).is_err());
    altered.sig = id.sign(&serde_json::to_value(&altered).unwrap());
    assert!(ticket::signed_by_inviter(&altered));
    assert!(
        ticket::verify(&altered, &r).is_err(),
        "a re-signed different key still mismatches the live link"
    );
    drop(store);
    host.daemon(false, &computer("job-host"), &[dialog("Allow")]);
    let encoded = ticket::encode(&bound);
    intruder.daemon(false, &computer("intruder"), &[peers(&[&host])]);
    let bad = person(
        &intruder,
        json!({"id":1,"op":"team.join","ticket":encoded,"name":"guest"}),
    );
    assert_eq!(bad["state"], "refused", "{bad}");
    assert_eq!(bad["reason"], "bad_ticket", "{bad}");
    assert_eq!(members(&host).len(), 1);
    expected.daemon(false, &computer("expected"), &[peers(&[&host])]);
    let bad_unbound = person(
        &expected,
        json!({"id":1,"op":"team.join","ticket":ticket::encode(&unbound),"name":"guest"}),
    );
    assert_eq!(bad_unbound["state"], "refused", "{bad_unbound}");
    assert_eq!(bad_unbound["reason"], "bad_ticket", "{bad_unbound}");
    let ok = person(
        &expected,
        json!({"id":1,"op":"team.join","ticket":encoded,"name":"guest"}),
    );
    assert_eq!(ok["state"], "admitted", "{ok}");
    until(5, "once link consumed", || {
        let r = roster::replay(&roster::entries(&host.store()).unwrap());
        !r.links.contains_key(&bound.ticket_id)
    });
    let after = roster::replay(&roster::entries(&host.store()).unwrap());
    assert!(
        ticket::verify(&bound, &after).is_err(),
        "used ticket verified again"
    );
}
