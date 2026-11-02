//! `loopherd session-hook`: Claude's SessionStart hook. A member that ran
//! `/clear` or was resumed has a new session; this keeps its ledger entry
//! pointing at it, so the stop hook still recognizes its stops.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::ledger::Ledger;

/// Where the hook runs: `$HERDR_PANE_ID` and `$HERDR_SOCKET_PATH`.
pub struct Pane<'a> {
    pub id: &'a str,
    pub socket: Option<&'a Path>,
}

/// Points the member in `pane` at the session `input` starts, when `input` is
/// a `clear` or `resume`; returns the member's name if it did. A `startup`
/// (a nested `claude -p` among them) or `compact` changes nothing, and so does
/// a pane matched without an exact socket or a session another live member owns.
pub fn session_hook(ledger: &mut dyn Ledger, pane: &Pane, input: &[u8]) -> Result<Option<String>> {
    let Some(session) = new_session(input) else {
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
    if live().any(|e| e.session_id.as_deref() == Some(session.as_str())) {
        return Ok(None);
    }
    ledger.update(&member.name, &mut |record| {
        record.session_id = Some(session.clone());
        Ok(())
    })?;
    Ok(Some(member.name.clone()))
}

fn new_session(input: &[u8]) -> Option<String> {
    let input: Value = serde_json::from_slice(input).ok()?;
    let source = input.get("source")?.as_str()?;
    if !matches!(source, "clear" | "resume") {
        return None;
    }
    let session = input.get("session_id")?.as_str()?;
    (!session.is_empty()).then(|| session.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::ledger::{AgentRecord, FileLedger, MemLedger};

    const SOCKET: &str = "/run/herdr/herdr.sock";

    fn member(name: &str, pane: &str, socket: Option<&str>, session: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
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

    fn run(ledger: &mut dyn Ledger, socket: Option<&str>, input: &str) -> Option<String> {
        let pane = Pane {
            id: "w1:p1",
            socket: socket.map(Path::new),
        };
        session_hook(ledger, &pane, input.as_bytes()).unwrap()
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
            fn started(&mut self, name: &str, at: std::time::SystemTime) -> anyhow::Result<()> {
                self.0.started(name, at)
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
