//! The shepherd's resume after its handoff. The watch that tells it to hand
//! off records where its Claude runs; the first `/clear` there after the
//! handoff is written gets told, through the SessionStart hook, to resume.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context;
use crate::ledger::Process;

/// Where the shepherd's Claude runs. A `/clear` keeps the process, and the
/// hook can't see the session it cleared, so the process stands for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub pane: String,
    pub socket: PathBuf,
    pub process: Process,
}

#[derive(Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    place: Place,
    session: String,
    at: String,
    used: bool,
}

fn record_path(state: &Path) -> PathBuf {
    state.join("shepherd.json")
}

fn handoff_path(state: &Path) -> PathBuf {
    context::handoff_path(state, context::SHEPHERD)
}

/// Replaces any earlier record with an unused one for `session` at `place`.
/// An unused record of the same session and place keeps its time, so a
/// repeated handoff line can't postdate a handoff already written.
pub fn record(state: &Path, place: &Place, session: &str, at: SystemTime) -> Result<()> {
    let earlier = fs::read_to_string(record_path(state))
        .ok()
        .and_then(|text| serde_json::from_str::<Record>(&text).ok())
        .filter(|r| !r.used && r.session == session && r.place == *place);
    let at = match earlier {
        Some(earlier) => earlier.at,
        None => humantime::format_rfc3339_nanos(at).to_string(),
    };
    write(
        state,
        &Record {
            place: place.clone(),
            session: session.into(),
            at,
            used: false,
        },
    )
}

fn write(state: &Path, record: &Record) -> Result<()> {
    let path = record_path(state);
    let written = || format!("write {}", path.display());
    fs::create_dir_all(state).with_context(written)?;
    let mut temp = tempfile::NamedTempFile::new_in(state).with_context(written)?;
    serde_json::to_writer_pretty(&mut temp, record).with_context(written)?;
    temp.persist(&path).with_context(written)?;
    Ok(())
}

