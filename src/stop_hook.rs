//! `loopherd stop-hook`: Claude's Stop and StopFailure hook. Each stop of a
//! member lands as a `stop` entry in its mailbox; anything else is ignored.

use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde_json::Value;

use crate::ledger::Ledger;
use crate::report::{Background, Body, Entry, Stop, Tally, Unknown, append, member_in};

/// Where the hook runs: `$HERDR_PANE_ID` and `$HERDR_SOCKET_PATH`.
pub struct Pane<'a> {
    pub id: &'a str,
    pub socket: Option<&'a Path>,
}

/// Records the stop `input` describes for the member in `pane`; returns the
/// member's name, or `None` when the pane is no member's or the stop came from
/// a session other than the member's. Never writes the ledger.
pub fn stop_hook(
    ledger: &dyn Ledger,
    dir: &Path,
    pane: &Pane,
    input: &[u8],
    now: SystemTime,
    warn: &mut dyn FnMut(String),
) -> Result<Option<String>> {
    let entries = ledger.entries()?;
    let Some(member) = member_in(&entries, pane.id, pane.socket) else {
        return Ok(None);
    };
    let stop = parse(input);
    if let (Some(ours), Some(theirs)) = (&member.session_id, &stop.session)
        && ours != theirs
    {
        warn(format!(
            "dropped a stop from session {theirs} in {}'s pane; its session is {ours}",
            member.name
        ));
        return Ok(None);
    }
    append(dir, &member.name, &Entry::new(now, Body::Stop(stop)))?;
    Ok(Some(member.name.clone()))
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
        session,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ledger::{AgentRecord, MemLedger};

    const SOCKET: &str = "/run/herdr/herdr.sock";

    fn member(name: &str, pane: &str, socket: Option<&str>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
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
            herd: None,
            template: None,
            params: None,
            socket: socket.map(PathBuf::from),
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
        let pane = Pane {
            id: pane,
            socket: socket.map(Path::new),
        };
        stop_hook(ledger, dir, &pane, input.as_bytes(), at(), &mut |_| {}).unwrap()
    }

    fn mailbox(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(format!("{name}.jsonl"))).unwrap_or_default()
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
    fn an_entry_recorded_without_a_socket_matches_on_its_pane_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w1:p1", None)]);
        assert_eq!(
            run(&ledger, dir.path(), "w1:p1", Some(SOCKET), "{}").as_deref(),
            Some("lh-1")
        );
    }

    #[test]
    fn an_exact_socket_match_beats_an_older_socketless_entry_on_the_same_pane() {
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
            run(&ledger, dir.path(), "w1:p3", Some("/run/other.sock"), "{}").as_deref(),
            Some("aaa-old")
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
        let ledger = in_session(Some("sess-a"));
        let pane = Pane {
            id: "w1:p1",
            socket: Some(Path::new(SOCKET)),
        };
        let mut warnings = vec![];
        stop_hook(
            &ledger,
            dir.path(),
            &pane,
            stop_from("sess-b").as_bytes(),
            at(),
            &mut |w| warnings.push(w),
        )
        .unwrap();
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
}
