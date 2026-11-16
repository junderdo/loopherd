//! Claude Code can move a conversation to a new session without running a
//! SessionStart hook; the old transcript then holds a `continued-in` record
//! naming the new session (D33).

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
/// another live member in `entries` records that one or the ledger's entry no
/// longer records the session `member` was read with; the entry as saved when
/// it tried.
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
        if record.session_id == member.session_id {
            record.session_id = Some(resolved.clone());
        }
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

/// Follows `transcript`'s `continued-in` records to the transcript its
/// conversation lives on in, looking in its own project first and then in the
/// others, and stopping at a missing transcript or [`MAX_HOPS`]. A cycle means
/// no continuation, so every session in it resolves to itself.
pub fn live_transcript(transcript: &Path) -> PathBuf {
    let mut current = transcript.to_path_buf();
    let mut seen = HashSet::from([session_in(&current)]);
    for _ in 0..MAX_HOPS {
        let Some(next) = continued_in(&current) else {
            break;
        };
        if !seen.insert(next.clone()) {
            return transcript.to_path_buf();
        }
        let sibling = current.with_file_name(format!("{next}.jsonl"));
        let found = match sibling.is_file() {
            true => Some(sibling),
            false => current
                .parent()
                .and_then(Path::parent)
                .and_then(|projects| in_projects(projects, &next)),
        };
        let Some(found) = found else {
            break;
        };
        current = found;
    }
    current
}

/// `projects/<project>/<session>.jsonl`, in whichever project holds it.
pub fn transcript_of(claude_dir: &Path, session: &str) -> Option<PathBuf> {
    in_projects(&claude_dir.join("projects"), session)
}

fn in_projects(projects: &Path, session: &str) -> Option<PathBuf> {
    if !is_plain_name(session) {
        return None;
    }
    let name = format!("{session}.jsonl");
    fs::read_dir(projects)
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
    let bytes = fs::read(transcript).ok()?;
    let text = String::from_utf8_lossy(&bytes);
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
            self.transcript_in("p", session, next)
        }

        fn transcript_in(&self, project: &str, session: &str, next: Option<&str>) -> PathBuf {
            let project = self.dir().join("projects").join(project);
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
    fn a_cycle_is_no_continuation_so_each_session_in_it_is_its_own_live_one() {
        let c = Claude::new();
        c.transcript("a", Some("b"));
        c.transcript("b", Some("a"));
        for x in ["a", "b"] {
            let live = live_session(c.dir(), x);
            assert_eq!(live, x);
            assert_eq!(live_session(c.dir(), &live), live);
        }
    }

    #[test]
    fn a_chain_is_followed_at_most_32_hops() {
        let c = Claude::new();
        for i in 0..40 {
            c.transcript(&format!("s{i}"), Some(&format!("s{}", i + 1)));
        }
        c.transcript("s40", None);
        assert_eq!(live_session(c.dir(), "s0"), "s32");
        assert_eq!(live_session(c.dir(), "s30"), "s40");
    }

    #[test]
    fn a_continuation_into_another_project_is_followed() {
        let c = Claude::new();
        c.transcript_in("p", "a", Some("b"));
        c.transcript_in("q", "b", None);
        assert_eq!(live_session(c.dir(), "a"), "b");
    }

    #[test]
    fn a_continuation_naming_a_path_is_ignored() {
        let c = Claude::new();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("x");
        std::fs::write(target.with_extension("jsonl"), "").unwrap();
        c.transcript("a", Some(target.to_str().unwrap()));
        assert_eq!(live_session(c.dir(), "a"), "a");
    }

    #[test]
    fn a_session_naming_a_path_has_no_transcript() {
        let c = Claude::new();
        c.transcript("a", None);
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("x");
        std::fs::write(target.with_extension("jsonl"), "").unwrap();
        assert_eq!(transcript_of(c.dir(), target.to_str().unwrap()), None);
    }

    #[test]
    fn a_transcript_with_invalid_utf8_is_still_followed() {
        let c = Claude::new();
        let a = c.transcript("a", Some("b"));
        c.transcript("b", None);
        let mut bytes = b"\xff\xfe not text\n".to_vec();
        bytes.extend(std::fs::read(&a).unwrap());
        std::fs::write(&a, bytes).unwrap();
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
