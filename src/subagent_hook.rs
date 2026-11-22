//! `loopherd subagent-hook`: Claude's SubagentStart and SubagentStop hook.
//! Each subagent a member's Claude starts or finishes lands in the member's
//! mailbox, so `view` and `status` can list what is running between stops.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use serde_json::Value;

use crate::continuation::{follow_recorded, live_for};
use crate::ledger::{AgentRecord, Ledger, Process};
use crate::report::{Body, Entry, Subagent, append, member_of_own_pane};

/// Where the hook runs: `$HERDR_PANE_ID` and `$HERDR_SOCKET_PATH`.
pub struct Pane<'a> {
    pub id: &'a str,
    pub socket: Option<&'a Path>,
    /// The caller and its ancestors, which carry a moved member's old pane id.
    pub lineage: &'a [Process],
}

/// Records the subagent start or stop `input` describes in the mailbox of the
/// member in `pane`; returns the member's name if it did.
pub fn subagent_hook(
    ledger: &mut dyn Ledger,
    dir: &Path,
    pane: &Pane,
    input: &[u8],
    now: SystemTime,
    live: &dyn Fn(&str) -> String,
) -> Result<Option<String>> {
    let Some((event, mut subagent)) = parse(input) else {
        return Ok(None);
    };
    let entries = ledger.entries()?;
    let Some(member) = member_of_own_pane(&entries, pane.id, pane.lineage, pane.socket) else {
        return Ok(None);
    };
    let member = match follow_recorded(ledger, &entries, member, live) {
        Ok(followed) => followed.unwrap_or_else(|| member.clone()),
        // Compared as if moved, so the subagent isn't dropped against a stale session.
        Err(_) => AgentRecord {
            session_id: member
                .session_id
                .as_deref()
                .map(|s| live_for(&entries, member, s, live)),
            ..member.clone()
        },
    };
    let session = subagent
        .session
        .as_deref()
        .map(|s| live_for(&entries, &member, s, live));
    if let (Some(ours), Some(theirs)) = (&member.session_id, &session)
        && ours != theirs
    {
        return Ok(None);
    }
    subagent.session = session;
    subagent.description = subagent.meta.as_deref().and_then(description);
    let body = match event {
        Event::Start => Body::SubagentStart(subagent),
        Event::Stop => Body::SubagentStop(subagent),
    };
    append(dir, &member.name, &Entry::new(now, body))?;
    Ok(Some(member.name))
}

/// The description in Claude's `meta.json` for a subagent, once it exists.
pub fn description(meta: &Path) -> Option<String> {
    let meta: Value = serde_json::from_slice(&fs::read(meta).ok()?).ok()?;
    let description = meta.get("description")?.as_str()?.trim();
    (!description.is_empty()).then(|| description.to_string())
}

enum Event {
    Start,
    Stop,
}

