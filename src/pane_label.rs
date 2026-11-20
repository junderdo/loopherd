//! A label in herdr's sidebar on the pane of whoever must relay or answer
//! what waits on the human, cleared once the human next prompts there. A
//! mark in the mailboxes directory says which panes loopherd labelled, so
//! clearing an unlabelled pane costs no herdr call. A per-pane lock holds
//! each mark change and its herdr call together, so a set and a clear can't
//! interleave into a label with no mark.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::lock::Lock;
use crate::notify::{self, Kind, Waiting};

/// Labels `pane` with `waiting`: the asker and the question's first line,
/// redacted as a notification is, or the member that went blocked.
pub fn set(herdr: &dyn Herdr, dir: &Path, pane: &str, waiting: &Waiting) -> Result<()> {
    let label = match waiting.kind {
        Kind::Question => format!(
            "{} asks: {}",
            waiting.member,
            notify::summary(&waiting.text)
        ),
        Kind::Blocked => format!("{} is blocked", waiting.member),
        // A lead's watch sends these for itself, and the shepherd's self-restart labels its own pane.
        Kind::MergeAlert | Kind::SelfRestart => return Ok(()),
    };
    label_until_prompted(herdr, dir, pane, &label)
}

/// Labels `pane` with `label` until the human next prompts there.
pub fn label_until_prompted(herdr: &dyn Herdr, dir: &Path, pane: &str, label: &str) -> Result<()> {
    let mark = mark(dir, pane);
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let _lock = lock(dir, pane)?;
    // Marked first: a mark with no label costs one needless clear; a label with no mark stays.
    open_regular(&mark)?
        .set_len(0)
        .with_context(|| format!("write {}", mark.display()))?;
    herdr.label_pane(pane, Some(label))
}

/// Labels `pane` with `question` when `reporter` is the lead whose pane it is.
pub fn lead_asked(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    dir: &Path,
    pane: &str,
    reporter: &str,
    question: &str,
) -> Result<()> {
    let entries = ledger.entries()?;
    let Some(lead) = entries
        .get(reporter)
        .filter(|e| e.is_lead() && e.pane_id == pane)
    else {
        return Ok(());
    };
    let waiting = Waiting {
        herd: lead.herd.clone(),
        member: lead.name.clone(),
        kind: Kind::Question,
        text: question.to_string(),
        at: None,
    };
    set(herdr, dir, pane, &waiting)
}

/// Clears `pane`'s label for the prompt in a UserPromptSubmit hook's `input`.
/// Claude hands each finished background task to the model as a prompt of
/// its own, the shepherd's watch among them; that keeps the label, and a
/// prompt with any other text before or after the block clears it.
pub fn prompted(herdr: &dyn Herdr, dir: &Path, pane: &str, input: &[u8]) -> Result<()> {
    let input: Option<serde_json::Value> = serde_json::from_slice(input).ok();
    let prompt = input.as_ref().and_then(|i| i["prompt"].as_str());
    if prompt.is_some_and(is_task_notification) {
        return Ok(());
    }
    clear(herdr, dir, pane)
}

/// A prompt Claude made from a finished background task, not one the human typed.
pub fn is_task_notification(prompt: &str) -> bool {
    let prompt = prompt.trim();
    prompt.starts_with("<task-notification>") && prompt.ends_with("</task-notification>")
}

/// Clears `pane`'s label for a SessionStart hook's `input` that starts a
/// session afresh or resumes one; a compaction or fork carries the wait on.
pub fn session_started(herdr: &dyn Herdr, dir: &Path, pane: &str, input: &[u8]) -> Result<()> {
    let input: Option<serde_json::Value> = serde_json::from_slice(input).ok();
    match input.as_ref().and_then(|i| i["source"].as_str()) {
        Some("compact" | "fork") => Ok(()),
        _ => clear(herdr, dir, pane),
    }
}

/// Clears the label loopherd left on `pane`, if any.
pub fn clear(herdr: &dyn Herdr, dir: &Path, pane: &str) -> Result<()> {
    let mark = mark(dir, pane);
    if !mark.exists() {
        return Ok(());
    }
    let _lock = lock(dir, pane)?;
    match fs::remove_file(&mark) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        removed => removed.with_context(|| format!("remove {}", mark.display()))?,
    }
    herdr.label_pane(pane, None).inspect_err(|_| {
        // Marked again, so the next prompt or session start retries.
        let _ = open_regular(&mark);
    })
}

fn mark(dir: &Path, pane: &str) -> PathBuf {
    dir.join(format!(".pane-label-{pane}"))
}

