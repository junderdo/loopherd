//! A member's context limit, and the handoff its Stop hook asks for once the
//! member's context is past it (D20).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::{self, ConfigFile};
use crate::continuation::{is_plain_name, live_transcript, session_in, transcript_of};
use crate::ledger::AgentRecord;
use crate::process::{self, Procs};

/// The shepherd's handoff file's name; no member may take it.
pub const SHEPHERD: &str = "shepherd";
pub const DEFAULT_LIMIT: u64 = 250_000;
pub const DEFAULT_WINDOW: u64 = 200_000;
/// The smallest limit or window a config accepts; below it a member would
/// hand off before doing any work.
pub const MIN_TOKENS: u64 = 10_000;
/// The limit when a member's own can't be read: [`DEFAULT_LIMIT`] capped at
/// 80% of [`DEFAULT_WINDOW`].
const FALLBACK_LIMIT: u64 = if DEFAULT_LIMIT < DEFAULT_WINDOW / 5 * 4 {
    DEFAULT_LIMIT
} else {
    DEFAULT_WINDOW / 5 * 4
};

/// `own` (a template's or `[lead]`'s `context_limit`), else `config`'s
/// `default_context_limit`, else [`DEFAULT_LIMIT`]; never over 80% of
/// `config`'s `context_window`, else of [`DEFAULT_WINDOW`].
pub fn effective_limit(own: Option<u64>, config: &Path) -> Result<u64> {
    Ok(config::read(config)?.limit(own))
}

/// The shepherd's limit: `config`'s `[shepherd] context_limit` in place of a
/// member's own, resolved as [`effective_limit`].
pub fn shepherd_limit(config: &Path) -> Result<u64> {
    let file = config::read(config)?;
    let own = file.shepherd.as_ref().and_then(|s| s.context_limit);
    Ok(file.limit(own))
}

impl ConfigFile {
    fn limit(&self, own: Option<u64>) -> u64 {
        let limit = own.or(self.default_context_limit).unwrap_or(DEFAULT_LIMIT);
        let window = self.context_window.unwrap_or(DEFAULT_WINDOW);
        limit.min(window / 5 * 4)
    }
}

/// Fails when `value` is under [`MIN_TOKENS`], naming `key`.
pub fn check_tokens(key: &str, value: Option<u64>) -> Result<()> {
    match value {
        Some(v) if v < MIN_TOKENS => anyhow::bail!("{key} must be at least {MIN_TOKENS}, got {v}"),
        _ => Ok(()),
    }
}

/// Where `name` writes its handoff, in loopherd's state dir `state`.
pub fn handoff_path(state: &Path, name: &str) -> PathBuf {
    state.join("handoffs").join(format!("{name}.md"))
}

/// The reason to block the stop `input` describes, when `member`'s context
/// is over `limit`: once per session, never on a stop a Stop hook already
/// continued. Before asking, moves any handoff from last time to
/// `<name>.prev.md`. A failed `limit` falls back to the built-in one and is
/// passed to `note` once per session.
pub fn handoff_block(
    member: &AgentRecord,
    input: &[u8],
    limit: Result<u64>,
    state: &Path,
    note: &dyn Fn(String) -> Result<()>,
) -> Result<Option<String>> {
    let Ok(input) = serde_json::from_slice::<Value>(input) else {
        return Ok(None);
    };
    if input["hook_event_name"] != "Stop" || input["stop_hook_active"] == true {
        return Ok(None);
    }
    let Some(session) = input["session_id"].as_str().filter(|s| is_plain_name(s)) else {
        return Ok(None);
    };
    let live = input["transcript_path"]
        .as_str()
        .map(|t| live_transcript(Path::new(t)));
    let session = match &live {
        Some(live) if input["transcript_path"].as_str() != live.to_str() => session_in(live),
        _ => session.to_string(),
    };
    let session = session.as_str();
    let (limit, noted) = match limit {
        Ok(limit) => (limit, Ok(())),
        Err(e) => (FALLBACK_LIMIT, note_once(state, session, &e, note)),
    };
    // A failed note must not skip the check; it surfaces only when nothing is asked.
    let asked = match &live {
        Some(transcript) => ask(member, transcript, session, limit, state)?,
        None => None,
    };
    match asked {
        Some(reason) => Ok(Some(reason)),
        None => noted.map(|()| None),
    }
}

