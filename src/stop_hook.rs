//! `loopherd stop-hook`: Claude's Stop and StopFailure hook. Each stop of a
//! member lands as a `stop` entry in its mailbox, unless the hook blocks it
//! to ask for a handoff; anything else is ignored.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde_json::Value;

use crate::context::Check;
use crate::continuation::{follow_recorded, live_for};
use crate::ledger::{AgentRecord, Ledger, Process};
use crate::process::{self, Claim, Procs};
use crate::report::{Background, Body, Entry, Stop, Tally, Unknown, append, member_of_own_pane};
use crate::shepherd_resume::{self, Place, StopAt};

/// Where the hook runs: `$HERDR_PANE_ID`, `$HERDR_SOCKET_PATH` and `$CLAUDE_PID`.
pub struct Pane<'a> {
    pub id: &'a str,
    pub socket: Option<&'a Path>,
    pub claude_pid: Option<u32>,
    /// The caller and its ancestors, which carry a moved member's old pane id.
    pub lineage: &'a [Process],
}

/// What the hook did with a member's stop.
#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    Filed(String),
    /// Not filed: Claude continues with `reason` as its next instruction.
    Blocked {
        name: String,
        reason: String,
    },
}

/// Records the stop `input` describes for the member in `pane`, or blocks it
/// when `handoff` gives a reason; `None` when the pane is no member's or the
/// stop came from another Claude than the member's. With the member's process known, that is
/// the test, and a stop from it carries the member's current session into the
/// ledger; otherwise the hook tries to claim the process. A Claude that isn't
/// the pane's top-level one is dropped; if the claim fails for another reason,
/// or there is no usable `$CLAUDE_PID`, sessions are compared. Sessions are
/// taken as `live` resolves them, and a recorded one that resolves elsewhere
/// is moved there first.
#[allow(clippy::too_many_arguments)]
pub fn stop_hook(
    ledger: &mut dyn Ledger,
    procs: &dyn Procs,
    dir: &Path,
    pane: &Pane,
    input: &[u8],
    now: SystemTime,
    live: &dyn Fn(&str) -> String,
    handoff: &dyn Fn(&AgentRecord) -> Check,
    warn: &mut dyn FnMut(String),
) -> Result<Option<Stopped>> {
    let entries = ledger.entries()?;
    let Some(member) = member_of_own_pane(&entries, pane.id, pane.lineage, pane.socket) else {
        return Ok(None);
    };
    let member = match follow_recorded(ledger, &entries, member, live) {
        Ok(followed) => followed.unwrap_or_else(|| member.clone()),
        Err(e) => {
            warn(format!("could not update {}'s session: {e:#}", member.name));
            // Compared as if moved, so the stop isn't dropped against a stale session.
            AgentRecord {
                session_id: member
                    .session_id
                    .as_deref()
                    .map(|s| live_for(&entries, member, s, live)),
                ..member.clone()
            }
        }
    };
    let member = &member;
    let mut stop = parse(input);
    stop.session = stop.session.map(|s| live_for(&entries, member, &s, live));
    if let Some(pid) = pane.claude_pid {
        let exact = pane.socket.is_some() && member.socket.as_deref() == pane.socket;
        match process::live(procs, member.process) {
            Some(ours) if ours.pid == pid => {
                if let Err(e) = follow_session(ledger, &entries, member, &stop) {
                    warn(format!("could not update {}'s session: {e:#}", member.name));
                }
                return settle(dir, pane, member, stop, now, handoff, warn);
            }
            Some(ours) => {
                warn(format!(
                    "dropped a stop from process {pid} in {}'s pane; its process is {}",
                    member.name, ours.pid
                ));
                return Ok(None);
            }
            None => {
                let claim =
                    process::claim(ledger, procs, member, exact, pid, stop.session.as_deref())
                        .unwrap_or_else(|e| {
                            warn(format!("could not claim {}'s process: {e:#}", member.name));
                            Claim::Declined
                        });
                match claim {
                    Claim::Claimed => return settle(dir, pane, member, stop, now, handoff, warn),
                    Claim::Nested => {
                        warn(format!(
                            "dropped a stop from process {pid} in {}'s pane; it is not the pane's top-level Claude",
                            member.name
                        ));
                        return Ok(None);
                    }
                    Claim::Declined => {}
                }
            }
        }
    }
    if let (Some(ours), Some(theirs)) = (&member.session_id, &stop.session)
        && ours != theirs
    {
        warn(format!(
            "dropped a stop from session {theirs} in {}'s pane; its session is {ours}",
            member.name
        ));
        return Ok(None);
    }
    settle(dir, pane, member, stop, now, handoff, warn)
}

