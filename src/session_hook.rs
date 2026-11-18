//! `loopherd session-hook`: Claude's SessionStart hook. A member's Claude moves
//! to a new session on `/clear`, `/branch` or a resume; this keeps its ledger
//! entry pointing at it, so the stop hook still recognizes its stops.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::continuation::{follow_recorded, live_for};
use crate::ledger::Ledger;
use crate::process::{self, Claim, Procs};

/// Where the hook runs: `$HERDR_PANE_ID`, `$HERDR_SOCKET_PATH` and `$CLAUDE_PID`.
pub struct Pane<'a> {
    pub id: &'a str,
    pub socket: Option<&'a Path>,
    pub claude_pid: Option<u32>,
}

/// Points the member in `pane` at the session `input` starts; returns the
/// member's name if it did. Once the member's Claude process is known, its
/// session changes are adopted whatever the source and other processes' are
/// not; until then only `clear` and `resume` are. A pane matched without an
/// exact socket, or a session another live member owns, changes nothing.
/// Sessions are taken as `live_session` resolves them, and a recorded one
/// that resolves elsewhere is moved there first.
pub fn session_hook(
    ledger: &mut dyn Ledger,
    procs: &dyn Procs,
    pane: &Pane,
    input: &[u8],
    live_session: &dyn Fn(&str) -> String,
    warn: &mut dyn FnMut(String),
) -> Result<Option<String>> {
    let Some((source, session)) = parse(input) else {
        return Ok(None);
    };
    let Some(socket) = pane.socket else {
        return Ok(None);
    };
    let entries = ledger.entries()?;
    let live = || entries.values().filter(|e| !e.retired);
    let Some(member) = live().find(|e| e.pane_id == pane.id && e.socket.as_deref() == Some(socket))
    else {
        return Ok(None);
    };
    let followed = follow_recorded(ledger, &entries, member, live_session)?;
    let member = followed.as_ref().unwrap_or(member);
    let session = live_for(&entries, member, &session, live_session);
    if live().any(|e| e.name != member.name && e.session_id.as_deref() == Some(session.as_str())) {
        return Ok(None);
    }
    let from_clear_or_resume = matches!(source.as_str(), "clear" | "resume");
    let adopt = match pane.claude_pid {
        Some(pid) if process::live(procs, member.process).is_some() => {
            if member.process.map(|p| p.pid) != Some(pid) {
                warn(format!(
                    "ignored a session start from process {pid} in {}'s pane; its process is {}",
                    member.name,
                    member.process.map_or(0, |p| p.pid)
                ));
                return Ok(None);
            }
            true
        }
        Some(pid) => match process::claim(ledger, procs, member, true, pid, Some(&session))? {
            Claim::Claimed => true,
            Claim::Nested => {
                warn(format!(
                    "ignored a session start from process {pid} in {}'s pane; it is not the pane's top-level Claude",
                    member.name
                ));
                return Ok(None);
            }
            Claim::Declined => from_clear_or_resume,
        },
        None => from_clear_or_resume,
    };
    if !adopt || member.session_id.as_deref() == Some(session.as_str()) {
        return Ok(adopt.then(|| member.name.clone()));
    }
    ledger.update(&member.name, &mut |record| {
        record.session_id = Some(session.clone());
        Ok(())
    })?;
    Ok(Some(member.name.clone()))
}