fn ask(
    member: &AgentRecord,
    transcript: &Path,
    session: &str,
    limit: u64,
    state: &Path,
) -> Result<Option<String>> {
    let Some(tokens) = tokens_in(transcript).filter(|&t| t > limit) else {
        return Ok(None);
    };
    let asked = state.join("handoff-asked");
    let marker = asked.join(session);
    if marker.exists() {
        return Ok(None);
    }
    // Rotate before marking, so a failed rotation leaves the ask for the next stop.
    let handoff = handoff_path(state, &member.name);
    if handoff.exists() {
        let prev = handoff.with_extension("prev.md");
        fs::rename(&handoff, &prev)
            .with_context(|| format!("move {} to {}", handoff.display(), prev.display()))?;
    }
    fs::create_dir_all(&asked).with_context(|| format!("create {}", asked.display()))?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("create {}", marker.display())),
    }
    Ok(Some(reason(member, tokens, limit, &handoff)))
}

fn note_once(
    state: &Path,
    session: &str,
    error: &anyhow::Error,
    note: &dyn Fn(String) -> Result<()>,
) -> Result<()> {
    let noted = state.join("limit-noted");
    fs::create_dir_all(&noted).with_context(|| format!("create {}", noted.display()))?;
    let marker = noted.join(session);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("create {}", marker.display())),
    }
    let error = format!("{error:#}");
    let error: Vec<&str> = error
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !is_snippet(l))
        .collect();
    let text = format!(
        "could not read my context limit, so it is {FALLBACK_LIMIT} until fixed: {}",
        error.join(" ")
    );
    note(text).inspect_err(|_| {
        // Unmarked, so the next stop tries the note again.
        let _ = fs::remove_file(&marker);
    })
}

/// A line of a TOML error's source excerpt: `|`, `1 | repo =`, `|   ^`.
fn is_snippet(line: &str) -> bool {
    let gutter = line.trim_start_matches(|c: char| c.is_ascii_digit());
    gutter.starts_with('|') || gutter.starts_with(" |")
}

fn reason(member: &AgentRecord, tokens: u64, limit: u64, handoff: &Path) -> String {
    // A lead's watch is a background shell; left running, its stop reads
    // paused and restart refuses it.
    let (save, plan) = match member.is_lead() {
        true => (
            "stop your own `loopherd watch` (TaskStop it, or kill it) so your stop reads finished, then ",
            "your plan as next steps, re-arming your watch first",
        ),
        false => (
            "commit and push your branch, comment your progress on your task in the project's tracker, then ",
            "your plan as next steps",
        ),
    };
    format!(
        "loopherd: your context is at {tokens} tokens, past your limit of {limit}. \
         Stop your current work: {save}write your handoff to {} (your state, {plan}, \
         gotchas; nothing already in the tracker or the ledger), run \
         `loopherd report --kind handoff \"<one line on where you are>\"`, and end your turn. \
         Whoever reads your reports restarts you fresh from that file; don't restart yourself.",
        handoff.display()
    )
}

/// What the shepherd's watch needs to find its caller's Claude session.
pub struct ShepherdCheck<'a> {
    pub procs: &'a dyn Procs,
    /// `$CLAUDE_PID`, counted only when it is an ancestor of `me`.
    pub claude_pid: Option<&'a str>,
    pub me: u32,
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude_dir: &'a Path,
    pub state: &'a Path,
    pub limit: u64,
}

/// The line telling the shepherd to hand off, due until [`Self::mark`].
pub struct ShepherdHandoff {
    pub line: String,
    /// The Claude session measured.
    pub session: String,
    marker: PathBuf,
}

impl ShepherdHandoff {
    #[cfg(test)]
    pub fn new(line: &str, session: &str, marker: PathBuf) -> ShepherdHandoff {
        ShepherdHandoff {
            line: line.into(),
            session: session.into(),
            marker,
        }
    }