fn follow_session(
    ledger: &mut dyn Ledger,
    entries: &BTreeMap<String, AgentRecord>,
    member: &AgentRecord,
    stop: &Stop,
) -> Result<()> {
    let owned_by_other = |s: &String| {
        entries
            .values()
            .any(|e| !e.retired && e.name != member.name && e.session_id.as_ref() == Some(s))
    };
    let Some(session) = stop
        .session
        .as_ref()
        .filter(|s| member.session_id.as_ref() != Some(s) && !owned_by_other(s))
    else {
        return Ok(());
    };
    ledger.update(&member.name, &mut |record| {
        record.session_id = Some(session.clone());
        Ok(())
    })?;
    Ok(())
}

/// Blocks the stop if `handoff` says to; otherwise files it. A failing
/// `handoff` files the stop with a warning and whatever it measured. Only a member recorded in this
/// pane's herdr is asked, never a socketless one.
fn settle(
    dir: &Path,
    pane: &Pane,
    member: &AgentRecord,
    mut stop: Stop,
    now: SystemTime,
    handoff: &dyn Fn(&AgentRecord) -> Check,
    warn: &mut dyn FnMut(String),
) -> Result<Option<Stopped>> {
    let name = member.name.clone();
    let ours = pane.socket.is_some() && member.socket.as_deref() == pane.socket;
    let check = if ours {
        handoff(member)
    } else {
        Check::default()
    };
    match check.reason {
        Ok(Some(reason)) => return Ok(Some(Stopped::Blocked { name, reason })),
        Ok(None) => {}
        Err(e) => warn(format!("could not check {name}'s context: {e:#}")),
    }
    stop.context = check.measured.map(|m| m.tokens);
    stop.limit = check.measured.map(|m| m.limit);
    append(dir, &name, &Entry::new(now, Body::Stop(stop)))?;
    Ok(Some(Stopped::Filed(name)))
}

/// Records a stop of the shepherd's Claude at `place` in its handoff record,
/// read as a member's stop is; nothing when no record names `place`.
pub fn shepherd_stopped(state: &Path, place: &Place, input: &[u8], now: SystemTime) -> Result<()> {
    let stop = StopAt {
        at: humantime::format_rfc3339_nanos(now).to_string(),
        stop: parse(input),
    };
    shepherd_resume::update(state, place, |r| r.restart.stop = Some(stop))?;
    Ok(())
}

