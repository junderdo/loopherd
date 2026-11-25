//! A lead's question or gate request waits for the next prompt in its pane.
//! An `.awaiting-<pane>` mark in the mailboxes directory names the lead, so
//! a prompt in an unmarked pane costs one file check.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::report::{Body, Entry, Kind, append, history};

/// What a lead has waited on since its last `answered` or `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wait {
    /// The oldest waiting entry's kind: `Question` or `Gate`.
    pub kind: Kind,
    pub since: SystemTime,
}

/// Marks `pane` as the lead `lead` waiting for an answer there.
pub fn mark(dir: &Path, pane: &str, lead: &str) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = mark_path(dir, pane);
    let mut file = crate::pane_label::open_regular(&path)?;
    file.set_len(0)
        .and_then(|()| file.write_all(lead.as_bytes()))
        .with_context(|| format!("write {}", path.display()))
}

/// Answers the lead marked on `pane` for the prompt in a UserPromptSubmit
/// hook's `input`, unless that prompt is a finished background task's.
pub fn prompted(dir: &Path, pane: &str, input: &[u8], now: SystemTime) -> Result<()> {
    let path = mark_path(dir, pane);
    let lead = match fs::read_to_string(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        read => read.with_context(|| format!("read {}", path.display()))?,
    };
    let input: Option<serde_json::Value> = serde_json::from_slice(input).ok();
    let prompt = input.as_ref().and_then(|i| i["prompt"].as_str());
    if prompt.is_some_and(crate::pane_label::is_background_event) {
        return Ok(());
    }
    // The lead can write this directory, so its mark mustn't reach another path.
    crate::spawn::validate_name(&lead).with_context(|| format!("{}", path.display()))?;
    append(dir, &lead, &Entry::new(now, Body::Answered {}))?;
    forget(&path)
}

/// Removes `pane`'s mark when it names `lead`; a warning line if that fails.
pub fn unmark(dir: &Path, pane: &str, lead: &str) -> Option<String> {
    let path = mark_path(dir, pane);
    if fs::read_to_string(&path).ok()? != lead {
        return None;
    }
    forget(&path).err().map(|e| format!("warning: {e:#}"))
}

/// The lead's oldest question or gate request after its last `answered` or `start`.
pub fn wait(dir: &Path, lead: &str) -> Option<Wait> {
    let (entries, _) = history(dir, lead).ok()?;
    let mut oldest = None;
    for entry in &entries {
        match &entry.body {
            Body::Answered {} | Body::Start { .. } => oldest = None,
            Body::Question { .. } | Body::Gate { .. } if oldest.is_none() => oldest = Some(entry),
            _ => {}
        }
    }
    let (kind, _) = oldest?.report()?;
    let since = humantime::parse_rfc3339_weak(&oldest?.at).ok()?;
    Some(Wait { kind, since })
}

fn forget(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        removed => removed.with_context(|| format!("remove {}", path.display())),
    }
}