    /// Remembers the session was told, so no later watch tells it again.
    pub fn mark(self) -> Result<()> {
        let dir = self.marker.parent().expect("a marker is in handoff-asked");
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        fs::write(&self.marker, "").with_context(|| format!("create {}", self.marker.display()))
    }
}

/// `shepherd handoff <n>k over <limit>k` when the caller's Claude session is
/// past `limit` and hasn't been told yet; nothing when the session or its
/// transcript can't be found.
pub fn shepherd_handoff(check: &ShepherdCheck) -> Option<ShepherdHandoff> {
    let pid = process::claude_pid(check.procs, check.claude_pid, check.me)?;
    let session = session_of(check.procs, pid, check.claude_dir)?;
    let marker = check.state.join("handoff-asked").join(&session);
    if marker.exists() {
        return None;
    }
    let tokens = tokens_in(&transcript_of(check.claude_dir, &session)?)?;
    (tokens > check.limit).then(|| ShepherdHandoff {
        line: format!(
            "shepherd handoff {}k over {}k",
            tokens / 1000,
            check.limit / 1000
        ),
        session,
        marker,
    })
}

/// The session in Claude's `sessions/<pid>.json`, which follows `/clear`; its
/// `procStart` must be `pid`'s, so a reused PID's stale file doesn't count.
fn session_of(procs: &dyn Procs, pid: u32, claude_dir: &Path) -> Option<String> {
    let path = claude_dir.join("sessions").join(format!("{pid}.json"));
    let file: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let start = procs.start(pid)?.to_string();
    if file["procStart"].as_str() != Some(start.as_str()) {
        return None;
    }
    let session = file["sessionId"].as_str()?;
    is_plain_name(session).then(|| session.to_string())
}

