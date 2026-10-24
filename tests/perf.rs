//! The performance harness: a team of `N` computers (one daemon each, a temp home each), found
//! over the loopback files as two daemons on one computer find each other, with `SESSIONS`
//! sessions across them. Prints join time, directory refresh, message latency, resident memory
//! and wall time. Ignored by default; run it alone:
//! `cargo test --test perf -- --ignored --nocapture`.

mod common;

use common::{computer, members, person, until, Env};
use lightbulb::{os, policy};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const N: usize = 20;
const SESSIONS: usize = 50;
/// Of `SESSIONS`, registered one at a time and timed until another computer lists each.
const TIMED: usize = 20;
const MESSAGES: usize = 100;

/// Every daemon of the run: shut down on drop, then killed if one is still there, on success
/// and on a panic alike.
struct Team(Vec<Env>);

impl Team {
    fn pids(&self) -> Vec<i32> {
        (self.0.iter())
            .filter_map(|e| std::fs::read_to_string(e.state.path().join("daemon.lock")).ok())
            .filter_map(|t| t.trim().parse().ok())
            .collect()
    }
}

impl Drop for Team {
    fn drop(&mut self) {
        let pids = self.pids();
        // Each `Env` asks its daemon to shut down.
        self.0.clear();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && pids.iter().any(|&p| unsafe { libc::kill(p, 0) } == 0) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for p in pids {
            unsafe { libc::kill(p, libc::SIGKILL) };
        }
    }
}