/// Skips Claude's internal agents, such as prompt suggestions, which have no type.
fn parse(input: &[u8]) -> Option<(Event, Subagent)> {
    let input: Value = serde_json::from_slice(input).ok()?;
    let field = |name| {
        input
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let event = match field("hook_event_name")? {
        "SubagentStart" => Event::Start,
        "SubagentStop" => Event::Stop,
        _ => return None,
    };
    let id = field("agent_id")?;
    let meta = field("transcript_path").map(|t| meta_path(Path::new(t), id));
    Some((
        event,
        Subagent {
            id: id.to_string(),
            agent_type: field("agent_type")?.to_string(),
            description: None,
            session: field("session_id").map(str::to_string),
            meta,
        },
    ))
}

/// Beside the session's transcript `<session>.jsonl`, Claude keeps
/// `<session>/subagents/agent-<id>.meta.json`.
fn meta_path(transcript: &Path, id: &str) -> PathBuf {
    let file = match id.starts_with("agent-") {
        true => format!("{id}.meta.json"),
        false => format!("agent-{id}.meta.json"),
    };
    transcript.with_extension("").join("subagents").join(file)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::*;
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::report::{Body, Subagent, history};

    const SOCKET: &str = "/run/herdr/herdr.sock";

    fn member(name: &str, pane: &str, session: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
            env_pane_id: None,
            env_pane_shell: None,
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
            outcome: None,
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: Some(SOCKET.into()),
            process: None,
            role: None,
            attended: None,
            no_slot: false,
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

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_000_000 + secs)
    }

    fn run(ledger: &mut MemLedger, dir: &Path, input: &str) -> Option<String> {
        let pane = Pane {
            id: "w1:p1",
            socket: Some(Path::new(SOCKET)),
            lineage: &[],
        };
        subagent_hook(ledger, dir, &pane, input.as_bytes(), at(0), &|s| {
            s.to_string()
        })
        .unwrap()
    }

    /// Claude's transcripts for `session` under a project dir in `claude`.
    fn transcript(claude: &Path, session: &str) -> PathBuf {
        let project = claude.join("projects/-src-repo");
        fs::create_dir_all(&project).unwrap();
        project.join(format!("{session}.jsonl"))
    }

    fn start_input(transcript: &Path, session: &str, id: &str, kind: &str) -> String {
        serde_json::json!({
            "session_id": session,
            "transcript_path": transcript,
            "cwd": "/src/repo",
            "hook_event_name": "SubagentStart",
            "agent_id": id,
            "agent_type": kind,
        })
        .to_string()
    }

    fn stop_input(transcript: &Path, session: &str, id: &str, kind: &str) -> String {
        let agent_transcript = transcript
            .with_extension("")
            .join(format!("subagents/agent-{id}.jsonl"));
        serde_json::json!({
            "session_id": session,
            "transcript_path": transcript,
            "cwd": "/src/repo",
            "permission_mode": "default",
            "hook_event_name": "SubagentStop",
            "stop_hook_active": false,
            "agent_id": id,
            "agent_type": kind,
            "agent_transcript_path": agent_transcript,
            "last_assistant_message": "Looks good.",
            "background_tasks": [],
            "session_crons": [],
        })
        .to_string()
    }

    fn write_meta(transcript: &Path, id: &str, description: &str) -> PathBuf {
        let dir = transcript.with_extension("").join("subagents");
        fs::create_dir_all(&dir).unwrap();
        let meta = dir.join(format!("agent-{id}.meta.json"));
        let json = serde_json::json!({"agentType": "reviewer", "description": description});
        fs::write(&meta, json.to_string()).unwrap();
        meta
    }

    fn bodies(dir: &Path, name: &str) -> Vec<Body> {
        let (entries, bad) = history(dir, name).unwrap();
        assert!(bad.is_empty(), "{bad:?}");
        entries.into_iter().map(|e| e.body).collect()
    }

    #[test]
    fn a_start_is_recorded_with_its_id_type_session_and_where_its_description_will_be() {
        let (claude, mail) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut l = ledger(vec![member("lh-1", "w1:p1", "s1")]);
        let t = transcript(claude.path(), "s1");

        let named = run(
            &mut l,
            mail.path(),
            &start_input(&t, "s1", "a1f", "reviewer"),
        );

        assert_eq!(named.as_deref(), Some("lh-1"));
        let meta = claude
            .path()
            .join("projects/-src-repo/s1/subagents/agent-a1f.meta.json");
        assert_eq!(
            bodies(mail.path(), "lh-1"),
            [Body::SubagentStart(Subagent {
                id: "a1f".into(),
                agent_type: "reviewer".into(),
                description: None,
                session: Some("s1".into()),
                meta: Some(meta),
            })]
        );
        let (entries, _) = history(mail.path(), "lh-1").unwrap();
        assert_eq!(
            entries[0].at,
            humantime::format_rfc3339_nanos(at(0)).to_string()
        );
    }

    #[test]
    fn a_stop_is_recorded_with_the_description_claude_wrote_for_it() {
        let (claude, mail) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut l = ledger(vec![member("lh-1", "w1:p1", "s1")]);
        let t = transcript(claude.path(), "s1");
        let meta = write_meta(&t, "a1f", "Review PR 325 round 1");

        run(
            &mut l,
            mail.path(),
            &stop_input(&t, "s1", "a1f", "reviewer"),
        );

        assert_eq!(
            bodies(mail.path(), "lh-1"),
            [Body::SubagentStop(Subagent {
                id: "a1f".into(),
                agent_type: "reviewer".into(),
                description: Some("Review PR 325 round 1".into()),
                session: Some("s1".into()),
                meta: Some(meta),
            })]
        );
    }

    #[test]
    fn outside_a_members_pane_or_session_nothing_is_recorded() {
        let (claude, mail) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut elsewhere = ledger(vec![member("lh-1", "w1:p9", "s1")]);
        let t = transcript(claude.path(), "s1");
        assert_eq!(
            run(
                &mut elsewhere,
                mail.path(),
                &start_input(&t, "s1", "a1", "reviewer")
            ),
            None
        );

        let mut nested = ledger(vec![member("lh-1", "w1:p1", "s1")]);
        let other = transcript(claude.path(), "s2");
        assert_eq!(
            run(
                &mut nested,
                mail.path(),
                &start_input(&other, "s2", "a2", "reviewer")
            ),
            None
        );
        assert_eq!(bodies(mail.path(), "lh-1"), []);
    }

    #[test]
    fn claudes_internal_agents_with_no_type_and_unparsable_input_are_ignored() {
        let (claude, mail) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut l = ledger(vec![member("lh-1", "w1:p1", "s1")]);
        let t = transcript(claude.path(), "s1");

        assert_eq!(
            run(&mut l, mail.path(), &stop_input(&t, "s1", "a3", "")),
            None
        );
        assert_eq!(run(&mut l, mail.path(), "not json"), None);
        assert_eq!(bodies(mail.path(), "lh-1"), []);
    }
}