/// Tokens in context as of the transcript's last main-thread assistant turn
/// with any usage: its input, cache-creation and cache-read tokens.
fn tokens_in(transcript: &Path) -> Option<u64> {
    let text = fs::read_to_string(transcript).ok()?;
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|r| r["type"] == "assistant" && r["isSidechain"] != true)
        .find_map(|r| {
            let usage = r.pointer("/message/usage")?;
            let count = |key: &str| usage[key].as_u64().unwrap_or(0);
            let tokens = count("input_tokens")
                + count("cache_creation_input_tokens")
                + count("cache_read_input_tokens");
            // A zero is a placeholder record, not a turn the API answered.
            (tokens > 0).then_some(tokens)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Role;

    fn config(text: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        if let Some(text) = text {
            fs::write(&path, text).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn without_a_config_file_the_limit_is_its_own_capped_at_80_percent_of_200k() {
        let (_dir, path) = config(None);
        assert_eq!(effective_limit(Some(100_000), &path).unwrap(), 100_000);
        assert_eq!(effective_limit(Some(190_000), &path).unwrap(), 160_000);
        assert_eq!(effective_limit(None, &path).unwrap(), 160_000);
    }

    #[test]
    fn the_config_default_applies_only_without_an_own_limit() {
        let (_dir, path) = config(Some("default_context_limit = 90000\n"));
        assert_eq!(effective_limit(None, &path).unwrap(), 90_000);
        assert_eq!(effective_limit(Some(120_000), &path).unwrap(), 120_000);
    }

    #[test]
    fn the_built_in_limit_applies_under_a_large_window() {
        let (_dir, path) = config(Some("context_window = 1000000\n"));
        assert_eq!(effective_limit(None, &path).unwrap(), 250_000);
        assert_eq!(effective_limit(Some(900_000), &path).unwrap(), 800_000);
    }

    #[test]
    fn an_invalid_config_file_is_an_error_naming_it() {
        for text in [
            "context_windw = 1",
            "context_window = \"big\"",
            "= =",
            "context_window = 0",
            "default_context_limit = 9999",
        ] {
            let (_dir, path) = config(Some(text));
            let err = format!("{:#}", effective_limit(None, &path).unwrap_err());
            assert!(
                err.starts_with(&path.display().to_string()),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn the_shepherds_limit_is_its_own_else_the_default_capped_at_80_percent() {
        let (_dir, path) = config(None);
        assert_eq!(shepherd_limit(&path).unwrap(), 160_000);
        let (_dir, path) = config(Some("[shepherd]\ncontext_limit = 120000\n"));
        assert_eq!(shepherd_limit(&path).unwrap(), 120_000);
        let (_dir, path) = config(Some(
            "default_context_limit = 90000\ncontext_window = 1000000\n",
        ));
        assert_eq!(shepherd_limit(&path).unwrap(), 90_000);
        let (_dir, path) = config(Some(
            "default_context_limit = 90000\ncontext_window = 1000000\n[shepherd]\ncontext_limit = 900000\n",
        ));
        assert_eq!(shepherd_limit(&path).unwrap(), 800_000);
    }

    #[test]
    fn a_members_limit_ignores_the_shepherds() {
        let (_dir, path) = config(Some("[shepherd]\ncontext_limit = 120000\n"));
        assert_eq!(effective_limit(None, &path).unwrap(), 160_000);
    }

    #[test]
    fn an_invalid_shepherd_section_is_an_error_naming_the_file() {
        for text in [
            "[shepherd]\ncontext_limt = 120000\n",
            "[shepherd]\ncontext_limit = 9999\n",
            "shepherd = 1\n",
        ] {
            let (_dir, path) = config(Some(text));
            for err in [
                shepherd_limit(&path).unwrap_err(),
                effective_limit(None, &path).unwrap_err(),
            ] {
                let err = format!("{err:#}");
                assert!(
                    err.starts_with(&path.display().to_string()),
                    "{text}: {err}"
                );
            }
        }
        let (_dir, path) = config(Some("[shepherd]\ncontext_limit = 9999\n"));
        let err = format!("{:#}", shepherd_limit(&path).unwrap_err());
        assert!(
            err.ends_with("shepherd.context_limit must be at least 10000, got 9999"),
            "{err}"
        );
    }

    fn member(name: &str, role: Option<Role>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
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
            socket: None,
            process: None,
            role,
            after_merge: None,
        }
    }

    fn assistant(input: u64, creation: u64, read: u64, sidechain: bool) -> String {
        serde_json::json!({
            "type": "assistant",
            "isSidechain": sidechain,
            "message": {"usage": {
                "input_tokens": input,
                "cache_creation_input_tokens": creation,
                "cache_read_input_tokens": read,
                "output_tokens": 999_999,
            }},
        })
        .to_string()
    }

    struct World {
        dir: tempfile::TempDir,
    }

    impl World {
        fn new() -> World {
            World {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn transcript(&self, lines: &[String]) -> PathBuf {
            let path = self.dir.path().join("t.jsonl");
            fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        }

        fn stop(&self, session: &str, transcript: &Path, extra: &str) -> Vec<u8> {
            format!(
                r#"{{"hook_event_name":"Stop","session_id":"{session}","transcript_path":"{}"{extra}}}"#,
                transcript.display()
            )
            .into_bytes()
        }

        fn block(&self, who: &AgentRecord, input: &[u8], limit: u64) -> Option<String> {
            handoff_block(who, input, Ok(limit), &self.state(), &|_| Ok(())).unwrap()
        }
    }

    #[test]
    fn over_the_limit_the_first_stop_of_a_session_is_blocked_with_the_handoff_steps() {
        let w = World::new();
        let t = w.transcript(&[
            assistant(10, 20, 100_000, false),
            r#"{"type":"user"}"#.into(),
        ]);
        let reason = w
            .block(&member("lh-1", None), &w.stop("s1", &t, ""), 100_000)
            .unwrap();
        let handoff = w.state().join("handoffs/lh-1.md");
        assert_eq!(
            reason,
            format!(
                "loopherd: your context is at 100030 tokens, past your limit of 100000. \
                 Stop your current work: commit and push your branch, comment your progress on \
                 your task in the project's tracker, then write your handoff to {} (your state, \
                 your plan as next steps, gotchas; nothing already in the tracker or the ledger), \
                 run `loopherd report --kind handoff \"<one line on where you are>\"`, and end \
                 your turn. Whoever reads your reports restarts you fresh from that file; don't \
                 restart yourself.",
                handoff.display()
            )
        );
    }

    #[test]
    fn a_continued_session_is_measured_and_asked_once_in_the_session_it_continued_in() {
        let w = World::new();
        let stub = w.dir.path().join("stub.jsonl");
        fs::write(
            &stub,
            r#"{"type":"continued-in","sessionId":"stub","continuedInSessionId":"live"}"#,
        )
        .unwrap();
        fs::write(
            w.dir.path().join("live.jsonl"),
            assistant(0, 0, 195_000, false),
        )
        .unwrap();
        let lead = member("lh-1", None);
        assert!(
            w.block(&lead, &w.stop("stub", &stub, ""), 150_000)
                .is_some()
        );
        assert!(w.state().join("handoff-asked/live").exists());
        assert_eq!(w.block(&lead, &w.stop("live", &stub, ""), 150_000), None);
    }

    #[test]
    fn a_leads_reason_has_it_stop_its_watch_not_push_a_branch() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let reason = w
            .block(
                &member("lead-cards", Some(Role::Lead)),
                &w.stop("s1", &t, ""),
                100,
            )
            .unwrap();
        assert!(
            reason.contains(
                "Stop your current work: stop your own `loopherd watch` (TaskStop it, or kill it) \
                 so your stop reads finished, then write your handoff to"
            ),
            "{reason}"
        );
        assert!(!reason.contains("branch"), "{reason}");
        assert!(
            reason.contains("your plan as next steps, re-arming your watch first"),
            "{reason}"
        );
    }

    #[test]
    fn a_session_is_blocked_only_once_and_a_new_session_again() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let who = member("lh-1", None);
        assert!(w.block(&who, &w.stop("s1", &t, ""), 100).is_some());
        assert_eq!(w.block(&who, &w.stop("s1", &t, ""), 100), None);
        assert!(w.block(&who, &w.stop("s2", &t, ""), 100).is_some());
    }

    #[test]
    fn at_or_under_the_limit_nothing_is_blocked_or_remembered() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 100, false)]);
        let who = member("lh-1", None);
        assert_eq!(w.block(&who, &w.stop("s1", &t, ""), 100), None);
        assert!(w.block(&who, &w.stop("s1", &t, ""), 99).is_some());
    }

    #[test]
    fn a_stop_a_hook_already_continued_is_never_blocked() {
        let w = World::new();
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let who = member("lh-1", None);
        let active = w.stop("s1", &t, r#","stop_hook_active":true"#);
        assert_eq!(w.block(&who, &active, 100), None);
        assert!(w.block(&who, &w.stop("s1", &t, ""), 100).is_some());
    }

    #[test]
    fn the_measure_is_the_last_main_thread_assistant_turn() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[
            assistant(0, 0, 500, false),
            assistant(0, 0, 50, false),
            assistant(0, 0, 900, true),
        ]);
        assert_eq!(w.block(&who, &w.stop("s1", &t, ""), 100), None);
        let t = w.transcript(&[assistant(0, 0, 50, false), assistant(0, 0, 500, false)]);
        assert!(w.block(&who, &w.stop("s2", &t, ""), 100).is_some());
        let t = w.transcript(&[assistant(0, 0, 500, false), assistant(0, 0, 0, false)]);
        assert!(w.block(&who, &w.stop("s3", &t, ""), 100).is_some());
    }

    #[test]
    fn unusable_input_or_transcript_blocks_nothing() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 500, false)]);
        let missing = w.dir.path().join("missing.jsonl");
        let empty = w.transcript(&[]);
        for input in [
            b"not json".to_vec(),
            format!(
                r#"{{"hook_event_name":"Stop","transcript_path":"{}"}}"#,
                t.display()
            )
            .into_bytes(),
            br#"{"hook_event_name":"Stop","session_id":"s1"}"#.to_vec(),
            format!(
                r#"{{"hook_event_name":"StopFailure","session_id":"s1","transcript_path":"{}"}}"#,
                t.display()
            )
            .into_bytes(),
            w.stop("s1", &missing, ""),
            w.stop("s1", &empty, ""),
        ] {
            assert_eq!(
                w.block(&who, &input, 100),
                None,
                "{}",
                String::from_utf8_lossy(&input)
            );
        }
    }

    #[test]
    fn asking_again_keeps_the_last_handoff_as_prev() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let handoffs = w.state().join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        fs::write(handoffs.join("lh-1.md"), "first").unwrap();
        w.block(&who, &w.stop("s1", &t, ""), 100).unwrap();
        assert!(!handoffs.join("lh-1.md").exists());
        assert_eq!(
            fs::read_to_string(handoffs.join("lh-1.prev.md")).unwrap(),
            "first"
        );
    }

    #[test]
    fn a_failed_rotation_leaves_the_ask_for_the_next_stop() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        let handoffs = w.state().join("handoffs");
        fs::create_dir_all(handoffs.join("lh-1.prev.md/x")).unwrap();
        fs::write(handoffs.join("lh-1.md"), "first").unwrap();
        assert!(
            handoff_block(
                &who,
                &w.stop("s1", &t, ""),
                Ok(100),
                &w.state(),
                &|_| Ok(())
            )
            .is_err()
        );
        fs::remove_dir_all(handoffs.join("lh-1.prev.md")).unwrap();
        assert!(w.block(&who, &w.stop("s1", &t, ""), 100).is_some());
    }

    #[test]
    fn a_failed_limit_still_blocks_when_its_note_fails_and_the_next_stop_notes_again() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 160_001, false)]);
        let broken = || Err(anyhow::anyhow!("config.toml: TOML parse error"));
        let reason = handoff_block(&who, &w.stop("s1", &t, ""), broken(), &w.state(), &|_| {
            anyhow::bail!("mailbox full")
        })
        .unwrap()
        .unwrap();
        assert!(reason.contains("past your limit of 160000"), "{reason}");

        let notes = std::cell::RefCell::new(vec![]);
        let note = |text: String| {
            notes.borrow_mut().push(text);
            Ok(())
        };
        let t = w.transcript(&[assistant(0, 0, 100, false)]);
        for _ in 0..2 {
            let stop = w.stop("s1", &t, "");
            assert_eq!(
                handoff_block(&who, &stop, broken(), &w.state(), &note).unwrap(),
                None
            );
        }
        assert_eq!(
            *notes.borrow(),
            [
                "could not read my context limit, so it is 160000 until fixed: config.toml: TOML parse error"
            ]
        );
    }

    #[test]
    fn a_failing_note_under_the_limit_is_the_checks_error() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 100, false)]);
        let err = handoff_block(
            &who,
            &w.stop("s1", &t, ""),
            Err(anyhow::anyhow!("bad")),
            &w.state(),
            &|_| anyhow::bail!("mailbox full"),
        )
        .unwrap_err();
        assert_eq!(format!("{err:#}"), "mailbox full");
    }

    #[test]
    fn a_failed_limit_is_noted_even_without_a_transcript() {
        let w = World::new();
        let noted = std::cell::RefCell::new(vec![]);
        let input = br#"{"hook_event_name":"Stop","session_id":"s1"}"#;
        let blocked = handoff_block(
            &member("lh-1", None),
            input,
            Err(anyhow::anyhow!("bad")),
            &w.state(),
            &|text| {
                noted.borrow_mut().push(text);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(blocked, None);
        assert_eq!(noted.borrow().len(), 1);
    }

    #[test]
    fn a_session_id_that_isnt_a_plain_file_name_blocks_nothing() {
        let w = World::new();
        let who = member("lh-1", None);
        let t = w.transcript(&[assistant(0, 0, 200, false)]);
        for session in ["", "../x", "/tmp/x", ".hidden"] {
            assert_eq!(
                w.block(&who, &w.stop(session, &t, ""), 100),
                None,
                "{session}"
            );
        }
        assert!(!w.state().join("x").exists());
    }
}

#[cfg(test)]
mod shepherd_tests {
    use super::*;
    use crate::process::FakeProcs;

    const SESSION: &str = "d7acc609-7ff5-45ff-84c9-c4b088fb954e";

    /// The shepherd's Claude (30, started at 300) in a herdr pane, running
    /// `sh` (35) running the watch (36), with its session file and transcript.
    struct Shepherd {
        dir: tempfile::TempDir,
        procs: FakeProcs,
    }

    impl Shepherd {
        fn new(tokens: u64) -> Shepherd {
            let mut procs = FakeProcs::pane();
            procs.add(35, "/bin/sh", 30, 350);
            procs.add(36, "/usr/bin/loopherd", 35, 360);
            let s = Shepherd {
                dir: tempfile::tempdir().unwrap(),
                procs,
            };
            s.session_file(30, SESSION, "300");
            s.transcript(SESSION, tokens);
            s
        }

        fn claude(&self) -> PathBuf {
            self.dir.path().join("claude")
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn session_file(&self, pid: u32, session: &str, start: &str) {
            let sessions = self.claude().join("sessions");
            fs::create_dir_all(&sessions).unwrap();
            let body = serde_json::json!({"pid": pid, "sessionId": session, "procStart": start});
            fs::write(sessions.join(format!("{pid}.json")), body.to_string()).unwrap();
        }

        fn transcript(&self, session: &str, tokens: u64) {
            let project = self.claude().join("projects/-home-u-repo");
            fs::create_dir_all(&project).unwrap();
            let line = serde_json::json!({
                "type": "assistant",
                "message": {"usage": {"input_tokens": 5, "cache_read_input_tokens": tokens - 5}},
            });
            fs::write(
                project.join(format!("{session}.jsonl")),
                format!("{line}\n"),
            )
            .unwrap();
        }

        fn check_from(&self, env: Option<&str>, limit: u64) -> Option<String> {
            let handoff = shepherd_handoff(&ShepherdCheck {
                procs: &self.procs,
                claude_pid: env,
                me: 36,
                claude_dir: &self.claude(),
                state: &self.state(),
                limit,
            })?;
            let line = handoff.line.clone();
            handoff.mark().unwrap();
            Some(line)
        }

        fn check(&self, limit: u64) -> Option<String> {
            self.check_from(Some("30"), limit)
        }
    }

    #[test]
    fn over_the_limit_it_tells_the_shepherd_to_hand_off_once_per_session() {
        let s = Shepherd::new(171_400);
        assert_eq!(
            s.check(160_000).as_deref(),
            Some("shepherd handoff 171k over 160k")
        );
        assert_eq!(s.check(160_000), None);
        s.session_file(30, "after-clear", "300");
        s.transcript("after-clear", 171_400);
        assert!(s.check(160_000).is_some());
    }

    #[test]
    fn at_or_under_the_limit_it_says_nothing_and_remembers_nothing() {
        let s = Shepherd::new(160_000);
        assert_eq!(s.check(160_000), None);
        assert!(s.check(159_999).is_some());
    }

    #[test]
    fn an_unmarked_line_comes_back() {
        let s = Shepherd::new(200_000);
        let (claude, state) = (s.claude(), s.state());
        let check = ShepherdCheck {
            procs: &s.procs,
            claude_pid: Some("30"),
            me: 36,
            claude_dir: &claude,
            state: &state,
            limit: 100_000,
        };
        assert!(shepherd_handoff(&check).is_some());
        assert!(shepherd_handoff(&check).is_some());
    }

    #[test]
    fn a_session_or_transcript_it_cant_find_says_nothing() {
        let s = Shepherd::new(200_000);
        assert_eq!(s.check_from(None, 100), None);
        assert_eq!(s.check_from(Some("x"), 100), None);
        // A Claude that isn't the watch's ancestor.
        let mut other = Shepherd::new(200_000);
        other.procs.add(99, crate::process::CLAUDE, 1, 990);
        other.session_file(99, SESSION, "990");
        assert_eq!(other.check_from(Some("99"), 100), None);

        let reused_pid = Shepherd::new(200_000);
        reused_pid.session_file(30, SESSION, "301");
        assert_eq!(reused_pid.check(100), None);

        let no_session_file = Shepherd::new(200_000);
        fs::remove_file(no_session_file.claude().join("sessions/30.json")).unwrap();
        assert_eq!(no_session_file.check(100), None);

        let no_transcript = Shepherd::new(200_000);
        no_transcript.session_file(30, "fresh", "300");
        assert_eq!(no_transcript.check(100), None);

        for session in ["", "../x", ".hidden"] {
            let odd = Shepherd::new(200_000);
            odd.session_file(30, session, "300");
            assert_eq!(odd.check(100), None, "{session}");
        }
    }
}
