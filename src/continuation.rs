//! Claude Code can move a conversation to a new session without running a
//! SessionStart hook; the old transcript then holds a `continued-in` record
//! naming the new session (D31).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::ledger::{AgentRecord, Ledger};

/// More hops than any real chain, so a runaway one still ends.
const MAX_HOPS: usize = 32;

/// The session `session` lives on in, found among Claude's transcripts under
/// `claude_dir`; `session` itself when its transcript isn't there.
pub fn live_session(claude_dir: &Path, session: &str) -> String {
    transcript_of(claude_dir, session)
        .map(|t| session_in(&live_transcript(&t)))
        .unwrap_or_else(|| session.to_string())
}

/// Moves `member`'s recorded session to the one `live` resolves it to, unless
/// another live member in `entries` records that one; the updated record when
/// it moved.
pub fn follow_recorded(
    ledger: &mut dyn Ledger,
    entries: &BTreeMap<String, AgentRecord>,
    member: &AgentRecord,
    live: &dyn Fn(&str) -> String,
) -> Result<Option<AgentRecord>> {
    let Some(recorded) = &member.session_id else {
        return Ok(None);
    };
    let resolved = live_for(entries, member, recorded, live);
    if &resolved == recorded {
        return Ok(None);
    }
    let updated = ledger.update(&member.name, &mut |record| {
        record.session_id = Some(resolved.clone());
        Ok(())
    })?;
    Ok(Some(updated))
}

/// `session` as `live` resolves it, unless another live member than `member`
/// in `entries` records that one.
pub fn live_for(
    entries: &BTreeMap<String, AgentRecord>,
    member: &AgentRecord,
    session: &str,
    live: &dyn Fn(&str) -> String,
) -> String {
    let resolved = live(session);
    let owned = entries.values().any(|e| {
        !e.retired && e.name != member.name && e.session_id.as_deref() == Some(resolved.as_str())
    });
    match owned {
        true => session.to_string(),
        false => resolved,
    }
}

/// Follows `transcript`'s `continued-in` records to the sibling transcript
/// its conversation lives on in, stopping at a missing transcript, a cycle,
/// or [`MAX_HOPS`].
pub fn live_transcript(transcript: &Path) -> PathBuf {
    let mut current = transcript.to_path_buf();
    let mut seen = HashSet::from([session_in(&current)]);
    for _ in 0..MAX_HOPS {
        let Some(next) = continued_in(&current)
            .filter(|next| seen.insert(next.clone()))
            .map(|next| current.with_file_name(format!("{next}.jsonl")))
            .filter(|next| next.is_file())
        else {
            break;
        };
        current = next;
    }
    current
}

/// `projects/<project>/<session>.jsonl`, in whichever project holds it.
pub fn transcript_of(claude_dir: &Path, session: &str) -> Option<PathBuf> {
    if !is_plain_name(session) {
        return None;
    }
    let name = format!("{session}.jsonl");
    fs::read_dir(claude_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|project| project.path().join(&name))
        .find(|path| path.is_file())
}

/// The session a transcript records: its file name without `.jsonl`.
pub fn session_in(transcript: &Path) -> String {
    transcript
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The session the last `continued-in` record names, when it is a plain file
/// name and no turn follows it: a resumed session goes on in its own transcript.
fn continued_in(transcript: &Path) -> Option<String> {
    let text = fs::read_to_string(transcript).ok()?;
    let last = text
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|r| {
            matches!(
                r["type"].as_str(),
                Some("continued-in" | "user" | "assistant")
            )
        })?;
    (last["type"] == "continued-in")
        .then(|| last["continuedInSessionId"].as_str())?
        .filter(|s| is_plain_name(s))
        .map(str::to_string)
}

pub fn is_plain_name(session: &str) -> bool {
    !(session.is_empty() || session.contains('/') || session.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Claude(tempfile::TempDir);

    impl Claude {
        fn new() -> Claude {
            Claude(tempfile::tempdir().unwrap())
        }

        fn dir(&self) -> &Path {
            self.0.path()
        }

        /// A transcript for `session` in project `p`, continued in `next` if given.
        fn transcript(&self, session: &str, next: Option<&str>) -> PathBuf {
            let project = self.dir().join("projects").join("p");
            std::fs::create_dir_all(&project).unwrap();
            let mut lines = vec![format!(
                r#"{{"type":"user","sessionId":"{session}","message":{{"role":"user","content":"hi"}}}}"#
            )];
            if let Some(next) = next {
                lines.push(format!(
                    r#"{{"type":"continued-in","sessionId":"{session}","continuedInSessionId":"{next}"}}"#
                ));
            }
            lines.push(format!(
                r#"{{"type":"cost-state","sessionId":"{session}"}}"#
            ));
            let path = project.join(format!("{session}.jsonl"));
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        }
    }

    #[test]
    fn a_session_without_a_continued_in_record_is_its_own_live_session() {
        let c = Claude::new();
        c.transcript("a", None);
        assert_eq!(live_session(c.dir(), "a"), "a");
    }

    #[test]
    fn a_continued_session_resolves_to_the_session_it_continued_in() {
        let c = Claude::new();
        c.transcript("a", Some("b"));
        c.transcript("b", None);
        assert_eq!(live_session(c.dir(), "a"), "b");
    }

    #[test]
    fn a_chain_of_continuations_is_followed_to_its_end() {
        let c = Claude::new();
        c.transcript("a", Some("b"));
        c.transcript("b", Some("c"));
        c.transcript("c", None);
        assert_eq!(live_session(c.dir(), "a"), "c");
    }

    #[test]
    fn a_missing_target_transcript_stops_the_chain_at_the_last_session_found() {
        let c = Claude::new();
        c.transcript("a", Some("b"));
        c.transcript("b", Some("gone"));
        assert_eq!(live_session(c.dir(), "a"), "b");
    }

    #[test]
    fn a_cycle_stops_before_revisiting_a_session() {
        let c = Claude::new();
        c.transcript("a", Some("b"));
        c.transcript("b", Some("a"));
        assert_eq!(live_session(c.dir(), "a"), "b");
    }

    #[test]
    fn an_unknown_session_or_one_that_isnt_a_plain_name_is_unchanged() {
        let c = Claude::new();
        c.transcript("a", Some("../x"));
        assert_eq!(live_session(c.dir(), "nope"), "nope");
        assert_eq!(live_session(c.dir(), "a"), "a");
    }

    #[test]
    fn a_session_resumed_after_its_continued_in_record_is_still_live() {
        let c = Claude::new();
        let a = c.transcript("a", Some("b"));
        c.transcript("b", None);
        let mut text = std::fs::read_to_string(&a).unwrap();
        text.push_str(r#"{"type":"user","sessionId":"a"}"#);
        text.push('\n');
        std::fs::write(&a, text).unwrap();
        assert_eq!(live_session(c.dir(), "a"), "a");
    }

    #[test]
    fn a_transcript_resolves_to_the_live_ones_path() {
        let c = Claude::new();
        let a = c.transcript("a", Some("b"));
        let b = c.transcript("b", None);
        assert_eq!(live_transcript(&a), b);
        assert_eq!(live_transcript(&b), b);
    }
}