fn parse(input: &[u8]) -> Option<(String, String)> {
    let input: Value = serde_json::from_slice(input).ok()?;
    let source = input.get("source")?.as_str()?;
    let session = input.get("session_id")?.as_str()?;
    (!session.is_empty()).then(|| (source.to_string(), session.to_string()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ledger::{AgentRecord, FileLedger, MemLedger, Process};
    use crate::process::{CLAUDE, FakeProcs};

    const SOCKET: &str = "/run/herdr/herdr.sock";

    fn member(name: &str, pane: &str, socket: Option<&str>, session: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
            tab_id: None,
            session_id: Some(session.into()),
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
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: socket.map(PathBuf::from),
            process: None,
            role: None,
            after_merge: None,
            earlier: vec![],
        }
    }

    fn ledger(members: Vec<AgentRecord>) -> MemLedger {
        MemLedger(
            members.into_iter().map(|m| (m.name.clone(), m)).collect(),
            vec![],
        )
    }

    fn run(ledger: &mut dyn Ledger, socket: Option<&str>, input: &str) -> Option<String> {
        run_as(ledger, &FakeProcs::default(), None, socket, input)
    }

    fn run_as(
        ledger: &mut dyn Ledger,
        procs: &FakeProcs,
        claude_pid: Option<u32>,
        socket: Option<&str>,
        input: &str,
    ) -> Option<String> {
        let pane = Pane {
            id: "w1:p1",
            socket: socket.map(Path::new),
            claude_pid,
        };
        session_hook(
            ledger,
            procs,
            &pane,
            input.as_bytes(),
            &|s| s.to_string(),
            &mut |_| {},
        )
        .unwrap()
    }

    fn continued(session: &str) -> String {
        match session {
            "stub" => "live".into(),
            other => other.into(),
        }
    }

    fn run_continued(ledger: &mut MemLedger, input: &str) -> Option<String> {
        let pane = Pane {
            id: "w1:p1",
            socket: Some(Path::new(SOCKET)),
            claude_pid: None,
        };
        let procs = FakeProcs::default();
        session_hook(
            ledger,
            &procs,
            &pane,
            input.as_bytes(),
            &continued,
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn a_recorded_session_that_continued_is_moved_to_the_live_one_on_any_start() {
        let mut l = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "stub")]);
        run_continued(&mut l, &start("startup", "unrelated"));
        assert_eq!(session_of(&l, "lh-1").as_deref(), Some("live"));
    }

    #[test]
    fn resuming_a_continued_session_records_the_live_one() {
        let mut l = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old")]);
        assert_eq!(
            run_continued(&mut l, &start("resume", "stub")).as_deref(),
            Some("lh-1")
        );
        assert_eq!(session_of(&l, "lh-1").as_deref(), Some("live"));
    }

    #[test]
    fn a_member_already_moved_to_its_live_session_keeps_it_when_a_start_reports_the_stub() {
        let mut l = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "live")]);
        assert_eq!(
            run_continued(&mut l, &start("resume", "stub")).as_deref(),
            Some("lh-1")
        );
        assert_eq!(session_of(&l, "lh-1").as_deref(), Some("live"));
    }

    #[test]
    fn resuming_a_session_whose_continuation_another_member_owns_keeps_it() {
        let mut l = ledger(vec![
            member("lh-1", "w1:p1", Some(SOCKET), "stub"),
            member("lh-2", "w1:p2", Some(SOCKET), "live"),
        ]);
        assert_eq!(
            run_continued(&mut l, &start("resume", "stub")).as_deref(),
            Some("lh-1")
        );
        assert_eq!(session_of(&l, "lh-1").as_deref(), Some("stub"));
    }

    #[test]
    fn a_continued_session_another_live_member_owns_is_not_followed() {
        let mut l = ledger(vec![
            member("lh-1", "w1:p1", Some(SOCKET), "stub"),
            member("lh-2", "w1:p2", Some(SOCKET), "live"),
        ]);
        run_continued(&mut l, &start("startup", "unrelated"));
        assert_eq!(session_of(&l, "lh-1").as_deref(), Some("stub"));
    }

    fn start(source: &str, session: &str) -> String {
        format!(
            r#"{{"hook_event_name":"SessionStart","source":"{source}","session_id":"{session}"}}"#
        )
    }

    fn session_of(ledger: &MemLedger, name: &str) -> Option<String> {
        ledger.0[name].session_id.clone()
    }

    #[test]
    fn clear_and_resume_adopt_the_new_session() {
        for source in ["clear", "resume"] {
            let mut ledger = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old")]);
            let who = run(&mut ledger, Some(SOCKET), &start(source, "new"));
            assert_eq!(who.as_deref(), Some("lh-1"), "{source}");
            assert_eq!(
                session_of(&ledger, "lh-1").as_deref(),
                Some("new"),
                "{source}"
            );
        }
    }

    #[test]
    fn startup_and_compact_change_nothing() {
        for source in ["startup", "compact", "other"] {
            let mut ledger = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old")]);
            assert_eq!(run(&mut ledger, Some(SOCKET), &start(source, "new")), None);
            assert_eq!(
                session_of(&ledger, "lh-1").as_deref(),
                Some("old"),
                "{source}"
            );
        }
    }

    #[test]
    fn unusable_input_changes_nothing() {
        for input in [
            "not json",
            "{}",
            r#"{"source":"clear"}"#,
            r#"{"source":"clear","session_id":""}"#,
        ] {
            let mut ledger = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old")]);
            assert_eq!(run(&mut ledger, Some(SOCKET), input), None, "{input}");
            assert_eq!(session_of(&ledger, "lh-1").as_deref(), Some("old"));
        }
    }

    #[test]
    fn a_socketless_entry_or_another_herdr_is_not_adopted() {
        let mut ledger = ledger(vec![member("lh-1", "w1:p1", None, "old")]);
        assert_eq!(run(&mut ledger, Some(SOCKET), &start("clear", "new")), None);
        assert_eq!(session_of(&ledger, "lh-1").as_deref(), Some("old"));

        let mut ledger = self::ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old")]);
        assert_eq!(
            run(&mut ledger, Some("/run/other.sock"), &start("clear", "new")),
            None
        );
        assert_eq!(run(&mut ledger, None, &start("clear", "new")), None);
        assert_eq!(session_of(&ledger, "lh-1").as_deref(), Some("old"));
    }

    #[test]
    fn a_session_another_live_member_owns_is_not_adopted() {
        let mut ledger = ledger(vec![
            member("lh-1", "w1:p1", Some(SOCKET), "old"),
            member("lh-2", "w1:p2", Some(SOCKET), "theirs"),
        ]);
        assert_eq!(
            run(&mut ledger, Some(SOCKET), &start("resume", "theirs")),
            None
        );
        assert_eq!(session_of(&ledger, "lh-1").as_deref(), Some("old"));
    }

    #[test]
    fn a_session_only_a_retired_member_had_can_be_adopted() {
        let mut gone = member("lh-2", "w1:p2", Some(SOCKET), "theirs");
        gone.retired = true;
        let mut ledger = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old"), gone]);
        assert!(run(&mut ledger, Some(SOCKET), &start("resume", "theirs")).is_some());
        assert_eq!(session_of(&ledger, "lh-1").as_deref(), Some("theirs"));
    }

    #[test]
    fn a_retired_member_or_an_unknown_pane_is_not_adopted() {
        let mut retired = member("lh-1", "w1:p1", Some(SOCKET), "old");
        retired.retired = true;
        let mut ledger = ledger(vec![retired, member("lh-3", "w1:p3", Some(SOCKET), "x")]);
        assert_eq!(run(&mut ledger, Some(SOCKET), &start("clear", "new")), None);
        assert_eq!(session_of(&ledger, "lh-1").as_deref(), Some("old"));
    }

    fn claimed(mut m: AgentRecord, pid: u32, start: u64) -> AgentRecord {
        m.process = Some(Process { pid, start });
        m
    }

    fn two_claudes() -> FakeProcs {
        let mut p = FakeProcs::pane();
        p.add(40, "/bin/sh", 30, 400);
        p.add(50, CLAUDE, 40, 500);
        p
    }

    #[test]
    fn the_top_level_member_claims_its_process_and_keeps_its_session() {
        let mut l = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "s1")]);
        let who = run_as(
            &mut l,
            &two_claudes(),
            Some(30),
            Some(SOCKET),
            &start("startup", "s1"),
        );
        assert_eq!(who.as_deref(), Some("lh-1"));
        assert_eq!(
            l.0["lh-1"].process,
            Some(Process {
                pid: 30,
                start: 300
            })
        );
        assert_eq!(session_of(&l, "lh-1").as_deref(), Some("s1"));
    }

    #[test]
    fn a_member_without_a_session_claims_and_takes_the_hooks() {
        let mut m = member("lh-1", "w1:p1", Some(SOCKET), "x");
        m.session_id = None;
        let mut l = ledger(vec![m]);
        run_as(
            &mut l,
            &two_claudes(),
            Some(30),
            Some(SOCKET),
            &start("startup", "s1"),
        );
        assert_eq!(
            l.0["lh-1"].process,
            Some(Process {
                pid: 30,
                start: 300
            })
        );
        assert_eq!(session_of(&l, "lh-1").as_deref(), Some("s1"));
    }

    #[test]
    fn a_nested_claude_a_socket_mismatch_and_a_session_mismatch_do_not_claim() {
        let procs = two_claudes();
        let cases = [
            (50, Some(SOCKET), Some(SOCKET), "s1"),
            (30, Some(SOCKET), Some("/run/other.sock"), "s1"),
            (30, Some(SOCKET), None, "s1"),
            (30, Some(SOCKET), Some(SOCKET), "unrelated"),
        ];
        for (pid, recorded, hook, session) in cases {
            let mut l = ledger(vec![member("lh-1", "w1:p1", recorded, "s1")]);
            run_as(&mut l, &procs, Some(pid), hook, &start("startup", session));
            assert_eq!(l.0["lh-1"].process, None, "{pid} {hook:?} {session}");
        }
    }

    #[test]
    fn a_socketless_entry_is_never_claimed() {
        let mut l = ledger(vec![member("lh-1", "w1:p1", None, "s1")]);
        run_as(
            &mut l,
            &two_claudes(),
            Some(30),
            Some(SOCKET),
            &start("startup", "s1"),
        );
        assert_eq!(l.0["lh-1"].process, None);
    }

    #[test]
    fn a_dead_recorded_process_is_reclaimed() {
        let mut l = ledger(vec![claimed(
            member("lh-1", "w1:p1", Some(SOCKET), "s1"),
            30,
            299,
        )]);
        run_as(
            &mut l,
            &two_claudes(),
            Some(30),
            Some(SOCKET),
            &start("resume", "s1"),
        );
        assert_eq!(
            l.0["lh-1"].process,
            Some(Process {
                pid: 30,
                start: 300
            })
        );
    }

    #[test]
    fn a_fork_from_the_members_process_is_adopted() {
        for source in ["fork", "startup", "clear", "resume"] {
            let mut l = ledger(vec![claimed(
                member("lh-1", "w1:p1", Some(SOCKET), "old"),
                30,
                300,
            )]);
            let who = run_as(
                &mut l,
                &two_claudes(),
                Some(30),
                Some(SOCKET),
                &start(source, "new"),
            );
            assert_eq!(who.as_deref(), Some("lh-1"), "{source}");
            assert_eq!(session_of(&l, "lh-1").as_deref(), Some("new"), "{source}");
        }
    }

    #[test]
    fn a_fork_or_resume_from_another_process_is_not_adopted() {
        for source in ["fork", "resume", "clear", "startup"] {
            let mut l = ledger(vec![claimed(
                member("lh-1", "w1:p1", Some(SOCKET), "old"),
                30,
                300,
            )]);
            let who = run_as(
                &mut l,
                &two_claudes(),
                Some(50),
                Some(SOCKET),
                &start(source, "new"),
            );
            assert_eq!(who, None, "{source}");
            assert_eq!(session_of(&l, "lh-1").as_deref(), Some("old"), "{source}");
        }
    }

    #[test]
    fn a_nested_clear_or_resume_is_not_adopted_by_an_unclaimed_member() {
        for source in ["clear", "resume", "fork"] {
            let mut l = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old")]);
            let who = run_as(
                &mut l,
                &two_claudes(),
                Some(50),
                Some(SOCKET),
                &start(source, "new"),
            );
            assert_eq!(who, None, "{source}");
            assert_eq!(session_of(&l, "lh-1").as_deref(), Some("old"), "{source}");
            assert_eq!(l.0["lh-1"].process, None);
        }
    }

    #[test]
    fn a_nested_claude_reparented_to_init_does_not_claim() {
        let mut procs = FakeProcs::pane();
        procs.add(60, CLAUDE, 1, 600);
        let mut l = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "s1")]);
        run_as(
            &mut l,
            &procs,
            Some(60),
            Some(SOCKET),
            &start("resume", "s1"),
        );
        assert_eq!(l.0["lh-1"].process, None);
    }

    #[test]
    fn a_dropped_session_start_says_why() {
        let procs = two_claudes();
        let pane = Pane {
            id: "w1:p1",
            socket: Some(Path::new(SOCKET)),
            claude_pid: Some(50),
        };
        let mut l = ledger(vec![claimed(
            member("lh-1", "w1:p1", Some(SOCKET), "s"),
            30,
            300,
        )]);
        let mut said = vec![];
        session_hook(
            &mut l,
            &procs,
            &pane,
            start("fork", "n").as_bytes(),
            &|s| s.to_string(),
            &mut |w| said.push(w),
        )
        .unwrap();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("process 50") && said[0].contains("lh-1"));
    }

    #[test]
    fn a_fork_without_claude_pid_is_not_adopted() {
        let mut l = ledger(vec![member("lh-1", "w1:p1", Some(SOCKET), "old")]);
        assert_eq!(run(&mut l, Some(SOCKET), &start("fork", "new")), None);
    }

    #[test]
    fn a_nested_resume_of_the_members_own_session_is_not_a_change() {
        let mut l = ledger(vec![claimed(
            member("lh-1", "w1:p1", Some(SOCKET), "s1"),
            30,
            300,
        )]);
        let who = run_as(
            &mut l,
            &two_claudes(),
            Some(50),
            Some(SOCKET),
            &start("resume", "s1"),
        );
        assert_eq!(who, None);
    }

    #[test]
    fn the_update_keeps_a_write_made_between_the_read_and_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.json");
        let mut file = FileLedger::new(path.clone());
        file.record(member("lh-1", "w1:p1", Some(SOCKET), "old"))
            .unwrap();

        struct RetiresAfterRead(FileLedger, FileLedger);
        impl Ledger for RetiresAfterRead {
            fn entries(&self) -> anyhow::Result<std::collections::BTreeMap<String, AgentRecord>> {
                let read = self.0.entries()?;
                let mut other = FileLedger::new(self.1.path().to_path_buf());
                other.update("lh-1", &mut |r| {
                    r.retired = true;
                    Ok(())
                })?;
                Ok(read)
            }
            fn record(&mut self, record: AgentRecord) -> anyhow::Result<()> {
                self.0.record(record)
            }
            fn update(
                &mut self,
                name: &str,
                change: &mut dyn FnMut(&mut AgentRecord) -> anyhow::Result<()>,
            ) -> anyhow::Result<AgentRecord> {
                self.0.update(name, change)
            }
            fn remove_herd(
                &mut self,
                _: &str,
                _: &mut dyn FnMut(&[String]) -> anyhow::Result<()>,
            ) -> anyhow::Result<crate::ledger::RemovedHerd> {
                unreachable!()
            }
            fn started(
                &mut self,
                name: &str,
                at: std::time::SystemTime,
                resumed: bool,
            ) -> anyhow::Result<()> {
                self.0.started(name, at, resumed)
            }
        }

        let mut racing =
            RetiresAfterRead(FileLedger::new(path.clone()), FileLedger::new(path.clone()));
        assert!(run(&mut racing, Some(SOCKET), &start("clear", "new")).is_some());

        let saved = FileLedger::new(path).entries().unwrap()["lh-1"].clone();
        assert!(saved.retired, "the concurrent retire was overwritten");
        assert_eq!(saved.session_id.as_deref(), Some("new"));
    }
}
