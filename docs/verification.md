# Verification

> Updated: message holds, Allow and per-person policy were removed (everyone on the network is trusted; an invite is the approval). Rows below that mention them are marked retired; the old test names no longer exist. map — lightbulb 0.3.7

> 0.4.0 (one member kind): the rows that existed only for cloud members, cloud markers, vouched
> person ids and sign-in proofs are marked REMOVED below, and the rows they touched point at the
> tests that replaced them. Everything else is the 0.3.7 record as it was.

This is the verify-gate record for 0.3.7 (main `d500099`). It maps each threat, each failure
mode, each parity row and each control added since 0.2.2 to the code that handles it and the
test that proves it. Every "Result" is from one `cargo test --no-fail-fast` run on 2026-10-24,
on the tree of commit `d500099` (see "Numbers"). Real-computer evidence is in "Real computers"
below; older real-Mac evidence is cited from `docs/acceptance-plan-a.md` and
`docs/acceptance-plan-b.md` by row (0.2.0 builds) and was not re-run here. File:line references
were re-pointed to this commit; a row that says NO TEST has none.

Notation: `src/x.rs:N` is the control. `tests/x.rs::name` is an integration test;
`src/x.rs::name` is a unit test in that file. **New** marks a test the 0.2.2 pass added; its "made it fail" note says how the control was
disabled in a scratch change to see the test fail before the control was restored (done for
0.2.2; not repeated for the 0.3.x tests below).

## A. Threat model