/// How long a label change waits for another holding its pane's lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// The pane's lock, within `LOCK_TIMEOUT`: the lead can write the mailboxes
/// directory, so its broker mustn't wait on a lock the lead holds forever.
fn lock(dir: &Path, pane: &str) -> Result<Lock> {
    let path = dir.join(format!(".pane-label-{pane}.lock"));
    let given_up = Instant::now() + LOCK_TIMEOUT;
    loop {
        let file = open_regular(&path)?;
        if let Some(lock) =
            Lock::try_take(file).with_context(|| format!("lock {}", path.display()))?
        {
            return Ok(lock);
        }
        if Instant::now() >= given_up {
            bail!(
                "{} stayed locked for {}s",
                path.display(),
                LOCK_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Opens or creates `path` for writing, refusing a symlink, a FIFO or
/// anything else but a plain file: a lead can plant one in the mailboxes
/// directory for its unsandboxed broker to write through.
fn open_regular(path: &Path) -> Result<File> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} isn't a plain file", path.display());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger, Role};

    fn member(name: &str, pane: &str, role: Option<Role>) -> AgentRecord {
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
            herd: Some("cards".into()),
            template: None,
            params: None,
            socket: None,
            process: None,
            role,
            after_merge: None,
            unsandboxed: None,
            earlier: vec![],
            unknown: Default::default(),
        }
    }

    fn ledger() -> MemLedger {
        let members = [
            member("lead-cards", "w9:p0", Some(Role::Lead)),
            member("lh-1", "w9:p1", None),
        ];
        MemLedger(
            members.into_iter().map(|m| (m.name.clone(), m)).collect(),
            vec![],
        )
    }

    #[test]
    fn a_leads_question_labels_its_own_pane_with_the_first_line_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);

        let question = "\nmerge with password: hunter2?\nmore";
        lead_asked(
            &herdr,
            &ledger(),
            dir.path(),
            "w9:p0",
            "lead-cards",
            question,
        )
        .unwrap();

        assert_eq!(
            herdr.label("w9:p0").as_deref(),
            Some("lead-cards asks: merge with password: [redacted]?")
        );
    }

    fn ask(herdr: &FakeHerdr, dir: &Path) -> Result<()> {
        lead_asked(herdr, &ledger(), dir, "w9:p0", "lead-cards", "which DB?")
    }

    #[test]
    fn a_symlink_planted_at_the_mark_or_its_lock_is_not_written_through() {
        for planted in [".pane-label-w9:p0", ".pane-label-w9:p0.lock"] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("settings.json");
            fs::write(&target, "the human's settings").unwrap();
            std::os::unix::fs::symlink(&target, dir.path().join(planted)).unwrap();
            let herdr = FakeHerdr::new(&[]);

            let err = ask(&herdr, dir.path()).unwrap_err();

            assert!(format!("{err:#}").contains(planted), "{err:#}");
            assert_eq!(fs::read_to_string(&target).unwrap(), "the human's settings");
            assert_eq!(herdr.label("w9:p0"), None);
        }
    }

    #[test]
    fn a_fifo_planted_at_the_mark_or_its_lock_is_refused_without_blocking() {
        for planted in [".pane-label-w9:p0", ".pane-label-w9:p0.lock"] {
            let dir = tempfile::tempdir().unwrap();
            let fifo = std::ffi::CString::new(dir.path().join(planted).to_str().unwrap()).unwrap();
            // SAFETY: mkfifo on a path this test owns.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            // A reader, so opening the FIFO for writing would succeed and only the check refuses it.
            let _reader = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(dir.path().join(planted))
                .unwrap();
            let herdr = FakeHerdr::new(&[]);

            let err = ask(&herdr, dir.path()).unwrap_err();

            assert!(format!("{err:#}").contains("isn't a plain file"), "{err:#}");
            assert_eq!(herdr.label("w9:p0"), None);
        }
    }

    #[test]
    fn a_pane_lock_held_by_another_gives_up_rather_than_wait_forever() {
        let dir = tempfile::tempdir().unwrap();
        let _held = lock(dir.path(), "w9:p0").unwrap();
        let herdr = FakeHerdr::new(&[]);
        let started = Instant::now();

        let err = ask(&herdr, dir.path()).unwrap_err();

        assert!(started.elapsed() < LOCK_TIMEOUT * 2);
        assert!(
            format!("{err:#}").contains("stayed locked for 2s"),
            "{err:#}"
        );
        assert_eq!(herdr.label("w9:p0"), None);
    }

    #[test]
    fn a_question_from_a_worker_or_from_another_pane_labels_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        let ledger = ledger();

        lead_asked(&herdr, &ledger, dir.path(), "w9:p1", "lh-1", "which DB?").unwrap();
        lead_asked(
            &herdr,
            &ledger,
            dir.path(),
            "w9:p5",
            "lead-cards",
            "which DB?",
        )
        .unwrap();

        assert_eq!(herdr.label_calls(), 0);
    }

    #[test]
    fn clearing_a_labelled_pane_drops_its_label_once() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        lead_asked(
            &herdr,
            &ledger(),
            dir.path(),
            "w9:p0",
            "lead-cards",
            "which DB?",
        )
        .unwrap();

        clear(&herdr, dir.path(), "w9:p0").unwrap();
        clear(&herdr, dir.path(), "w9:p0").unwrap();

        assert_eq!(herdr.label("w9:p0"), None);
        assert_eq!(herdr.label_calls(), 2);
    }

    #[test]
    fn a_typed_prompt_clears_the_label_but_a_background_tasks_notification_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        lead_asked(
            &herdr,
            &ledger(),
            dir.path(),
            "w9:p0",
            "lead-cards",
            "which DB?",
        )
        .unwrap();

        let notification = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>",
        });
        prompted(
            &herdr,
            dir.path(),
            "w9:p0",
            notification.to_string().as_bytes(),
        )
        .unwrap();
        assert!(herdr.label("w9:p0").is_some());

        let typed = r#"{"hook_event_name":"UserPromptSubmit","prompt":"use postgres"}"#;
        prompted(&herdr, dir.path(), "w9:p0", typed.as_bytes()).unwrap();
        assert_eq!(herdr.label("w9:p0"), None);
    }

    #[test]
    fn a_new_or_resumed_session_clears_the_label_but_a_compaction_or_fork_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        let start = |source: &str| {
            let input = format!(r#"{{"hook_event_name":"SessionStart","source":"{source}"}}"#);
            session_started(&herdr, dir.path(), "w9:p0", input.as_bytes()).unwrap();
            herdr.label("w9:p0").is_some()
        };
        let ask = || {
            lead_asked(
                &herdr,
                &ledger(),
                dir.path(),
                "w9:p0",
                "lead-cards",
                "which DB?",
            )
            .unwrap()
        };

        ask();
        assert!(start("compact"));
        assert!(start("fork"));
        for source in ["startup", "resume", "clear"] {
            ask();
            assert!(!start(source), "{source}");
        }
    }

    #[test]
    fn a_prompt_hook_input_that_cannot_be_read_clears_the_label() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        lead_asked(
            &herdr,
            &ledger(),
            dir.path(),
            "w9:p0",
            "lead-cards",
            "which DB?",
        )
        .unwrap();

        prompted(&herdr, dir.path(), "w9:p0", b"not json").unwrap();

        assert_eq!(herdr.label("w9:p0"), None);
    }

    #[test]
    fn a_clear_waits_for_a_set_holding_the_panes_lock() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        lead_asked(
            &herdr,
            &ledger(),
            dir.path(),
            "w9:p0",
            "lead-cards",
            "which DB?",
        )
        .unwrap();
        let held = lock(dir.path(), "w9:p0").unwrap();

        let clearing = {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let herdr = FakeHerdr::new(&[]);
                clear(&herdr, &dir, "w9:p0").unwrap();
                herdr.label_calls()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!clearing.is_finished());
        drop(held);

        assert_eq!(clearing.join().unwrap(), 1);
    }

    #[test]
    fn only_a_prompt_that_is_wholly_a_task_notification_keeps_the_label() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        let notification = "<task-notification>\n<status>completed</status>\n</task-notification>";
        let kept = |prompt: &str| {
            lead_asked(
                &herdr,
                &ledger(),
                dir.path(),
                "w9:p0",
                "lead-cards",
                "which DB?",
            )
            .unwrap();
            let input =
                serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt});
            prompted(&herdr, dir.path(), "w9:p0", input.to_string().as_bytes()).unwrap();
            herdr.label("w9:p0").is_some()
        };

        assert!(kept(notification));
        assert!(kept(&format!("\n  {notification}\n")));
        assert!(kept(
            "<task-notification>\n<summary>printed </task-notification> literally</summary>\n</task-notification>"
        ));
        assert!(!kept(&format!("{notification}\nuse postgres")));
        assert!(!kept(&format!("use postgres\n{notification}")));
        assert!(!kept(&format!("note:\n{notification}")));
        assert!(!kept("<task-notification>\n<status>completed</status>"));
    }

    #[test]
    fn clearing_a_pane_loopherd_never_labelled_calls_no_herdr() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        herdr.set_unreachable(true);

        clear(&herdr, dir.path(), "w9:p0").unwrap();

        assert_eq!(herdr.label_calls(), 0);
    }

    #[test]
    fn a_label_herdr_refused_stays_marked_so_the_next_clear_tries_again() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        lead_asked(
            &herdr,
            &ledger(),
            dir.path(),
            "w9:p0",
            "lead-cards",
            "which DB?",
        )
        .unwrap();
        herdr.set_unreachable(true);

        assert!(clear(&herdr, dir.path(), "w9:p0").is_err());
        herdr.set_unreachable(false);
        clear(&herdr, dir.path(), "w9:p0").unwrap();

        assert_eq!(herdr.label("w9:p0"), None);
    }
}