fn mark_path(dir: &Path, pane: &str) -> PathBuf {
    dir.join(format!(".awaiting-{pane}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Body, Entry, append, history};

    fn at(s: &str) -> SystemTime {
        humantime::parse_rfc3339(s).unwrap()
    }

    fn file(dir: &Path, kind: Kind, text: &str, when: &str) {
        append(
            dir,
            "lead-h",
            &Entry::new(at(when), Body::report(kind, text)),
        )
        .unwrap();
        mark(dir, "p-lead", "lead-h").unwrap();
    }

    fn prompt(dir: &Path, text: &str, when: &str) {
        let input = serde_json::json!({ "prompt": text }).to_string();
        prompted(dir, "p-lead", input.as_bytes(), at(when)).unwrap();
    }

    const NOTIFICATION: &str = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";

    #[test]
    fn a_gate_request_waits_until_a_prompt_in_the_lead_s_pane_answers_it() {
        let dir = tempfile::tempdir().unwrap();
        file(
            dir.path(),
            Kind::Gate,
            "Gate request: #9 at abc123",
            "2026-11-24T23:22:00Z",
        );

        assert_eq!(
            wait(dir.path(), "lead-h"),
            Some(Wait {
                kind: Kind::Gate,
                since: at("2026-11-24T23:22:00Z")
            })
        );

        prompt(dir.path(), "gate #9 passed", "2026-11-24T23:40:00Z");

        assert_eq!(wait(dir.path(), "lead-h"), None);
        let (entries, _) = history(dir.path(), "lead-h").unwrap();
        assert_eq!(
            entries.last(),
            Some(&Entry::new(at("2026-11-24T23:40:00Z"), Body::Answered {}))
        );
    }

    #[test]
    fn a_prompt_that_is_one_task_notification_leaves_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        file(
            dir.path(),
            Kind::Gate,
            "Gate request: #9",
            "2026-11-24T23:22:00Z",
        );

        prompt(dir.path(), NOTIFICATION, "2026-11-24T23:40:00Z");

        assert_eq!(wait(dir.path(), "lead-h").map(|w| w.kind), Some(Kind::Gate));
        prompt(dir.path(), "go on", "2026-11-24T23:41:00Z");
        assert_eq!(wait(dir.path(), "lead-h"), None);
    }

    #[test]
    fn one_prompt_answers_every_question_waiting() {
        let dir = tempfile::tempdir().unwrap();
        file(
            dir.path(),
            Kind::Question,
            "which DB?",
            "2026-11-24T10:00:00Z",
        );
        file(
            dir.path(),
            Kind::Question,
            "and the port?",
            "2026-11-24T10:05:00Z",
        );

        prompt(dir.path(), "postgres on 5432", "2026-11-24T10:30:00Z");

        assert_eq!(wait(dir.path(), "lead-h"), None);
    }

    #[test]
    fn the_wait_names_the_kind_of_the_oldest_waiting_entry() {
        let dir = tempfile::tempdir().unwrap();
        file(
            dir.path(),
            Kind::Question,
            "which DB?",
            "2026-11-24T10:00:00Z",
        );
        file(
            dir.path(),
            Kind::Gate,
            "Gate request: #9",
            "2026-11-24T10:05:00Z",
        );

        assert_eq!(
            wait(dir.path(), "lead-h"),
            Some(Wait {
                kind: Kind::Question,
                since: at("2026-11-24T10:00:00Z")
            })
        );
    }

    #[test]
    fn a_request_before_a_start_is_not_waiting() {
        let dir = tempfile::tempdir().unwrap();
        file(
            dir.path(),
            Kind::Gate,
            "Gate request: #9",
            "2026-11-24T10:00:00Z",
        );
        file(
            dir.path(),
            Kind::Question,
            "which DB?",
            "2026-11-24T10:01:00Z",
        );
        let start = Entry::new(at("2026-11-24T10:10:00Z"), Body::Start { resumed: false });
        append(dir.path(), "lead-h", &start).unwrap();

        assert_eq!(wait(dir.path(), "lead-h"), None);
    }

    #[test]
    fn a_note_or_a_later_report_neither_waits_nor_answers() {
        let dir = tempfile::tempdir().unwrap();
        append(
            dir.path(),
            "lead-h",
            &Entry::new(
                at("2026-11-24T09:00:00Z"),
                Body::report(Kind::Note, "starting"),
            ),
        )
        .unwrap();
        assert_eq!(wait(dir.path(), "lead-h"), None);

        file(
            dir.path(),
            Kind::Question,
            "which DB?",
            "2026-11-24T10:00:00Z",
        );
        append(
            dir.path(),
            "lead-h",
            &Entry::new(
                at("2026-11-24T10:05:00Z"),
                Body::report(Kind::Note, "still here"),
            ),
        )
        .unwrap();

        assert_eq!(
            wait(dir.path(), "lead-h").map(|w| w.since),
            Some(at("2026-11-24T10:00:00Z"))
        );
    }

    #[test]
    fn a_prompt_in_an_unmarked_pane_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();

        prompt(dir.path(), "hello", "2026-11-24T10:00:00Z");

        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