| Threat | Control | Tests | Result |
|---|---|---|---|
| **Forged frames.** A frame from a key that is not a member. | QUIC (TLS 1.3, raw public keys) authenticates the remote key; the daemon takes the member key from the connection (`src/net/mod.rs:685`) and closes any key not in the roster before a stream opens (`src/net/mod.rs:862`). Nothing a frame says about its own sender is trusted. | `tests/net_two.rs::non_member_is_refused_on_member_alpn` | PASS |
| Forged frames: a member's envelope that claims another member's address. | `from` must be under the sending member's own prefix, else dropped unanswered (`src/mail.rs:763`). | `tests/envelope.rs::envelope_from_another_members_prefix_is_dropped`, `tests/directory.rs::presence_naming_another_prefix_is_dropped` | PASS |
| Forged frames: a roster entry with a tampered body, or a `signer` that did not sign it, relayed by a real member. | Each entry carries its own Ed25519 signature, checked before it is stored (`src/roster.rs:766`). | **New** `tests/net_two.rs::tampered_or_misattributed_roster_entries_are_refused` (made it fail: `if false && !verify(…)` at `src/roster.rs:766` — the tampered entry is stored and B's heads never equal the good entry's). Also `src/identity.rs::sign_and_verify_ignore_the_sig_field`, `tests/join.rs::candidate_with_a_bad_signature_is_dropped`, `tests/streams.rs::grant_with_a_bad_signature_is_dropped` | PASS |
| **Wrong-caller hooks.** A hook run by a process that is not the agent it names. | The hook walks its own process ancestry for that agent (`src/agents/caller.rs:41`); not that agent → do nothing (`src/hook.rs:156`). | `src/agents/caller.rs::identify_refuses_a_hook_run_under_the_wrong_agent`, `tests/hooks.rs::a_claude_hook_fired_by_cursor_registers_nothing`; real Mac: plan A row 5 (a forged `hook grok session-start`) PASS | PASS |
| Wrong-caller hooks: a hook for a session that is not its own. | `register` refuses a key owned by another live process (`src/daemon/ipc.rs:498`); `end` only by the row's process or once that process is gone (`src/daemon/ipc.rs:416`); `inbox` only by the row's process (`src/daemon/ipc.rs:857`). | `tests/hooks.rs::register_cannot_overwrite_a_key_owned_by_another_live_process`, `…::session_end_pinned_to_another_live_process_does_not_end_the_row`, `…::a_hand_run_session_end_cannot_end_a_live_session`, `…::inbox_from_another_process_is_refused` | PASS |
| Wrong-caller: person-only ops (`remove`, `leave`, relay changes; `invite`, `join` and `approve` are open to agents, who record their session) from an agent's shell. | The socket peer's process tree must have no agent in it: `remove` at `src/daemon/net_ops.rs:87` (`gate`, `:61`), `leave` at `src/daemon/ipc.rs:213`/`:348`, relay changes at `src/daemon/net_ops.rs:246`; the tree walk is `person_only` (`src/daemon/net_ops.rs:861`). | `src/daemon/net_ops.rs::person_only_accepts_people_and_refuses_agents`, `tests/team_cli.rs::remove_and_leave_from_an_agent_shell_are_refused`, `tests/team_cli.rs::an_agent_shell_may_invite_in_every_mode`, `tests/join.rs::an_agent_may_join` | PASS |
| **Removed members** dialling in. | Not in the roster → closed at the gate; a removed one is first told its removal (`src/net/mod.rs:882`), then closed. Removal also closes a live link (`src/net/mod.rs` `refresh`) and forgets its addresses (`src/net/mod.rs:359`). | `tests/net_two.rs::removed_member_connection_is_closed_on_removal`, `…::removal_inside_the_gate_closes_the_connection`, `…::removed_member_is_told`, `…::a_removed_members_addresses_are_forgotten`; real Mac: plan B re-run rows 9, 15 (F6 fixed since) | PASS |
| Removed members replaying old heads. | A removed member is sent only the `remove` entry and the ancestors it lacks, never a later entry, whatever heads it claims (`src/net/mod.rs:1331`). | `tests/net_two.rs::removed_member_hears_its_removal_and_nothing_after` | PASS |
| Removed members using the old invite. | A removal ends every link it did not see (`src/roster.rs:603`, `retired` at `:619`); the inviter checks the link against the current roster (`src/admission.rs:367`). | `tests/join.rs::link_replaced_on_removal_no_longer_joins`, `src/roster.rs::link_concurrent_with_a_removal_is_ended`, `src/roster.rs::fork_removal_wins_and_drops_admissions_by_removed_signer` | PASS |
| **Replay** of an old envelope. | Each envelope id is kept per sender in `seen_ids`; a resend is re-acked with its current state and never stored twice (`src/mail.rs:783`); the same `request_id` with another body is `request_id_reused`. | `src/mail.rs::a_duplicate_envelope_is_reacked_with_its_reason_and_never_stored_twice`, `tests/envelope.rs::outbox_survives_restart_and_dedupes`, `tests/envelope.rs::wire_refusals_name_their_reason`, `tests/holds.rs::teammate_first_message_is_held_then_allow_delivers_and_writes_accept` (a resend makes no second hold) | PASS |
| Replay of an old receipt or ack. | Receipts only move forward (`src/mail.rs:103`); an answer counts only from the member the envelope went to (`src/outbox.rs:51`). | `tests/envelope.rs::late_duplicate_ack_never_regresses_a_receipt`, `…::held_receipt_advances_to_stored_on_allow`, `…::ack_from_a_third_member_is_ignored`, `src/outbox.rs::acks_move_a_receipt_forward_only_and_only_from_its_member` | PASS |
| **Loops**: agent↔agent ping-pong (per thread). | `MAX_HOPS` = 4: a person's message (hop 0) and 3 agent replies get through (`src/mail.rs:23`). The receiver sets the floor from its own record of the thread, whatever hops the peer claims (`src/mail.rs:811`); only a person at the terminal resets to 0. | `src/mail.rs::the_fourth_agent_reply_is_refused_as_hops`, `src/mail.rs::forwarding_without_reply_to_keeps_hop_ancestry`, `tests/envelope.rs::fourth_hop_and_thirteenth_send_are_refused_loop`, `tests/envelope.rs::a_person_at_the_terminal_resets_hops_to_zero` | PASS |
| Loops: per sender. | 12 messages per 600 s from one member (all of its sessions) to one session (`src/mail.rs`, `receive`). | `src/mail.rs::thirteenth_send_in_ten_minutes_is_refused_with_loop`, `tests/envelope.rs::a_named_session_of_any_member_is_delivered_and_its_loop_limit_is_the_members`, `tests/mcp_tools.rs::loop_refusals_name_their_limit_here_and_on_a_teammate`; real Mac: plan A row 3 | PASS |
| **Malicious teammate**: a body that forges the owner or peer header. | Every line that could pass for Lightbulb provenance gets a zero-width break after `[` (`src/frame.rs:26`); the header is built from the roster, not the body. | `src/frame.rs::a_forged_owner_line_in_the_body_is_neutralised`, `src/frame.rs::a_forged_frame_header_in_the_body_is_neutralised`, `src/frame.rs::a_hostile_request_id_is_escaped`, `tests/cli.rs::a_listened_body_cannot_forge_the_reply_line`, `tests/holds.rs::a_forged_prefix_behind_a_hidden_character_is_defanged`, `tests/mcp_tools.rs::check_inbox_defangs_a_teammate_name_and_verifies_this_computer`; real Mac: plan A row 4 | PASS |
| Malicious teammate: oversized bodies. | A frame over 1 MiB is refused before its body is read (`src/net/frames.rs:237`); a body over 64 KiB is `unsupported` (`src/mail.rs:768`); a roster entry over 16 KiB is refused (`src/roster.rs:20`); presence lists are capped. | `src/net/frames.rs::frames_round_trip_and_cap_at_one_mib`, `tests/envelope.rs::files_or_oversize_body_are_refused_unsupported`, `src/roster.rs::oversize_entry_is_refused`, `tests/streams.rs::chunk_over_64k_is_bad_request` | PASS |
| Malicious teammate: attachments or a kind outside the allowed one. | Only `kind = "message"` with no `files` is accepted (`src/mail.rs:768`). | `tests/envelope.rs::input_kind_is_refused_unsupported`, `tests/envelope.rs::files_or_oversize_body_are_refused_unsupported` | PASS |
| Malicious teammate: sends to sessions that are not theirs to address. | `to` must be a well-formed address on this computer (`src/mail.rs:772`); a group target or another member's prefix is refused; an instance takes messages only for its declared sessions. | `tests/envelope.rs::wire_refusals_name_their_reason` (`*`, `<me>/*` → `policy`; another prefix → `not_member`), `tests/embed.rs::an_undeclared_session_cannot_send`, `tests/embed.rs::only_a_listed_persons_computers_reach_its_sessions`, `tests/streams.rs::owner_refuses_a_stream_without_a_grant` | PASS |
| **Compromised relay.** | What iroh/QUIC gives (iroh's documented design; not tested here): a relay forwards end-to-end encrypted QUIC packets addressed by endpoint key; it cannot read or change them, or pose as a member, since the TLS handshake binds each side's key. A relay **can see** which keys talk, their IPs, when, and how much; it **can drop or delay** traffic. What we add: no relay list is fetched (the relays are yours, the team's or the iroh default); relay URLs must be https (`src/relay.rs`, `parse`); messages wait in the outbox while no path exists. | `src/relay.rs::only_https_relay_urls_are_valid`, `src/relay.rs::config_then_team_win_and_no_list_is_the_default`, `tests/relay.rs::relay_down_keeps_mail_queued`, `tests/relay.rs::messages_flow_via_relay_when_direct_is_blocked` (the daemon log names no body) | PASS |

## B. Failure modes

| Failure | Control | Tests | Result |
|---|---|---|---|
| **Daemon crash mid-turn** (`kill -9`). | The outbox row, the hold and the receipt are each written in one SQLite transaction before the sender is answered (`src/outbox.rs:20`, `src/mail.rs` `receive`); nothing that matters lives only in memory. The outbox is resent on the next `hello` under the same envelope id, so the receiver dedupes it. | **New** `tests/envelope.rs::a_kill_9_loses_no_outbox_row_hold_or_receipt` (made it fail twice: deleting `outbox` at `Store::open` — the in-flight envelope is never resent; deleting `holds` there — no open hold after restart). Also `tests/envelope.rs::outbox_survives_restart_and_dedupes` (a clean stop) | PASS |
| **Sleep / wake.** | Same path as a peer going offline: the outbox keeps the message (24 h), redials every 30 s, flushes on `hello`; the receipt walks `queued` → `stored` → `delivered`. | In-process proxy: `tests/envelope.rs::offline_recipient_stays_queued_then_stores_on_reconnect`, `tests/envelope.rs::outbox_expires_after_24h_unreachable`. Real Mac: plan B re-run row 5 (B stopped, restarted) PASS on 0.2.0. **Real-Mac only, not run on 0.3.7** — a sleeping Mac cannot be made in-process. Recipe: plan B "Step 2" row 9 (close B's lid 2 min while A sends to B's Claude; open it; the message arrives and A's `receipt --json` reads `delivered`). | proxy PASS; real-Mac not run (any build since 0.2.0) |
| **Relay down.** | A relay is only one path; direct addresses (from `hello`, the ticket and the loopback file) work without it; the team's relay replaces the default when a member sets it. | **New** `tests/relay.rs::relay_down_leaves_the_direct_path_working` (made it fail: forcing the relay-only knob at `src/net/mod.rs:596` — no direct link). Also `tests/relay.rs::relay_down_keeps_mail_queued` (relay-only side waits, then delivers), `tests/net_two.rs::two_members_on_one_computer_find_each_other`, `tests/relay.rs::team_relay_from_meta_replaces_the_default`, `…::team_relay_overrides_the_default`, `…::relay_clear_returns_to_the_default` | PASS |
| **An agent changes its hook format** (unknown JSON on stdin). | Unparseable stdin is an error that only goes to stderr; an unknown shape or event normalizes to "ignore" (`src/hook.rs:76`); the hook always exits 0 (`src/main.rs:264`). | **New** `tests/hooks.rs::an_unknown_hook_payload_exits_0_and_changes_nothing` (4 agents × 5 events × 7–8 payloads, run as that agent; made it fail twice: `exit(1)` on the error path in `src/main.rs` — "a hook must always exit 0"; and falling back to the argument for an unknown `hook_event_name` in `src/hook.rs` — the hook acted on it and printed output). Real Mac: plan B re-run P4 (20 hooks with `{}` exit 0) | PASS |
| **Two daemons on one computer.** Same home. | `daemon.lock` (`src/daemon/mod.rs:233`): the second waits, touching nothing, and takes over when the first exits. | `tests/daemon_ipc.rs::a_lock_loser_waits_and_takes_over`, `…::two_ensure_running_calls_leave_one_daemon`, `…::ensure_running_respawns_when_a_dying_daemon_holds_the_lock`, `tests/cli.rs::install_refuses_while_the_old_daemon_will_not_stop` | PASS |
| Two daemons on one computer: separate homes (two members). | Each has its own key; they find each other through the loopback file. | `tests/net_two.rs::two_members_on_one_computer_find_each_other`, `tests/net_two.rs::loopback_file_is_withdrawn_on_sigterm` | PASS |
| **Clock skew.** What is compared to what. | Message times, hold times, the 24 h mailbox expiry and the loop window all use this computer's clock (at arrival; for a held message, at Allow); the peer's `sent_at` is not read (`src/mail.rs:633`, `:849`). Receipts and acks carry no timestamp. A roster entry may be stamped at most a day ahead of the receiver's clock (`src/roster.rs:760`); a presence `seq` (the sender's milliseconds) likewise (`src/net/frames.rs:298`), and the first presence on each connection is taken whatever its seq, so a clock that stepped back is not frozen (`src/presence.rs:345`). A link's expiry is stamped by the inviter and compared with the inviter's clock (`src/admission.rs:367`); the joiner's clock is never read. Stream grants are checked against the receiver's clock. | **New** `tests/envelope.rs::a_skewed_peer_clock_sets_no_deadline_here` (made it fail twice: taking the envelope's `sent_at` at `src/mail.rs:633`, `:849` — the message's time is 30 days out; removing the seq bound at `src/net/frames.rs:298` — the far-future presence freezes the next). **New** `tests/join.rs::an_expired_link_is_refused_by_the_inviters_clock` (made it fail: `if false &&` at `src/admission.rs:367` — the candidate is admitted). Also `src/roster.rs::second_root_and_far_future_are_refused`, `tests/streams.rs::expired_grant_is_dropped`, `src/presence.rs::an_older_presence_is_ignored` | PASS |

## C. Parity checklist (spec "Acceptance checklist", rows 1–10)

| # | Row | Automated | Real Mac | State |
|---|---|---|---|---|
| 1 | Own ring Claude → Codex → Cursor → Grok → Claude, labelled "Your <agent>" | `tests/mcp_tools.rs::send_message_from_claude_to_codex_returns_stored_and_receipt_follows`, `tests/push_claude.rs::claude_push_posts_a_user_frame_to_the_inbox_socket_and_marks_delivered`, `tests/push_codex.rs::codex_push_runs_codex_queue_with_the_thread_id`, `tests/hooks.rs::cursor_stop_prints_followup_message`, `tests/hooks.rs::stop_hook_hands_waiting_mail_to_grok_as_block_decision`, `src/label.rs::labels_follow_the_roster`, `src/frame.rs::same_person_is_your_agent` | plan A row 1 and re-run rows 1, 6a: PASS except Grok → Claude BLOCKED (Grok's free usage cap), on a pre-0.2 build | Automated PASS; **real-Mac Grok → Claude hop never run; whole ring not run on 0.3.7** |
| 2 | A teammate's message is stored on arrival, receipt `stored`; no hold, no prompt (membership is the gate) | `tests/holds.rs::a_teammates_message_is_stored_not_held` | superseded: message holds and Allow were removed | PASS |
| 3 | (retired) Allow/withdraw prompt for message holds | none: message holds were removed; permission prompts keep `tests/holds.rs::permission_hold_round_trip` | n/a | RETIRED |
| 4 | (retired) Decline of a held message | none | n/a | RETIRED |
| 5 | (retired) A held message arrives after Allow; a stored one arrives at the next hook | `tests/hooks.rs` (stored → next step) | n/a | RETIRED |
| 6 | Loop limit: the 13th message from one sender to one session in 10 minutes is refused | `src/mail.rs::thirteenth_send_in_ten_minutes_is_refused_with_loop`, `tests/envelope.rs::fourth_hop_and_thirteenth_send_are_refused_loop` | plan A row 3 PASS | PASS |
| 7 | A body that forges the owner header is neutralised | `src/frame.rs::a_forged_owner_line_in_the_body_is_neutralised`, `tests/cli.rs::a_listened_body_cannot_forge_the_reply_line`, `tests/holds.rs::a_forged_prefix_behind_a_hidden_character_is_defanged` | plan A row 4 PASS | PASS |
| 8 | Friday (through the bridge) → a session: agent label; delivered once the workspace is a member and the sender's proof is admitted | Engine side only: `tests/embed.rs::an_admins_once_link_brings_the_workspace_in_and_only_members_reach_friday`, `tests/embed.rs::embedded_instance_joins_and_talks_without_a_socket`, `src/label.rs::a_named_session_renders_its_declared_agent_under_its_member` | not run (needs the bridge) | Engine PASS; the member sidecar the bridge talks to is tested (`tests/member.rs`); **the bridge itself is not in this repo and not run here** |
| 9 | Recipient's Mac asleep: queued message arrives after wake, `stored` → `delivered` | proxy: `tests/envelope.rs::offline_recipient_stays_queued_then_stores_on_reconnect` | plan B re-run row 5 (stop/start, not sleep) PASS on 0.2.0 | **Real-Mac only, not run on 0.3.7** (recipe in B, "Sleep / wake") |
| 10 | `join` on a fresh Mac ends with "Connected as <you>" at once (an invite is the approval); `leave` | `tests/join.rs::joining_says_nothing_of_asking_and_the_commands_know_no_join_requests`, `…::join_takes_the_bare_ticket`, `…::join_takes_the_app_link`, `tests/team_cli.rs::leave_locally_recreates_a_solo_network`, `tests/envelope.rs::leave_reaches_a_peer_or_warns_after_5s` | plan B re-run row 1 PASS (0.2.0, two homes on one Mac); a fresh second Mac: not run | Automated PASS; **fresh-Mac run not done on 0.2.2.** (0.3.x joins by a four-word code or a link; see D and "Real computers".) Two differences from the spec text: the join line is `lightbulb join '<ticket-or-code>' --name NAME`, and `leave` returns this computer to its own network but does not stop the background service (`setup --remove` does). |

## D. Controls added since 0.2.2 (v0.2.3 – v0.3.7)

Removed since 0.2.2, so no row above depends on them: the relay-list fetch, sign-in renewal and
the sign-in flow (no call home, 0.2.4); in 0.4.0, the member kinds (`cloud`), the cloud genesis
marker, person-id vouching, sign-in proofs (`src/proof.rs`, `merge`/`proof` entries now parse as
`Other`), the `cloud_of` and `agents_send_without_asking` policy rows, Always allow, stream input
grants and a person's-other-computer join forwarding; the `admin`/`transfer` roles (now parsed as `Other`,
never verified, `src/roster.rs::retired_kinds_are_ignored`); the hosted-apps decision path and
the hub relays (`src/decision.rs`, `src/http.rs` deleted in 0.3.0; `tests/app_send.rs` now
proves a computer ignores app envelopes). The "no admins" rule is the flat network below.

### D1. Flat networks, people, agents

| Control added | Where | Tests | Result |
|---|---|---|---|
| **Flat network**: any computer member invites (makes a link), removes another member, renames the team and sets the relay. | `Kind::Remove` `src/roster.rs:550`, `Kind::Link` `:563`, `Kind::Meta` `:585`; gate `src/daemon/net_ops.rs:61`. | `src/roster.rs::plain_member_invites_removes_and_renames`, `src/roster.rs::any_member_admits_through_any_live_link_until_a_removal_ends_it`, `tests/team_cli.rs::a_plain_member_opens_links_and_removes_the_founder`, `tests/join.rs::non_admin_inviter_admits_its_own_invitee_with_no_admin_online` | PASS |
| Nobody removes itself by a `remove` entry (`target != e.signer`); leaving is the separate `leave` entry. | `src/roster.rs:551`, `:556`. | `leave`: `tests/team_cli.rs::leave_locally_recreates_a_solo_network`. A self-signed `remove` entry being ignored: **NO TEST** | leave PASS; self-remove NO TEST |
| ~~Cloud members never admit and never mint links; a workspace's cloud founder is never removed.~~ | **Removed in 0.4.0** (one member kind, `docs/security.md` "The signed roster"): every member admits, mints links and can be removed. | `src/roster.rs::plain_member_invites_removes_and_renames`, `src/roster.rs::a_member_admits_through_its_link_but_never_an_admin`, `tests/member.rs::a_member_in_anothers_team_mints_a_person_link` | REMOVED |
| **Agents may invite, join and approve**; an approval records the approving session as `by` (only a session of the signer is kept). | Actor resolution `src/daemon/net_ops.rs:93`, `src/daemon/ipc.rs:321`; `by` written `src/admission.rs:465`, read `src/roster.rs:705`. | `tests/team_cli.rs::an_agent_shell_may_invite_in_every_mode`, `tests/join.rs::an_agent_may_join`, `tests/join.rs::an_agent_may_approve_a_join_and_the_answer_names_it`, `tests/holds.rs::an_agent_shell_may_answer_a_hold_and_nothing_else`, `src/roster.rs::an_admit_keeps_by_only_for_a_session_of_its_signer` | PASS |
| ~~**Person ids** are vouched only by a `person` link signed by a cloud-marked genesis founder.~~ | **Removed in 0.4.0** (one member kind, `docs/security.md` "The signed roster"): a member's `person_id` is its own key, whatever its admit says. | `src/roster.rs::every_member_is_its_own_person_whatever_its_admit_says`, `tests/member.rs::a_computer_joined_by_a_person_link_is_that_person_to_the_member_alone` | REMOVED |
| ~~A vouched id is never key-form (`identity::is_key`).~~ | **Removed in 0.4.0** (one member kind, `docs/security.md` "The signed roster"): nothing is vouched. | none | REMOVED |
| **Policy** (retired: no per-person policy rows; membership is the gate). | n/a | n/a |
| ~~A workspace network from before the cloud marker is **founded again**.~~ | **Removed in 0.4.0** (one member kind, `docs/security.md` "The signed roster"): an instance not in its stored roster founds a fresh network, as any removed member does (`boot`, `src/embed.rs`). | none (the not-a-member path) | REMOVED |

### D2. Workspace members gate and wake

| Control added | Where | Tests | Result |
|---|---|---|---|
| **Hosted-app mail here**: a hosted app's message to a session on its person's computer is framed as "<App> (you)" (defanged), never as typed at the terminal, and is never hop 0, so the agents' loop limits count it. Not held: the person connected the app. | `src/daemon/ipc.rs` (`send_as`), `src/label.rs` (`hosted_app`) | `tests/app_send.rs::app_mail_is_framed_as_the_app_never_as_its_person`, `tests/app_send.rs::app_mail_is_never_hop_zero` | PASS |
| **Route link**: a signed-in computer keeps one WebSocket to Lightbulb, proves its key over the nonce and shows its proof, runs only `app.*` and `directory` for its own person's app sessions, sends a content-free mail notice, and drops the link on logout. | `src/link.rs` | `tests/link.rs` | PASS |
| **Members gate**: when an instance has `members` (people), mail from a computer whose sign-in proof (shown in its `hello`, verified offline) does not name one of them is refused `not_a_member`, with no hold: no proof, another person's, another computer's, or an expired one. Absent keeps the gate, `null` removes it, a list replaces it, a bad value is refused. | `src/mail.rs` (`receive`), `src/holds.rs` (`answer`); set by the sessions file and `cloud.sessions.set`. | `tests/embed.rs::an_admins_once_link_brings_the_workspace_in_and_only_members_reach_friday`, `tests/embed.rs::only_listed_computers_reach_its_sessions`, `tests/member.rs::the_members_list_gates_who_may_message_the_sessions`, `tests/member.rs::sessions_set_members_absent_keeps_null_removes_list_replaces_bad_refuses`, `tests/member.rs::a_computer_without_a_members_proof_is_refused`, `src/proof.rs` unit test. That no turn starts: the turn is the bridge's, not in this repo, **NO TEST** | gate PASS; turn NO TEST |
| **Per-member wake**: the roster carries a member's own `wake {url, token}` only if the URL is `https` with a public-looking DNS host (no IP, `localhost`, userinfo or single label) and the token is a plain word. | `wake_host`, `Wake::parse`, taken in `pass` (`src/roster.rs`). | `src/roster.rs::wake_parse_takes_http_s_and_a_plain_token_only`, `src/roster.rs::each_members_own_wake_is_taken`. A DNS name that resolves to a private address is accepted by design | PASS |
| The doorbell is rung only after a failed dial; the token goes to curl on **stdin** (`-K -`), never its command line or a log. | `doorbell` `src/net/mod.rs:1196`. | `tests/member.rs::a_computer_rings_the_doorbell_only_after_a_failed_dial`, `tests/member.rs::a_computer_rings_a_non_founder_members_doorbell`, `tests/member.rs::the_doorbell_token_is_absent_from_logs` (asserts curl's stdin and that argv and logs lack the token), `tests/member.rs::a_join_to_a_parked_member_rings_its_doorbell` | PASS |

### D3. Four-word codes and links

| Control added | Where | Tests | Result |
|---|---|---|---|
| Code is four words from the EFF short list. | `src/words.txt`, `generate` `src/code.rs:53`. | `src/code.rs::words_are_plain_and_enough` (at least 1024 words, lowercase), `src/code.rs::a_code_round_trips_through_every_separator`, `src/code.rs::what_is_not_a_code_is_refused` | PASS |
| Endpoint key is Argon2id, 64 MiB, 3 passes, salted with context and the 10-minute window. | `secret` `src/code.rs:91`, `MEM_KIB` `:31`. | `src/code.rs::the_derivation_is_deterministic_and_per_code` (debug builds use 1 MiB; the cost itself is not measured by a test: **NO TEST** for the 64 MiB figure in a test run) | PASS (derivation); cost NO TEST |
| A **release build** cannot derive with less than 64 MiB. | Compile-time assert `src/code.rs:80`. | Fails the release build, not a test; the release workflow builds it | enforced at build |
| Single use; 5 bad callers close the listener; 10-minute life. | `MAX_BAD` `src/code.rs:20`, `LIFE_S` `:27`, `live.len()`/`expires_at.min(now + LIFE_S)` `:144`. | `tests/code.rs::a_code_joins_once_and_a_second_try_or_a_wrong_code_fails`, `tests/code.rs::five_bad_callers_close_the_listener`. The 10-minute expiry: **NO TEST** | single use, 5 bad PASS; expiry NO TEST |
| Each code serves a fresh **once** link of its own, so a code works once. | `offer` `src/code.rs:109` (`code_link`, `src/daemon/net_ops.rs`), `src/admission.rs:316`. | `tests/code.rs::a_code_for_an_open_link_still_asks_the_inviter`, `tests/code.rs::a_code_serves_a_single_use_ask_link_of_its_own` | PASS |
| A wrong code ends in bounded time with a plain `no_code`, never repeating the code. | Joiner `FETCH` 15 s `src/code.rs:25`/`:212`. | `tests/code.rs::a_code_joins_once_and_a_second_try_or_a_wrong_code_fails` (answer is `no_code` and does not repeat the code). The 15 s bound itself: **NO TEST** | answer PASS; bound NO TEST |
| `https://thelightbulb.company/j#<ticket>` decodes like the bare ticket and the app link. | `WEB` `src/ticket.rs:13`, `decode` `:140`. | `src/ticket.rs::ticket_round_trips_through_every_link_form`, `tests/join.rs::join_takes_the_app_link`, `tests/join.rs::join_takes_the_bare_ticket`, `tests/code.rs::a_code_joins_once_and_a_second_try_or_a_wrong_code_fails` (the printed link starts with the `/j#` form) | PASS |

### D4. Updates, usage counts, setup

| Control added | Where | Tests | Result |
|---|---|---|---|
| Auto-update installs only a strictly newer release, once. | `should_auto_update` `src/cli.rs:1296`. | `src/cli.rs::auto_update_installs_only_a_strictly_newer_release_once`, `tests/update.rs::the_same_version_downloads_nothing`, `tests/update.rs::versions_before_0_2_2_are_refused` | PASS |
| SHA-256 of the tarball must match the release's `.sha256`; a mismatch changes nothing. | `install_release` `src/cli.rs:1233`, compare `:1248`. | `tests/update.rs::a_wrong_checksum_changes_nothing`, `tests/update.rs::update_installs_the_release_and_runs_its_setup` | PASS |
| macOS: the new binary must meet code requirement team `WZX4U3C22Y`. | `REQUIREMENT` `src/cli.rs:1205`, `verify_signature` `:1219`. | `src/cli.rs::the_signature_requirement_is_pinned_to_our_team` (macOS only; refuses an Apple-signed `/bin/ls`). Linux: checksum only, as documented | PASS (macOS) |
| A symlink where the release binary should be is refused. | `symlink_metadata(..).is_file()` `src/cli.rs:1260`. | **NO TEST** | NO TEST |
| Only the installed binary updates itself (not a side-by-side state dir, not `cargo run`). | `is_installed_exe` `src/cli.rs:1337`, used at `:1352`. | **NO TEST** | NO TEST |
| The daemon never edits a shell profile; only a terminal command adds `~/.local/bin`, once, and never through a symlinked profile. | `src/cli.rs:1433` (`person`), `:1463` (`add_to_path`). | `tests/cli.rs::setup_off_a_terminal_or_on_a_symlinked_profile_leaves_the_profile_alone` (macOS only), `tests/cli.rs::daemon_install_adds_local_bin_to_the_zsh_profile_once` (macOS only) | PASS (macOS) |
| `update --auto off|on` and `auto_update` in config / `LIGHTBULB_NO_AUTO_UPDATE=1` turn the daily check off; doctor shows it. | `auto_update_on` `src/cli.rs:1290`; daemon loop `src/daemon/mod.rs:298`. | `tests/cli.rs::update_auto_off_and_on_set_the_daily_update`, `src/cli.rs::auto_update_is_off_by_config` | PASS |
| After an auto-update the daemon runs the new `setup` once. | `refresh_setup` `src/cli.rs:1314`. | `src/cli.rs::setup_reruns_after_a_version_change` (the decision only; the spawn: **NO TEST**) | decision PASS |
| **Usage counts**: allowed fields only (`arch`, `os`, `version`, plus fixed per-event counts). | `build` `src/telemetry.rs:94`. | `src/telemetry.rs::events_carry_only_allowed_fields`, `src/telemetry.rs::join_input_is_classified` | PASS |
| An env opt-out (`DO_NOT_TRACK`, `LIGHTBULB_NO_TELEMETRY`) always wins, and is saved to config so a launchd daemon obeys it. | `env_off` `src/telemetry.rs:144`, `send` `:225`, `sync_env_optout` `:166`. | `src/telemetry.rs::an_env_opt_out_is_saved_to_config_and_gates_the_daemon`, `tests/cli.rs::telemetry_on_under_do_not_track_stays_off` | PASS |
| Test builds send nothing unless `LIGHTBULB_TELEMETRY_FORCE` is set. | `enabled` `src/telemetry.rs:136`. | `src/telemetry.rs::the_gate_reads_config_and_build` | PASS |
| Counting never blocks the command: one `curl` with a 3 s limit, reaped off the caller's path. | `post` `src/telemetry.rs:242`. | **NO TEST** | NO TEST |
| MCP server is registered as `lightbulb`; an old `lightbulb-network` entry is removed when it runs a `lightbulb` binary; a `lightbulb` entry is replaced only when it is ours, the app's `session-mcp` one or the hosted MCP's. | `DEFAULT_SERVER`/`OLD_SERVER`/`replaceable` `src/setup/mod.rs`. | `src/setup/tests.rs::our_old_lightbulb_network_entry_moves_to_the_new_name_and_anothers_stays`, `src/setup/tests.rs::setup_takes_the_lightbulb_name_from_the_app_or_hosted_entry_only`, `tests/cli.rs::setup_replaces_the_apps_lightbulb_tool_server`, `tests/mcp_tools.rs::the_server_advertises_its_installed_name` | PASS |
| `find_sessions` takes a session's `who` name (`claude-5`) as a query and returns it as `name`. | `src/mcp.rs:280`, `src/presence.rs:522`. | `tests/mcp_tools.rs::find_sessions_marks_is_this_session` | PASS |
| Off macOS, `setup` restarts a daemon whose version differs from the binary; `doctor` names such a daemon and the fix. | `restart_if_old` `src/cli.rs:1557` (called `:1864`), doctor `:1706`. | **NO TEST** (found on a real Linux computer; see "Real computers") | NO TEST |

### D5. 0.4.1 – 0.4.3 (found on prod on 2026-10-25)

| Control | Where | Test | Result |
|---|---|---|---|
| A bare `from` (`friday`) is this member's own session, as `to` already was; before, every workspace agent send was refused `not_declared`. | `src/daemon/ipc.rs`, `send` | `tests/embed.rs::a_person_line_reaches_friday_and_an_unlisted_teammate_does_not` (sends in the bridge's shape: bare `from`, no `to`, `reply_to` = the request id) | PASS |
| A receipt to a parked member with a doorbell carries `waking`; the CLI says it is waking up, not offline. | `src/daemon/ipc.rs`, `finish`; `src/mail.rs`, `receipt_text` | `src/mail.rs::a_parked_workspace_reads_as_waking_not_offline` | PASS |
| OpenCode: provider, caller, hooks, setup (MCP entry and plugin). Every pushed message carries the session's own agent and model, so a teammate's mail cannot switch a session from `plan` to `build`; with none recorded the plugin does not push and the mail waits for `check_inbox`. | `src/setup/opencode.rs`, `src/hook.rs`, `src/agents/caller.rs` | the OpenCode cases in `tests/hooks.rs` and `src/setup/tests.rs`, and the plugin smoke test (needs node) | PASS (unit and integration); **not run in a live OpenCode session** |

## Real computers

Observed on 2026-10-24 on v0.3.6 and v0.3.7 (nothing here was re-run for this file): the owner's
Mac (home network) and a Windows/WSL computer elsewhere, over the n0 relays.

- WSL `who --all` listed the Mac's three Claude Code sessions.
- WSL to a Mac Claude Code session: it arrived as a teammate contribution, sender "WSL, at the
  terminal (not verified)".
- Mac to a WSL inbox with `--reply-to`: read by `listen --as inbox --once`; the receipt read
  "Delivered to WSL on GlenhardieWindows".
- Four-word code: `invite` printed the code first, with "(the code works for 10 minutes)". On
  WSL, `leave`, then `join <code>` printed "Asking whoever sent the link…". The Mac's `held`
  showed the join request; `approve` joined it. WSL printed "Connected … 2 computers online".
- Found and fixed: a hand-started daemon on Linux kept running the old binary after an update
  (fixed in 0.3.7: `setup` restarts it, `doctor` names it; D4, NO TEST).
- A fresh Claude Code session found the `lightbulb-network` tools through the skill, and its
  message's receipt went `stored` then `delivered`.

On 2026-10-25, on prod (engine 0.4.1 in the workspace, 0.4.1 on WSL): WSL sent
`emxq…/friday` "reply with OK"; the parked workspace was woken by its doorbell (wake to stored
about 10 s), Friday ran one turn, and WSL read "From The Lightbulb Company's hermes (not
verified): OK" with nothing held. The label now reads the agent's name (backend change). Three
earlier tries failed on the bugs D5 and the bridge fixed: the bridge's reply needed `to`, the
engine refused a bare `from`, and the bridge's request ids held a `:`.

Earlier real-Mac passes are cited row by row above from `docs/acceptance-plan-a.md` and
`docs/acceptance-plan-b.md` (0.2.0 builds) and from `docs/acceptance-two-mac.md`; they stay as
cited.

## Numbers

- `cargo test --no-fail-fast`: **439 passed, 0 failed, 2 ignored**, across 26 suites plus doc-tests (0 tests). Ignored: `daemon::net_ops::tests::sleep_under_me` (a helper process another test runs, not a test) and `tests/perf.rs::twenty_computers_fifty_sessions` (run by hand, see `docs/performance.md`). 401 passed on 0.2.2, so +38.
- Per suite (passed): unit tests in `src/lib.rs` 168 (1 ignored); `tests/`: app_send 4, cli 35, code 4, daemon_ipc 5, dialog 3, directory 7, embed 10, envelope 18, holds 17, hooks 14, identity_ipc 1, join 31, mail_ipc 5, mcp_tools 17, member 21, net_two 17, perf 0 (1 ignored), push_claude 2, push_codex 3, relay 8, sessions_ipc 1, signin 8, streams 20, team_cli 13, ui_claim 3, update 4. `src/main.rs` and doc-tests: 0.
- Time: 201 s summed over the suites' own timers (the biggest: `tests/member.rs` 66 s, `tests/code.rs` 31 s, `tests/join.rs` 21 s); 274 s wall for the whole command including the build from a cold target directory. One Apple-silicon Mac, macOS 26.2, rustc 1.96.0, debug profile.
- Commit the numbers came from: `d500099` (v0.3.7; this file adds no code). Date: 2026-10-24.
- The run used `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0` and a separate `CARGO_TARGET_DIR`. Neither changes what is compiled or tested (`debug_assertions` stays on). Several D rows are macOS-only tests; they ran, since this was a Mac. A Linux run (CI) skips them.
- Not run for this file: `cargo clippy` and `cargo fmt --check`.

## Open

Red rows: none. Untested or open:

1. **No adversarial or fuzz suite.** Hostile frames are covered by the named cases above, not by
   a fuzzer.
2. **No failure-injection document.** The failure modes in B have tests, but there is no
   written plan that injects faults systematically.
3. **Performance at 50 sessions on 20 computers** was measured once, on 0.2.2
   (`docs/performance.md`, `tests/perf.rs`, ignored by default); it was **not re-measured on
   0.3.7**, and there are no numbers for a network of real computers.
4. **No second real person yet.** Every real-computer run above is the owner's own two
   computers; a teammate who is a different human has not joined a real network.
5. **Sleep/wake on a real Mac** (B, C9) and **two real Macs on different networks** on 0.3.7:
   the Mac and WSL run above is one pair on different networks, not a sleep/wake run.
6. **C1**: the Grok to Claude hop has never passed on a real Mac (Grok's usage cap); the ring was
   not re-run on 0.3.7.
7. **C5**: held, Allow, next Cursor/Grok step is proven in two halves, not as one test.
8. **C8**: the bridge (Friday) is not in this repo; only the engine's cloud-member side and the
   member sidecar (`tests/member.rs`) are tested here. A real reply from Friday passed on prod on
   2026-10-25 (see "Real computers").
9. **C10**: no fresh-Mac join on 0.3.7 with a Mac as the joiner.
10. **NO TEST rows in D**: a self-signed `remove` entry; a started turn on a refused
    non-member; the 64 MiB Argon2 cost in a test run; the
    10-minute code expiry and the 15 s wrong-code bound; a symlinked release binary; the
    installed-binary-only check; the spawn after an auto-update; counting never blocking; the
    off-macOS daemon restart and the doctor line.
11. **Replay after 8 days.** `seen_ids` and finished outbox rows are pruned after 8 days
    (`src/outbox.rs:17`, `:88`). An envelope its own sender resends after that is stored as new.
    Only that member's key can send on its connection, so no third party can do this. No test.
12. **Hooks against a hostile local user.** The caller check stops an agent's own tools; any
    process of the same user can speak raw IPC to the socket (`src/hook.rs` header). There is no
    control for that, by design.
13. **Relay metadata.** A relay sees who talks to whom, when and how much. No padding or cover
    traffic. A team relay is set by a member's roster entry, so a team trusts its members with
    that choice.
14. **Flat networks trust every computer member.** Any computer member can remove another and
    invite anyone (D1); that is the design, so there is no control against a malicious member
    beyond Allow/Decline holds on messages.
15. **Presence clock skew under a day.** A peer up to 24 h ahead is accepted; if its clock then
    steps back, its later presences on the same connection are ignored until a reconnect (the
    first on each connection is always taken). No test for the reconnect recovery beyond
    `src/presence.rs::an_older_presence_is_ignored`.
16. **Four-word codes are about 41 bits.** Argon2id and the 10-minute salt make a guess costly;
    `docs/security.md` states what remains (a relay operator with a billion cores).
17. **iroh's transport guarantees** are relied on, not tested here.