/// Unparseable input still records the stop, with everything unknown.
fn parse(input: &[u8]) -> Stop {
    let input: Value = serde_json::from_slice(input).unwrap_or(Value::Null);
    let background = match input.get("background_tasks").and_then(Value::as_array) {
        Some(tasks) => Tally::Known(tasks.iter().fold(Background::default(), |mut b, task| {
            match task.get("type").and_then(Value::as_str) {
                Some("shell") => b.shells += 1,
                Some("subagent") => b.subagents += 1,
                _ => b.other += 1,
            }
            b
        })),
        None => Tally::Unknown(Unknown::Unknown),
    };
    let crons = match input.get("session_crons").and_then(Value::as_array) {
        Some(crons) => Tally::Known(crons.len() as u32),
        None => Tally::Unknown(Unknown::Unknown),
    };
    let failed = input.get("hook_event_name").and_then(Value::as_str) == Some("StopFailure");
    let error = match input.get("error") {
        Some(Value::String(error)) => Some(error.clone()),
        Some(Value::Null) | None if failed => Some("unknown".into()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    };
    let session = input
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Stop {
        background,
        crons,
        error,
        context: None,
        limit: None,
        session,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ledger::{AgentRecord, MemLedger, NoUpdate, Process};
    use crate::process::{CLAUDE, FakeProcs, PANE_SHELL};

    #[test]
    fn the_shepherds_stop_is_recorded_only_at_its_recorded_place() {
        let state = tempfile::tempdir().unwrap();
        let place = Place {
            pane: "w1:p1".into(),
            socket: SOCKET.into(),
            process: Process {
                pid: 30,
                start: 300,
            },
        };
        let at = humantime::parse_rfc3339("2026-11-19T10:00:00Z").unwrap();
        shepherd_resume::record(state.path(), &place, None, "s-1", at).unwrap();
        let paused = br#"{"background_tasks":[{"type":"shell"}],"session_crons":[]}"#;
        let other = Place {
            process: Process {
                pid: 31,
                start: 310,
            },
            ..place.clone()
        };

        shepherd_stopped(state.path(), &other, paused, at).unwrap();
        let saved = || -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(state.path().join("shepherd.json")).unwrap())
                .unwrap()
        };
        assert!(saved().get("stop").is_none());

        shepherd_stopped(state.path(), &place, paused, at).unwrap();
        let stop = &saved()["stop"];
        assert_eq!(stop["at"], "2026-11-19T10:00:00.000000000Z");
        assert_eq!(stop["background"]["shells"], 1);
    }

    const SOCKET: &str = "/run/herdr/herdr.sock";

    fn member(name: &str, pane: &str, socket: Option<&str>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
            session_id: None,
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            outcome: None,
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: socket.map(PathBuf::from),
            process: None,
            role: None,
            after_merge: None,
            unsandboxed: None,
            sockets_unblocked: None,
            earlier: vec![],
            unknown: Default::default(),
        }
    }

    fn ledger(members: Vec<AgentRecord>) -> MemLedger {
        MemLedger(
            members.into_iter().map(|m| (m.name.clone(), m)).collect(),
            vec![],
        )
    }

    fn at() -> SystemTime {
        humantime::parse_rfc3339("2026-11-02T09:00:00Z").unwrap()
    }

    fn run(
        ledger: &MemLedger,
        dir: &Path,
        pane: &str,
        socket: Option<&str>,
        input: &str,
    ) -> Option<String> {
        let mut ledger = ledger.clone();
        run_as(
            &mut ledger,
            &FakeProcs::default(),
            None,
            dir,
            pane,
            socket,
            input,
            &mut |_| {},
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_as(
        ledger: &mut MemLedger,
        procs: &FakeProcs,
        claude_pid: Option<u32>,
        dir: &Path,
        pane: &str,
        socket: Option<&str>,
        input: &str,
        warn: &mut dyn FnMut(String),
    ) -> Option<String> {
        let pane = Pane {
            id: pane,
            socket: socket.map(Path::new),
            claude_pid,
            lineage: &crate::process::lineage(procs, 30),
        };
        let filed = stop_hook(
            ledger,
            procs,
            dir,
            &pane,
            input.as_bytes(),
            at(),
            &|s| s.to_string(),
            &|_| Check::default(),
            warn,
        )
        .unwrap();
        filed.map(|s| match s {
            Stopped::Filed(name) => name,
            Stopped::Blocked { name, .. } => panic!("blocked {name}"),
        })
    }

    fn mailbox(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(format!("{name}/mailbox.jsonl"))).unwrap_or_default()
    }

    #[test]
    fn a_stop_with_background_work_records_its_counts_and_crons() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", Some(SOCKET)),
            member("lh-2", "w1:p2", Some(SOCKET)),
        ]);
        let input = r#"{"hook_event_name":"Stop","background_tasks":[
            {"id":"a","type":"shell","status":"running","command":"sleep 240"},
            {"id":"b","type":"shell","status":"running"},
            {"id":"c","type":"subagent","status":"running"},
            {"id":"d","type":"monitor","status":"running"}],
            "session_crons":[{"id":"x"}]}"#;

        assert_eq!(
            run(&ledger, dir.path(), "w1:p2", Some(SOCKET), input).as_deref(),
            Some("lh-2")
        );

        assert_eq!(
            mailbox(dir.path(), "lh-2"),
            "{\"at\":\"2026-11-02T09:00:00.000000000Z\",\"kind\":\"stop\",\
             \"background\":{\"shells\":2,\"subagents\":1,\"other\":1},\"crons\":1}\n"
        );
        assert_eq!(mailbox(dir.path(), "lh-1"), "");
    }

    #[test]
    fn empty_arrays_are_a_finished_stop() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET))]);
        run(
            &ledger,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            r#"{"hook_event_name":"Stop","background_tasks":[],"session_crons":[]}"#,
        );
        assert!(
            mailbox(dir.path(), "lh-1")
                .contains(r#""background":{"shells":0,"subagents":0,"other":0},"crons":0}"#)
        );
    }

    #[test]
    fn a_member_moved_to_another_workspace_records_its_stop_from_its_own_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(vec![moved_from_w1_p1()]);

        let filed = run_as(
            &mut ledger,
            &FakeProcs::pane(),
            None,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            r#"{"hook_event_name":"Stop","background_tasks":[],"session_crons":[]}"#,
            &mut |_| {},
        );

        assert_eq!(filed.as_deref(), Some("lh-1"));
        assert!(mailbox(dir.path(), "lh-1").contains(r#""crons":0"#));
    }

    #[test]
    fn a_stranger_in_a_pane_that_took_a_moved_members_old_id_after_a_restart_is_no_member() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(vec![moved_from_w1_p1()]);

        let filed = run_as(
            &mut ledger,
            &FakeProcs::restarted_pane(),
            Some(30),
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            r#"{"hook_event_name":"Stop","background_tasks":[],"session_crons":[]}"#,
            &mut |_| {},
        );

        assert_eq!(filed, None);
        assert_eq!(ledger.0["lh-1"].process, None);
    }

    fn moved_from_w1_p1() -> AgentRecord {
        let mut moved = member("lh-1", "w3:p2", Some(SOCKET));
        moved.env_pane_id = Some("w1:p1".into());
        moved.env_pane_shell = Some(PANE_SHELL);
        moved
    }

    #[test]
    fn null_or_missing_arrays_are_recorded_as_unknown() {
        for input in [
            r#"{"hook_event_name":"Stop"}"#,
            r#"{"hook_event_name":"Stop","background_tasks":null,"session_crons":null}"#,
            "not json",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let ledger = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET))]);
            run(&ledger, dir.path(), "w1:p1", Some(SOCKET), input);
            assert!(
                mailbox(dir.path(), "lh-1").ends_with(
                    "\"kind\":\"stop\",\"background\":\"unknown\",\"crons\":\"unknown\"}\n"
                ),
                "{input}: {}",
                mailbox(dir.path(), "lh-1")
            );
        }
    }

    #[test]
    fn a_stop_failure_records_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET))]);
        run(
            &ledger,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            r#"{"hook_event_name":"StopFailure","error":"rate_limit"}"#,
        );
        run(
            &ledger,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            r#"{"hook_event_name":"StopFailure"}"#,
        );
        let lines: Vec<String> = mailbox(dir.path(), "lh-1")
            .lines()
            .map(str::to_string)
            .collect();
        assert!(
            lines[0].ends_with(r#""error":"rate_limit"}"#),
            "{}",
            lines[0]
        );
        assert!(lines[1].ends_with(r#""error":"unknown"}"#), "{}", lines[1]);
    }

    #[test]
    fn a_pane_of_another_herdr_or_no_member_records_nothing() {
        let ledger = ledger(vec![
            member("lh-1", "w1:p1", Some(SOCKET)),
            member("old", "w1:p3", None),
        ]);
        let mut retired = ledger.0.clone();
        retired.get_mut("old").unwrap().retired = true;
        let retired = MemLedger(retired, vec![]);
        let cases: [(&MemLedger, &str, Option<&str>); 4] = [
            (&ledger, "w1:p1", Some("/run/other/herdr.sock")),
            (&ledger, "w1:p1", None),
            (&ledger, "w1:p2", Some(SOCKET)),
            (&retired, "w1:p3", Some(SOCKET)),
        ];
        for (ledger, pane, socket) in cases {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(
                run(ledger, dir.path(), pane, socket, "{}"),
                None,
                "{pane} {socket:?}"
            );
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                0,
                "{pane} {socket:?}"
            );
        }
    }

    #[test]
    fn a_socketless_entry_matches_only_a_caller_without_a_socket() {
        let ledger = ledger(vec![member("lh-1", "w1:p1", None)]);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run(&ledger, dir.path(), "w1:p1", None, "{}").as_deref(),
            Some("lh-1")
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(&ledger, dir.path(), "w1:p1", Some(SOCKET), "{}"), None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_socketed_caller_matches_its_exact_socket_and_never_a_socketless_entry() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("aaa-old", "w1:p3", None),
            member("zzz-live", "w1:p3", Some(SOCKET)),
        ]);
        assert_eq!(
            run(&ledger, dir.path(), "w1:p3", Some(SOCKET), "{}").as_deref(),
            Some("zzz-live")
        );
        assert_eq!(
            run(&ledger, dir.path(), "w1:p3", Some("/run/other.sock"), "{}"),
            None
        );
    }

    fn in_session(session: Option<&str>) -> MemLedger {
        let mut m = member("lh-1", "w1:p1", Some(SOCKET));
        m.session_id = session.map(str::to_string);
        ledger(vec![m])
    }

    fn stop_from(session: &str) -> String {
        format!(r#"{{"hook_event_name":"Stop","session_id":"{session}"}}"#)
    }

    #[test]
    fn a_stop_from_the_members_own_session_records_it_with_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = in_session(Some("sess-a"));
        let who = run(
            &ledger,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            &stop_from("sess-a"),
        );
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert!(mailbox(dir.path(), "lh-1").contains(r#""session":"sess-a""#));
    }

    #[test]
    fn a_stop_from_another_session_records_nothing_and_leaves_the_ledger_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = in_session(Some("sess-a"));
        let before = ledger.0.clone();
        let who = run(
            &ledger,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            &stop_from("sess-b"),
        );
        assert_eq!(who, None);
        assert_eq!(mailbox(dir.path(), "lh-1"), "");
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn a_dropped_stop_says_which_member_and_both_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = in_session(Some("sess-a"));
        let mut warnings = vec![];
        run_as(
            &mut ledger,
            &FakeProcs::default(),
            None,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            &stop_from("sess-b"),
            &mut |w| warnings.push(w),
        );
        assert_eq!(
            warnings,
            ["dropped a stop from session sess-b in lh-1's pane; its session is sess-a"]
        );
    }

    #[test]
    fn an_entry_without_a_session_records_every_stop_and_stays_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = in_session(None);
        let who = run(
            &ledger,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            &stop_from("sess-b"),
        );
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert!(mailbox(dir.path(), "lh-1").contains(r#""session":"sess-b""#));
        assert_eq!(ledger.0["lh-1"].session_id, None);
    }

    #[test]
    fn a_stop_without_a_session_id_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = in_session(Some("sess-a"));
        let who = run(&ledger, dir.path(), "w1:p1", Some(SOCKET), "{}");
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert!(!mailbox(dir.path(), "lh-1").contains("session"));
    }

    fn two_claudes() -> FakeProcs {
        let mut p = FakeProcs::pane();
        p.add(40, "/bin/sh", 30, 400);
        p.add(50, CLAUDE, 40, 500);
        p
    }

    fn claimed(session: Option<&str>, pid: u32, start: u64) -> MemLedger {
        let mut l = in_session(session);
        l.0.get_mut("lh-1").unwrap().process = Some(Process { pid, start });
        l
    }

    fn from_pid(
        ledger: &mut MemLedger,
        pid: Option<u32>,
        input: &str,
    ) -> (Option<String>, Vec<String>, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut warnings = vec![];
        let who = run_as(
            ledger,
            &two_claudes(),
            pid,
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            input,
            &mut |w| warnings.push(w),
        );
        (who, warnings, mailbox(dir.path(), "lh-1"))
    }

    #[test]
    fn the_top_level_claude_claims_on_its_first_stop() {
        let mut l = in_session(Some("sess-a"));
        let (who, _, mailbox) = from_pid(&mut l, Some(30), &stop_from("sess-a"));
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert!(mailbox.contains("sess-a"));
        assert_eq!(
            l.0["lh-1"].process,
            Some(Process {
                pid: 30,
                start: 300
            })
        );
    }

    #[test]
    fn a_nested_claude_is_dropped_with_a_line_and_never_claims() {
        for session in ["sess-a", "sess-b"] {
            let mut l = in_session(Some("sess-a"));
            let (who, warnings, mailbox) = from_pid(&mut l, Some(50), &stop_from(session));
            assert_eq!(who, None, "{session}");
            assert_eq!(mailbox, "");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains("top-level"), "{warnings:?}");
            assert_eq!(l.0["lh-1"].process, None);
        }
    }

    #[test]
    fn a_nested_stop_after_the_members_process_died_is_dropped() {
        let mut l = claimed(Some("sess-a"), 30, 299);
        let (who, _, _) = from_pid(&mut l, Some(50), &stop_from("sess-a"));
        assert_eq!(who, None);
        assert_eq!(
            l.0["lh-1"].process,
            Some(Process {
                pid: 30,
                start: 299
            })
        );
    }

    #[test]
    fn a_nested_claude_reparented_to_init_is_dropped() {
        let mut l = in_session(Some("sess-a"));
        let mut procs = two_claudes();
        procs.add(60, CLAUDE, 1, 600);
        let dir = tempfile::tempdir().unwrap();
        let who = run_as(
            &mut l,
            &procs,
            Some(60),
            dir.path(),
            "w1:p1",
            Some(SOCKET),
            &stop_from("sess-a"),
            &mut |_| {},
        );
        assert_eq!(who, None);
        assert_eq!(l.0["lh-1"].process, None);
    }

    #[test]
    fn a_socketless_entry_is_filed_by_the_session_check_but_never_claimed() {
        let mut l = ledger(vec![member("lh-1", "w1:p1", None)]);
        l.0.get_mut("lh-1").unwrap().session_id = Some("sess-a".into());
        let dir = tempfile::tempdir().unwrap();
        let who = run_as(
            &mut l,
            &two_claudes(),
            Some(30),
            dir.path(),
            "w1:p1",
            None,
            &stop_from("sess-a"),
            &mut |_| {},
        );
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert_eq!(l.0["lh-1"].process, None);
    }

    fn continued(session: &str) -> String {
        match session {
            "stub" => "live".into(),
            other => other.into(),
        }
    }

    fn stop_continued(ledger: &mut dyn Ledger, input: &str) -> (Option<Stopped>, String) {
        stop_warned(ledger, input, &mut |_| {})
    }

    fn stop_warned(
        ledger: &mut dyn Ledger,
        input: &str,
        warn: &mut dyn FnMut(String),
    ) -> (Option<Stopped>, String) {
        let dir = tempfile::tempdir().unwrap();
        let pane = Pane {
            id: "w1:p1",
            socket: Some(Path::new(SOCKET)),
            claude_pid: None,
            lineage: &[],
        };
        let stopped = stop_hook(
            ledger,
            &FakeProcs::default(),
            dir.path(),
            &pane,
            input.as_bytes(),
            at(),
            &continued,
            &|_| Check::default(),
            warn,
        )
        .unwrap();
        (stopped, mailbox(dir.path(), "lh-1"))
    }

    #[test]
    fn a_member_whose_session_continued_is_moved_to_the_live_one_and_its_stop_filed() {
        for stop in ["stub", "live"] {
            let mut l = in_session(Some("stub"));
            let (stopped, mailbox) = stop_continued(&mut l, &stop_from(stop));
            assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())), "{stop}");
            assert!(mailbox.contains(r#""session":"live""#), "{stop}: {mailbox}");
            assert_eq!(l.0["lh-1"].session_id.as_deref(), Some("live"), "{stop}");
        }
    }

    #[test]
    fn a_member_already_moved_to_its_live_session_files_a_stop_its_hook_still_reports_as_the_stub()
    {
        let mut l = in_session(Some("live"));
        let (stopped, mailbox) = stop_continued(&mut l, &stop_from("stub"));
        assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())));
        assert!(mailbox.contains(r#""session":"live""#), "{mailbox}");
        assert_eq!(l.0["lh-1"].session_id.as_deref(), Some("live"));
    }

    #[test]
    fn a_failed_move_to_the_live_session_still_files_the_stop() {
        for stop in ["stub", "live"] {
            let mut l = NoUpdate(in_session(Some("stub")));
            let (stopped, mailbox) = stop_continued(&mut l, &stop_from(stop));
            assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())), "{stop}");
            assert!(mailbox.contains(r#""session":"live""#), "{stop}: {mailbox}");
            assert_eq!(l.0.0["lh-1"].session_id.as_deref(), Some("stub"), "{stop}");
        }
    }

    #[test]
    fn a_member_on_its_live_session_is_not_written() {
        let mut l = NoUpdate(in_session(Some("live")));
        let mut warnings = vec![];
        let (stopped, _) = stop_warned(&mut l, &stop_from("live"), &mut |w| warnings.push(w));
        assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_session_recorded_since_the_ledger_was_read_is_not_overwritten() {
        /// Reads the stub; updates see the session a SessionStart wrote since.
        struct Raced(MemLedger, MemLedger);
        impl Ledger for Raced {
            fn entries(&self) -> anyhow::Result<std::collections::BTreeMap<String, AgentRecord>> {
                self.0.entries()
            }
            fn record(&mut self, r: AgentRecord) -> anyhow::Result<()> {
                self.1.record(r)
            }
            fn update(
                &mut self,
                name: &str,
                change: &mut dyn FnMut(&mut AgentRecord) -> anyhow::Result<()>,
            ) -> anyhow::Result<AgentRecord> {
                self.1.update(name, change)
            }
            fn remove_herd(
                &mut self,
                _: &str,
                _: &mut dyn FnMut(&[String]) -> anyhow::Result<()>,
            ) -> anyhow::Result<crate::ledger::RemovedHerd> {
                unreachable!()
            }
            fn started(&mut self, n: &str, at: SystemTime, resumed: bool) -> anyhow::Result<()> {
                self.1.started(n, at, resumed)
            }
        }
        let mut l = Raced(in_session(Some("stub")), in_session(Some("fresh")));
        stop_continued(&mut l, &stop_from("fresh"));
        assert_eq!(l.1.0["lh-1"].session_id.as_deref(), Some("fresh"));
    }

    #[test]
    fn a_retired_member_recording_the_live_session_does_not_hold_it() {
        let mut l = in_session(Some("stub"));
        let mut other = member("lh-2", "w1:p2", Some(SOCKET));
        other.session_id = Some("live".into());
        other.retired = true;
        l.0.insert("lh-2".into(), other);
        let (stopped, _) = stop_continued(&mut l, &stop_from("stub"));
        assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())));
        assert_eq!(l.0["lh-1"].session_id.as_deref(), Some("live"));
    }

    #[test]
    fn a_continued_session_another_live_member_owns_is_not_followed() {
        let mut l = in_session(Some("stub"));
        let mut other = member("lh-2", "w1:p2", Some(SOCKET));
        other.session_id = Some("live".into());
        l.0.insert("lh-2".into(), other);
        let (stopped, mailbox) = stop_continued(&mut l, &stop_from("stub"));
        assert_eq!(l.0["lh-1"].session_id.as_deref(), Some("stub"));
        assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())));
        assert!(mailbox.contains(r#""session":"stub""#), "{mailbox}");
    }

    #[test]
    fn a_session_another_member_owns_is_not_followed() {
        let mut l = claimed(Some("old"), 30, 300);
        let mut other = member("lh-2", "w1:p2", Some(SOCKET));
        other.session_id = Some("theirs".into());
        l.0.insert("lh-2".into(), other);
        let (who, _, mailbox) = from_pid(&mut l, Some(30), &stop_from("theirs"));
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert!(mailbox.contains("theirs"));
        assert_eq!(l.0["lh-1"].session_id.as_deref(), Some("old"));
    }

    #[test]
    fn a_ledger_error_after_the_stop_is_filed_does_not_lose_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = NoUpdate(claimed(Some("old"), 30, 300));
        let mut warnings = vec![];
        let pane = Pane {
            id: "w1:p1",
            socket: Some(Path::new(SOCKET)),
            claude_pid: Some(30),
            lineage: &[],
        };
        let who = stop_hook(
            &mut l,
            &two_claudes(),
            dir.path(),
            &pane,
            stop_from("branched").as_bytes(),
            at(),
            &|s| s.to_string(),
            &|_| Check::default(),
            &mut |w| warnings.push(w),
        )
        .unwrap();
        assert_eq!(who, Some(Stopped::Filed("lh-1".into())));
        assert!(mailbox(dir.path(), "lh-1").contains("branched"));
        assert!(warnings[0].contains("disk full"), "{warnings:?}");
    }

    #[test]
    fn stops_from_the_members_process_are_filed_and_update_a_differing_session() {
        let mut l = claimed(Some("old"), 30, 300);
        let (who, warnings, mailbox) = from_pid(&mut l, Some(30), &stop_from("branched"));
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert!(warnings.is_empty());
        assert!(mailbox.contains(r#""session":"branched""#));
        assert_eq!(l.0["lh-1"].session_id.as_deref(), Some("branched"));
    }

    #[test]
    fn stops_from_another_process_are_dropped_with_a_line_even_in_the_same_session() {
        let mut l = claimed(Some("sess-a"), 30, 300);
        let (who, warnings, mailbox) = from_pid(&mut l, Some(50), &stop_from("sess-a"));
        assert_eq!(who, None);
        assert_eq!(mailbox, "");
        assert_eq!(
            warnings,
            ["dropped a stop from process 50 in lh-1's pane; its process is 30"]
        );
        assert_eq!(l.0["lh-1"].session_id.as_deref(), Some("sess-a"));
    }

    #[test]
    fn a_dead_recorded_process_is_reclaimed_by_the_next_stop() {
        let mut l = claimed(Some("sess-a"), 30, 299);
        let (who, _, _) = from_pid(&mut l, Some(30), &stop_from("sess-a"));
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert_eq!(
            l.0["lh-1"].process,
            Some(Process {
                pid: 30,
                start: 300
            })
        );
    }

    #[test]
    fn without_claude_pid_a_claimed_member_gets_the_session_check() {
        let mut l = claimed(Some("sess-a"), 30, 300);
        let (who, _, _) = from_pid(&mut l, None, &stop_from("sess-b"));
        assert_eq!(who, None);
        let (who, _, _) = from_pid(&mut l, None, &stop_from("sess-a"));
        assert_eq!(who.as_deref(), Some("lh-1"));
    }

    fn hook_with(
        ledger: &mut MemLedger,
        dir: &Path,
        pane: &str,
        pid: Option<u32>,
        input: &str,
        handoff: &dyn Fn(&AgentRecord) -> Check,
        warnings: &mut Vec<String>,
    ) -> Option<Stopped> {
        let pane = Pane {
            id: pane,
            socket: Some(Path::new(SOCKET)),
            claude_pid: pid,
            lineage: &[],
        };
        stop_hook(
            ledger,
            &two_claudes(),
            dir,
            &pane,
            input.as_bytes(),
            at(),
            &|s| s.to_string(),
            handoff,
            &mut |w| warnings.push(w),
        )
        .unwrap()
    }

    #[test]
    fn a_stop_over_the_context_limit_is_blocked_once_and_not_filed() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":150}}}"#,
        )
        .unwrap();
        let input = format!(
            r#"{{"hook_event_name":"Stop","session_id":"sess-a","transcript_path":"{}","background_tasks":[],"session_crons":[]}}"#,
            transcript.display()
        );
        let handoff = |m: &AgentRecord| {
            crate::context::handoff_block(m, input.as_bytes(), Ok(100), &state, &|_| Ok(()))
        };
        let mut l = claimed(Some("sess-a"), 30, 300);

        let first = hook_with(
            &mut l,
            dir.path(),
            "w1:p1",
            Some(30),
            &input,
            &handoff,
            &mut vec![],
        );
        let Some(Stopped::Blocked { name, reason }) = first else {
            panic!("{first:?}");
        };
        assert_eq!(name, "lh-1");
        assert!(
            reason.contains("151 tokens, past your limit of 100"),
            "{reason}"
        );
        assert!(reason.contains(&state.join("handoffs/lh-1/handoff.md").display().to_string()));
        assert_eq!(mailbox(dir.path(), "lh-1"), "");

        let second = hook_with(
            &mut l,
            dir.path(),
            "w1:p1",
            Some(30),
            &input,
            &handoff,
            &mut vec![],
        );
        assert_eq!(second, Some(Stopped::Filed("lh-1".into())));
        let filed = mailbox(dir.path(), "lh-1");
        assert!(filed.contains(r#""kind":"stop""#), "{filed}");
        assert!(filed.contains(r#""context":151,"limit":100"#), "{filed}");
    }

    #[test]
    fn a_failing_ask_still_files_the_stops_context_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let handoffs = state.join("handoffs");
        std::fs::create_dir_all(handoffs.join("lh-1/prev.md/x")).unwrap();
        std::fs::write(handoffs.join("lh-1/handoff.md"), "first").unwrap();
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":150}}}"#,
        )
        .unwrap();
        let input = format!(
            r#"{{"hook_event_name":"Stop","session_id":"sess-a","transcript_path":"{}","background_tasks":[],"session_crons":[]}}"#,
            transcript.display()
        );
        let handoff = |m: &AgentRecord| {
            crate::context::handoff_block(m, input.as_bytes(), Ok(100), &state, &|_| Ok(()))
        };
        let mut l = claimed(Some("sess-a"), 30, 300);
        let mut warnings = vec![];

        let stopped = hook_with(
            &mut l,
            dir.path(),
            "w1:p1",
            Some(30),
            &input,
            &handoff,
            &mut warnings,
        );

        assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("could not check lh-1's context: move"));
        let filed = mailbox(dir.path(), "lh-1");
        assert!(filed.contains(r#""context":151,"limit":100"#), "{filed}");
    }

    #[test]
    fn only_the_members_own_claude_is_asked_for_a_handoff() {
        let asked = std::cell::RefCell::new(vec![]);
        let handoff = |m: &AgentRecord| {
            asked.borrow_mut().push(m.name.clone());
            Check {
                reason: Ok(Some("hand off".into())),
                measured: None,
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let mut l = claimed(Some("sess-a"), 30, 300);
        let stop = stop_from("sess-a");
        assert_eq!(
            hook_with(
                &mut l,
                dir.path(),
                "w1:p9",
                Some(30),
                &stop,
                &handoff,
                &mut vec![]
            ),
            None
        );
        assert_eq!(
            hook_with(
                &mut l,
                dir.path(),
                "w1:p1",
                Some(50),
                &stop,
                &handoff,
                &mut vec![]
            ),
            None
        );
        assert!(asked.borrow().is_empty());
        assert!(matches!(
            hook_with(
                &mut l,
                dir.path(),
                "w1:p1",
                Some(30),
                &stop,
                &handoff,
                &mut vec![]
            ),
            Some(Stopped::Blocked { .. })
        ));
    }

    #[test]
    fn a_member_recorded_without_a_socket_is_never_asked_for_a_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = ledger(vec![member("lh-1", "w1:p1", None)]);
        let pane = Pane {
            id: "w1:p1",
            socket: None,
            claude_pid: None,
            lineage: &[],
        };
        let stopped = stop_hook(
            &mut l,
            &FakeProcs::default(),
            dir.path(),
            &pane,
            b"{}",
            at(),
            &|s| s.to_string(),
            &|_| panic!("asked a socketless member"),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())));
    }

    #[test]
    fn a_failing_context_check_files_the_stop_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = claimed(Some("sess-a"), 30, 300);
        let mut warnings = vec![];
        let stopped = hook_with(
            &mut l,
            dir.path(),
            "w1:p1",
            Some(30),
            &stop_from("sess-a"),
            &|_| Check {
                reason: Err(anyhow::anyhow!("config.toml: bad")),
                measured: None,
            },
            &mut warnings,
        );
        assert_eq!(stopped, Some(Stopped::Filed("lh-1".into())));
        assert_eq!(
            warnings,
            ["could not check lh-1's context: config.toml: bad"]
        );
        let filed = mailbox(dir.path(), "lh-1");
        assert!(
            !filed.contains("context") && !filed.contains("limit"),
            "{filed}"
        );
    }
}