/// This test process as codex session `conv` on `e`; its key.
fn register(e: &Env, conv: &str) -> String {
    let pid = std::process::id();
    let birth = os::process(pid).unwrap().birth_micros;
    let r = e.ask(
        json!({"id": 1, "op": "register", "session": {"provider": "codex",
        "conversation": conv, "pid": pid, "birth_micros": birth, "cwd": "/tmp"}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    r["key"].as_str().unwrap().into()
}

/// What `who --all` lists on `e`: the session ids of its `directory`.
fn lists(e: &Env, key: &str) -> bool {
    let r = e.ask(json!({"id": 1, "op": "directory"}));
    (r["sessions"].as_array().into_iter().flatten()).any(|s| s["session_id"] == key)
}

/// Waits on `f` with 2 ms polls (finer than `until`'s 20 ms) for up to `secs`.
fn poll(secs: u64, what: &str, mut f: impl FnMut() -> bool) -> Duration {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
    t.elapsed()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `min / median / max`, or `median / p95`, of `v` in ms.
fn stats(v: &mut [f64]) -> (f64, f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    (v[0], at(0.5), at(0.95), v[v.len() - 1])
}

#[test]
#[ignore]
fn twenty_computers_fifty_sessions() {
    let wall = Instant::now();
    let team = Team((0..N).map(|_| Env::new()).collect());
    let envs = &team.0;
    // Every person allows every other before any daemon starts, so no message is held.
    let keys: Vec<String> = envs.iter().map(|e| e.identity().key).collect();
    for e in envs {
        let store = e.store();
        for k in &keys {
            policy::set_person(&store, k, policy::Verdict::Accept, "local", 1).unwrap();
        }
    }

    // `lan = true` with no PEERS: each daemon is found through its loopback file.
    for (i, e) in envs.iter().enumerate() {
        e.daemon(true, &computer(&format!("c{i}")), &[]);
    }
    let founder = &envs[0];
    let mut joins = vec![];
    for (i, e) in envs.iter().enumerate().skip(1) {
        let t = Instant::now();
        let r = person(
            founder,
            json!({"id": 1, "op": "team.invite", "mode": "open"}),
        );
        let ticket = r["ticket"].as_str().unwrap_or_else(|| panic!("{r}"));
        let r = person(
            e,
            json!({"id": 1, "op": "team.join", "ticket": ticket, "name": format!("p{i}"),
                "kind": "computer"}),
        );
        assert_eq!(r["state"], "admitted", "{r}");
        joins.push(ms(t.elapsed()));
    }
    let mesh = Instant::now();
    until(120, "every computer sees every other online", || {
        envs.iter()
            .all(|e| e.status()["peers"]["online"] == (N - 1) as u64)
    });
    let mesh = mesh.elapsed();
    assert!(envs.iter().all(|e| members(e).len() == N));

    // The baseline sessions, round-robin over the computers.
    let mut sessions: Vec<(usize, String)> = (0..SESSIONS - TIMED)
        .map(|s| (s % N, register(&envs[s % N], &format!("s{s}"))))
        .collect();
    until(60, "every computer lists every baseline session", || {
        let n = envs[N - 1].ask(json!({"id": 1, "op": "directory"}))["sessions"]
            .as_array()
            .map_or(0, Vec::len);
        n == sessions.len()
    });
    // Directory refresh: a new session on X until Y lists it.
    let mut refresh = vec![];
    for s in 0..TIMED {
        let (x, y) = (s % N, (s * 7 + 3) % N);
        let y = if y == x { (y + 1) % N } else { y };
        let t = Instant::now();
        let key = register(&envs[x], &format!("t{s}"));
        poll(30, "Y lists the new session", || lists(&envs[y], &key));
        refresh.push(ms(t.elapsed()));
        sessions.push((x, key));
    }
    // `who --all` itself, once: the CLI shows what the directory op returned.
    let o = envs[1].detached(&[common::EXE, "who", "--all"], "");
    assert!(o.status.success(), "{}", common::err(&o));
    let last = sessions.last().unwrap();
    let addr = &last.1;
    assert!(last.0 == 1 || common::out(&o).contains(addr.as_str()));

    // Message latency: send on X to a session on Y, Y's session takes it, X's receipt says
    // `delivered`. Each from a sender of its own that never receives: a session that answers
    // what it was sent is an agent chain, refused at `MAX_HOPS`.
    let shorts: Vec<String> = envs.iter().map(Env::short).collect();
    let (mut latency, mut stored, mut took) = (vec![], vec![], vec![]);
    for m in 0..MESSAGES {
        let x = &sessions[m % SESSIONS].0;
        let from = &format!("{}/codex:sender{m}", shorts[*x]);
        let (y, to) = (sessions.iter().cycle().skip(m * 13 + 1))
            .find(|(y, _)| y != x)
            .unwrap();
        let rid = format!("m{m}");
        let t = Instant::now();
        envs[*x].send(from, to, "ping", &rid, None);
        // Where the time goes: X hears `stored`, Y's session takes it, X hears `delivered`.
        let (mut at_stored, mut at_taken) = (None, None);
        let deadline = t + Duration::from_secs(30);
        loop {
            if at_taken.is_none() && !envs[*y].inbox(to).is_empty() {
                at_taken = Some(t.elapsed());
            }
            let r = envs[*x].receipt(from, &rid);
            if at_stored.is_none() && r["state"] != "queued" {
                at_stored = Some(t.elapsed());
            }
            if r["state"] == "delivered" {
                break;
            }
            assert!(Instant::now() < deadline, "message {m} {from} -> {to}: {r}");
            std::thread::sleep(Duration::from_millis(2));
        }
        latency.push(ms(t.elapsed()));
        stored.push(ms(at_stored.unwrap()));
        took.push(ms(at_taken.unwrap()));
    }

    let mut rss: Vec<f64> = (team.pids().iter())
        .map(|p| {
            let o = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &p.to_string()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .unwrap()
                / 1024.0
        })
        .collect();
    assert_eq!(rss.len(), N);

    let (jmin, jmed, _, jmax) = stats(&mut joins);
    let (_, rmed, rp95, _) = stats(&mut refresh);
    let (_, lmed, lp95, _) = stats(&mut latency);
    let (_, smed, sp95, _) = stats(&mut stored);
    let (_, tmed, tp95, _) = stats(&mut took);
    let (mmin, mmed, _, mmax) = stats(&mut rss);
    let report: Value = json!({"computers": N, "sessions": sessions.len()});
    println!("perf: {report}");
    println!(
        "perf: join (invite -> admitted) ms: min {jmin:.0} / median {jmed:.0} / max {jmax:.0}"
    );
    println!(
        "perf: full mesh online after the last join ms: {:.0}",
        ms(mesh)
    );
    println!("perf: directory refresh ms ({TIMED} samples): median {rmed:.0} / p95 {rp95:.0}");
    println!(
        "perf: message send -> delivered ms ({MESSAGES} messages): median {lmed:.0} / p95 {lp95:.0}"
    );
    println!("perf:   of which send -> stored ms: median {smed:.0} / p95 {sp95:.0}");
    println!("perf:   of which send -> taken on Y ms: median {tmed:.0} / p95 {tp95:.0}");
    println!(
        "perf: resident memory MB per daemon: min {mmin:.1} / median {mmed:.1} / max {mmax:.1}"
    );
    println!("perf: wall time s: {:.1}", wall.elapsed().as_secs_f64());
    assert!(wall.elapsed() < Duration::from_secs(300), "over 5 minutes");
}