/// The context resuming the shepherd, for a `clear` session start at the
/// recorded `place` with an unused record and a handoff written after it.
/// Marks the record used, then moves the handoff to `shepherd.prev.md`, so a
/// failed move leaves the handoff where the human's fallback line names it.
pub fn resume(state: &Path, place: &Place, input: &[u8]) -> Result<Option<String>> {
    let source = serde_json::from_slice::<Value>(input)
        .ok()
        .and_then(|v| v.get("source")?.as_str().map(str::to_string));
    if source.as_deref() != Some("clear") {
        return Ok(None);
    }
    let path = record_path(state);
    let mut record: Record = match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if record.used || record.place != *place {
        return Ok(None);
    }
    let handoff = handoff_path(state);
    let written = match fs::metadata(&handoff).and_then(|m| m.modified()) {
        Ok(written) => written,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", handoff.display())),
    };
    let at = humantime::parse_rfc3339(&record.at)
        .with_context(|| format!("parse the time in {}", path.display()))?;
    if written <= at {
        return Ok(None);
    }
    record.used = true;
    write(state, &record)?;
    let read = handoff.with_extension("prev.md");
    fs::rename(&handoff, &read)
        .with_context(|| format!("move {} to {}", handoff.display(), read.display()))?;
    Ok(Some(format!(
        "You are the loopherd shepherd, resuming after your handoff. Run `loopherd --skill`, \
         read your handoff at {} (written {}), run `loopherd status` and \
         `loopherd reports --all --tail 40` (for a fuller history, `--since <time>` or plain \
         `--all`), re-arm your watch, and continue from the handoff's next steps.",
        read.display(),
        humantime::format_rfc3339_seconds(written),
    )))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const RECORDED_AT: &str = "2026-11-18T10:00:00Z";

    fn place() -> Place {
        Place {
            pane: "w1:p1".into(),
            socket: "/run/herdr.sock".into(),
            process: Process {
                pid: 30,
                start: 300,
            },
        }
    }

    fn start(source: &str) -> Vec<u8> {
        format!(r#"{{"hook_event_name":"SessionStart","source":"{source}","session_id":"new"}}"#)
            .into_bytes()
    }

    fn at(time: &str) -> SystemTime {
        humantime::parse_rfc3339(time).unwrap()
    }

    /// A state dir with a record at [`RECORDED_AT`] and a handoff written at `handoff_at`.
    fn handed_off(handoff_at: &str) -> tempfile::TempDir {
        let state = tempfile::tempdir().unwrap();
        record(state.path(), &place(), "s-old", at(RECORDED_AT)).unwrap();
        write_handoff(state.path(), handoff_at);
        state
    }

    fn write_handoff(state: &Path, written: &str) {
        let path = handoff_path(state);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "## State\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at(written))
            .unwrap();
    }

    fn saved(state: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(record_path(state)).unwrap()).unwrap()
    }

    #[test]
    fn a_clear_at_the_recorded_place_resumes_from_the_moved_handoff() {
        let state = handed_off("2026-11-18T10:05:00Z");

        let told = resume(state.path(), &place(), &start("clear"))
            .unwrap()
            .unwrap();

        let prev = state.path().join("handoffs/shepherd.prev.md");
        assert!(prev.is_file());
        assert!(!handoff_path(state.path()).exists());
        assert!(told.contains(&format!("read your handoff at {}", prev.display())));
        assert!(told.contains("(written 2026-11-18T10:05:00Z)"));
        for step in [
            "`loopherd --skill`",
            "`loopherd status`",
            "`loopherd reports --all --tail 40`",
            "re-arm your watch",
            "next steps",
        ] {
            assert!(told.contains(step), "{step}: {told}");
        }
        assert_eq!(saved(state.path())["used"], true);
    }

    #[test]
    fn a_second_clear_after_a_resume_adds_nothing() {
        let state = handed_off("2026-11-18T10:05:00Z");
        assert!(
            resume(state.path(), &place(), &start("clear"))
                .unwrap()
                .is_some()
        );
        write_handoff(state.path(), "2026-11-18T11:00:00Z");

        assert_eq!(
            resume(state.path(), &place(), &start("clear")).unwrap(),
            None
        );
        assert!(handoff_path(state.path()).is_file());
    }

    #[test]
    fn only_a_clear_resumes() {
        for source in ["startup", "resume", "compact"] {
            let state = handed_off("2026-11-18T10:05:00Z");
            assert_eq!(
                resume(state.path(), &place(), &start(source)).unwrap(),
                None,
                "{source}"
            );
            assert!(handoff_path(state.path()).is_file(), "{source}");
            assert_eq!(saved(state.path())["used"], false, "{source}");
        }
    }

    #[test]
    fn another_pane_socket_or_claude_process_adds_nothing() {
        let others = [
            Place {
                pane: "w1:p2".into(),
                ..place()
            },
            Place {
                socket: "/run/other.sock".into(),
                ..place()
            },
            Place {
                process: Process {
                    pid: 31,
                    start: 310,
                },
                ..place()
            },
            Place {
                process: Process {
                    pid: 30,
                    start: 301,
                },
                ..place()
            },
        ];
        for other in others {
            let state = handed_off("2026-11-18T10:05:00Z");
            assert_eq!(
                resume(state.path(), &other, &start("clear")).unwrap(),
                None,
                "{other:?}"
            );
            assert!(handoff_path(state.path()).is_file());
            assert_eq!(saved(state.path())["used"], false);
        }
    }

    #[test]
    fn a_handoff_not_written_after_the_record_adds_nothing() {
        for written in ["2026-11-18T09:00:00Z", RECORDED_AT] {
            let state = handed_off(written);
            assert_eq!(
                resume(state.path(), &place(), &start("clear")).unwrap(),
                None,
                "{written}"
            );
            assert_eq!(saved(state.path())["used"], false);
        }
    }

    #[test]
    fn no_record_or_no_handoff_adds_nothing() {
        let state = tempfile::tempdir().unwrap();
        write_handoff(state.path(), "2026-11-18T10:05:00Z");
        assert_eq!(
            resume(state.path(), &place(), &start("clear")).unwrap(),
            None
        );

        let state = tempfile::tempdir().unwrap();
        record(state.path(), &place(), "s-old", at(RECORDED_AT)).unwrap();
        assert_eq!(
            resume(state.path(), &place(), &start("clear")).unwrap(),
            None
        );
        assert_eq!(saved(state.path())["used"], false);
    }

    #[test]
    fn a_new_record_replaces_a_used_one_and_never_expires() {
        let state = handed_off("2026-11-18T10:05:00Z");
        assert!(
            resume(state.path(), &place(), &start("clear"))
                .unwrap()
                .is_some()
        );

        let later = at("2026-11-20T10:00:00Z");
        record(state.path(), &place(), "s-next", later).unwrap();
        write_handoff(state.path(), "2026-12-25T10:00:00Z");

        assert!(
            resume(state.path(), &place(), &start("clear"))
                .unwrap()
                .is_some()
        );
        assert_eq!(saved(state.path())["session"], "s-next");
    }

    #[test]
    fn a_repeated_record_of_the_same_unused_session_keeps_its_time() {
        let state = tempfile::tempdir().unwrap();
        record(state.path(), &place(), "s-old", at(RECORDED_AT)).unwrap();
        write_handoff(state.path(), "2026-11-18T10:05:00Z");
        record(state.path(), &place(), "s-old", at("2026-11-18T10:10:00Z")).unwrap();
        assert_eq!(saved(state.path())["at"], "2026-11-18T10:00:00.000000000Z");
        assert!(
            resume(state.path(), &place(), &start("clear"))
                .unwrap()
                .is_some()
        );

        record(state.path(), &place(), "s-next", at("2026-11-18T10:10:00Z")).unwrap();
        assert_eq!(saved(state.path())["at"], "2026-11-18T10:10:00.000000000Z");
    }

    #[test]
    fn the_record_names_the_pane_socket_session_process_and_time() {
        let state = tempfile::tempdir().unwrap();
        let when = at(RECORDED_AT) + Duration::from_millis(250);
        record(state.path(), &place(), "s-old", when).unwrap();
        assert_eq!(
            saved(state.path()),
            serde_json::json!({
                "pane": "w1:p1",
                "socket": "/run/herdr.sock",
                "process": {"pid": 30, "start": 300},
                "session": "s-old",
                "at": "2026-11-18T10:00:00.250000000Z",
                "used": false,
            })
        );
    }

    #[test]
    fn an_unreadable_record_is_an_error_not_a_resume() {
        let state = handed_off("2026-11-18T10:05:00Z");
        fs::write(record_path(state.path()), "not json").unwrap();
        assert!(resume(state.path(), &place(), &start("clear")).is_err());
        assert!(handoff_path(state.path()).is_file());
    }
}
