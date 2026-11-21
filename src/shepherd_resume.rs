//! The shepherd's resume after its handoff. The watch that tells it to hand
//! off records where its Claude runs and which session it measured; the
//! first `/clear` of that session after the handoff is written gets told,
//! through the SessionStart hook, to resume.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context;
use crate::handoff;
use crate::ledger::Process;
use crate::lock::Lock;
use crate::report::Stop;

/// Where the shepherd's Claude runs. A `/clear` keeps the process.
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
    /// The pane's shell, which outlives a Claude restarted in the pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell: Option<Process>,
    session: String,
    /// The session the process last started since the record, which a
    /// `/clear` hook can't see: its input names only the new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current: Option<String>,
    at: String,
    used: bool,
    #[serde(flatten)]
    restart: Restart,
}

/// What the shepherd's self-restart keeps in the record. Times are RFC 3339.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Restart {
    /// The shepherd's last stop at the recorded place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopAt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_restart: Option<Attempt>,
    /// When a self-restart last sent `/clear`; carried into later records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_self_restart: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopAt {
    pub at: String,
    #[serde(flatten)]
    pub stop: Stop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub armed: String,
    pub state: AttemptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Pending,
    Cancelled,
    Restarting,
    Restarted,
    GaveUp,
}

/// The record at a place, as [`update`] hands it over.
pub struct Recorded {
    pub used: bool,
    /// `handoffs/shepherd.md` was written after the record, so a `/clear` resumes from it.
    pub handoff_written: bool,
    pub restart: Restart,
}

fn record_path(state: &Path) -> PathBuf {
    state.join("shepherd.json")
}

fn handoff_path(state: &Path) -> PathBuf {
    handoff::path(state, context::SHEPHERD)
}

/// Replaces any earlier record with an unused one for `session` at `place`,
/// whose pane runs `shell`. An unused record of the same session and place
/// keeps its time, so a repeated handoff line can't postdate a handoff
/// already written. The last self-restart's time carries over from the
/// record replaced.
pub fn record(
    state: &Path,
    place: &Place,
    shell: Option<Process>,
    session: &str,
    at: SystemTime,
) -> Result<()> {
    let _lock = lock(state)?;
    let earlier = last_record(state);
    let record = match earlier {
        Some(earlier) if !earlier.used && earlier.session == session && earlier.place == *place => {
            Record {
                shell: earlier.shell.or(shell),
                ..earlier
            }
        }
        earlier => Record {
            restart: Restart {
                last_self_restart: earlier.and_then(|e| e.restart.last_self_restart),
                ..Restart::default()
            },
            place: place.clone(),
            shell,
            session: session.into(),
            current: None,
            at: humantime::format_rfc3339_nanos(at).to_string(),
            used: false,
        },
    };
    write(state, &record)
}

/// Holds `shepherd.json` for one read and rewrite: hooks and the self-restart waiter share it.
fn lock(state: &Path) -> Result<Lock> {
    let path = state.join("shepherd.json.lock");
    fs::create_dir_all(state).with_context(|| format!("create {}", state.display()))?;
    let file = fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
    Lock::wait(file, crate::lock::BRIEF).with_context(|| format!("lock {}", path.display()))
}

/// Runs `change` on the record at `place`, used or not, under its lock, and
/// saves what it changed; `None` when there's no record for `place`.
pub fn update<T>(
    state: &Path,
    place: &Place,
    change: impl FnOnce(&mut Recorded) -> T,
) -> Result<Option<T>> {
    let _lock = lock(state)?;
    let Some(mut record) = read_any(state, place)? else {
        return Ok(None);
    };
    let mut recorded = Recorded {
        used: record.used,
        handoff_written: handoff_written_after(state, &record.at)?.is_some(),
        restart: record.restart.clone(),
    };
    let out = change(&mut recorded);
    if recorded.restart != record.restart {
        record.restart = recorded.restart;
        write(state, &record)?;
    }
    Ok(Some(out))
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

/// Where the last record says the shepherd ran, used or not; `None` when
/// there's no record or it won't parse.
pub fn recorded_place(state: &Path) -> Option<Place> {
    last_record(state).map(|r| r.place)
}

/// The last record's place and the shell its pane ran, used or not.
pub fn recorded_place_and_shell(state: &Path) -> Option<(Place, Option<Process>)> {
    last_record(state).map(|r| (r.place, r.shell))
}

/// The record, used or not; `None` when missing or it won't parse.
fn last_record(state: &Path) -> Option<Record> {
    let text = fs::read_to_string(record_path(state)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The unused record for `place`; see [`read_any`].
fn read(state: &Path, place: &Place) -> Result<Option<Record>> {
    Ok(read_any(state, place)?.filter(|r| !r.used))
}

/// The record for `place`, used or not. A record that isn't JSON, or names
/// another pane or socket, is none; one for this pane and socket that won't
/// parse is an error.
fn read_any(state: &Path, place: &Place) -> Result<Option<Record>> {
    let path = record_path(state);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    let socket = place.socket.to_str();
    if value["pane"].as_str() != Some(place.pane.as_str()) || value["socket"].as_str() != socket {
        return Ok(None);
    }
    let record: Record =
        serde_json::from_value(value).with_context(|| format!("parse {}", path.display()))?;
    Ok((record.place == *place).then_some(record))
}

/// Called on every session start at `place`. Returns the context resuming the
/// shepherd for a `clear` of the recorded session (or one `live_session`
/// resolves to the same live session) once a handoff was written after the
/// record; marks the record used, then moves the handoff to
/// `shepherd.prev.md`, so a failed move leaves it where the fallback names it.
/// Any other start only follows the process to the session it started.
pub fn session_started(
    state: &Path,
    place: &Place,
    input: &[u8],
    live_session: &dyn Fn(&str) -> String,
) -> Result<Option<String>> {
    let Some((source, started)) = parse(input) else {
        return Ok(None);
    };
    let _lock = lock(state)?;
    let Some(mut record) = read(state, place)? else {
        return Ok(None);
    };
    let cleared = record.current.as_deref().unwrap_or(&record.session);
    let of_the_recorded = live_session(cleared) == live_session(&record.session);
    let written = match source == "clear" && of_the_recorded {
        true => handoff_written_after(state, &record.at)?,
        false => None,
    };
    let Some(written) = written else {
        if record.current.as_deref() != Some(started.as_str()) {
            record.current = Some(started);
            write(state, &record)?;
        }
        return Ok(None);
    };
    record.used = true;
    record.current = Some(started);
    write(state, &record)?;
    let resume = resume_from_moved_handoff(state, written)?;
    let restarting = record.restart.self_restart.as_ref().map(|a| a.state);
    Ok(Some(match restarting {
        Some(AttemptState::Restarting) => format!(
            "{resume} You restarted yourself after your handoff ([shepherd] self_restart); \
             say so in your first message."
        ),
        _ => resume,
    }))
}

/// A shepherd started afresh resuming from the handoff, which
/// [`NewResume::undo`] gives back when the start fails.
pub struct NewResume {
    pub prompt: String,
}

impl NewResume {
    /// Moves the handoff back from `shepherd.prev.md` and unmarks the record.
    pub fn undo(self, state: &Path) -> Result<()> {
        let _lock = lock(state)?;
        let handoff = handoff_path(state);
        let read = handoff::prev(state, context::SHEPHERD);
        fs::rename(&read, &handoff)
            .with_context(|| format!("move {} to {}", read.display(), handoff.display()))?;
        match last_record(state) {
            Some(mut record) => {
                record.used = false;
                write(state, &record)
            }
            None => Ok(()),
        }
    }
}

/// The resume for a shepherd started afresh on the herdr at `socket`, when
/// the last record is unused and names that socket, its Claude never moved to
/// another session, and `handoffs/shepherd.md` was written after it: marks the
/// record used and moves the handoff as a resume does. A moved session means
/// the handoff was already acted on.
pub fn new_shepherd_resume(state: &Path, socket: Option<&Path>) -> Result<Option<NewResume>> {
    let _lock = lock(state)?;
    let Some(mut record) = last_record(state) else {
        return Ok(None);
    };
    let moved_on = record
        .current
        .as_ref()
        .is_some_and(|c| *c != record.session);
    let here = socket == Some(record.place.socket.as_path());
    if record.used || moved_on || !here {
        return Ok(None);
    }
    let Some(written) = handoff_written_after(state, &record.at)? else {
        return Ok(None);
    };
    record.used = true;
    write(state, &record)?;
    let prompt = resume_from_moved_handoff(state, written)?;
    Ok(Some(NewResume { prompt }))
}

/// Moves the handoff to `shepherd.prev.md` and returns the context that
/// resumes the shepherd from it.
fn resume_from_moved_handoff(state: &Path, written: SystemTime) -> Result<String> {
    let handoff = handoff::find(state, context::SHEPHERD).unwrap_or_else(|| handoff_path(state));
    let aside = handoff::set_aside(state, context::SHEPHERD, &handoff)?;
    let read = aside.prev;
    let warning = aside
        .warning
        .map_or(String::new(), |w| format!(" loopherd warning: {w}"));
    Ok(format!(
        "You are the loopherd shepherd, resuming after your handoff. Run `loopherd --skill`, \
         read your handoff at {} (written {}), run `loopherd status` and \
         `loopherd reports --all --tail 40` (for a fuller history, `--since <time>` or plain \
         `--all`), re-arm your watch, and continue from the handoff's next steps.{warning}",
        read.display(),
        humantime::format_rfc3339_seconds(written),
    ))
}

/// When the handoff was written, if after the record's time `at`.
fn handoff_written_after(state: &Path, at: &str) -> Result<Option<SystemTime>> {
    let Some(handoff) = handoff::find(state, context::SHEPHERD) else {
        return Ok(None);
    };
    let written = match fs::metadata(&handoff).and_then(|m| m.modified()) {
        Ok(written) => written,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", handoff.display())),
    };
    let at = humantime::parse_rfc3339(at)
        .with_context(|| format!("parse the time in {}", record_path(state).display()))?;
    Ok((written > at).then_some(written))
}

fn parse(input: &[u8]) -> Option<(String, String)> {
    let input: Value = serde_json::from_slice(input).ok()?;
    let source = input.get("source")?.as_str()?;
    let session = input.get("session_id")?.as_str()?;
    (!session.is_empty()).then(|| (source.to_string(), session.to_string()))
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
        start_of(source, "new")
    }

    fn start_of(source: &str, session: &str) -> Vec<u8> {
        format!(
            r#"{{"hook_event_name":"SessionStart","source":"{source}","session_id":"{session}"}}"#
        )
        .into_bytes()
    }

    /// A session start where no session continued in another.
    fn resume(state: &Path, place: &Place, input: &[u8]) -> Result<Option<String>> {
        session_started(state, place, input, &|s| s.to_string())
    }

    fn at(time: &str) -> SystemTime {
        humantime::parse_rfc3339(time).unwrap()
    }

    /// A state dir with a record at [`RECORDED_AT`] and a handoff written at `handoff_at`.
    fn handed_off(handoff_at: &str) -> tempfile::TempDir {
        let state = tempfile::tempdir().unwrap();
        record(state.path(), &place(), None, "s-old", at(RECORDED_AT)).unwrap();
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
    fn a_flat_handoff_an_older_release_wrote_is_resumed_from_and_moved_to_prev() {
        let state = handed_off("2026-11-18T10:05:00Z");
        let handoffs = state.path().join("handoffs");
        fs::rename(handoff_path(state.path()), handoffs.join("shepherd.md")).unwrap();

        let told = resume(state.path(), &place(), &start("clear"))
            .unwrap()
            .unwrap();

        let prev = handoffs.join("shepherd/prev.md");
        assert_eq!(fs::read_to_string(&prev).unwrap(), "## State\n");
        assert!(!handoffs.join("shepherd.md").exists());
        assert!(told.contains(&format!("read your handoff at {}", prev.display())));
        assert!(told.contains("(written 2026-11-18T10:05:00Z)"));
    }

    #[test]
    fn a_clear_at_the_recorded_place_resumes_from_the_moved_handoff() {
        let state = handed_off("2026-11-18T10:05:00Z");

        let told = resume(state.path(), &place(), &start("clear"))
            .unwrap()
            .unwrap();

        let prev = state.path().join("handoffs/shepherd/prev.md");
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
        record(state.path(), &place(), None, "s-old", at(RECORDED_AT)).unwrap();
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
        record(state.path(), &place(), None, "s-next", later).unwrap();
        write_handoff(state.path(), "2026-12-25T10:00:00Z");

        assert!(
            resume(state.path(), &place(), &start("clear"))
                .unwrap()
                .is_some()
        );
        assert_eq!(saved(state.path())["session"], "s-next");
    }

    #[test]
    fn a_repeated_record_of_the_same_unused_session_fills_in_its_shell() {
        let state = tempfile::tempdir().unwrap();
        let shell = Process { pid: 7, start: 70 };
        record(state.path(), &place(), None, "s-old", at(RECORDED_AT)).unwrap();
        record(
            state.path(),
            &place(),
            Some(shell),
            "s-old",
            at(RECORDED_AT),
        )
        .unwrap();
        assert_eq!(
            recorded_place_and_shell(state.path()),
            Some((place(), Some(shell)))
        );
    }

    #[test]
    fn a_repeated_record_of_the_same_unused_session_keeps_its_time() {
        let state = tempfile::tempdir().unwrap();
        record(state.path(), &place(), None, "s-old", at(RECORDED_AT)).unwrap();
        write_handoff(state.path(), "2026-11-18T10:05:00Z");
        record(
            state.path(),
            &place(),
            None,
            "s-old",
            at("2026-11-18T10:10:00Z"),
        )
        .unwrap();
        assert_eq!(saved(state.path())["at"], "2026-11-18T10:00:00.000000000Z");
        assert!(
            resume(state.path(), &place(), &start("clear"))
                .unwrap()
                .is_some()
        );

        record(
            state.path(),
            &place(),
            None,
            "s-next",
            at("2026-11-18T10:10:00Z"),
        )
        .unwrap();
        assert_eq!(saved(state.path())["at"], "2026-11-18T10:10:00.000000000Z");
    }

    #[test]
    fn the_record_names_the_pane_socket_session_process_and_time() {
        let state = tempfile::tempdir().unwrap();
        let when = at(RECORDED_AT) + Duration::from_millis(250);
        record(state.path(), &place(), None, "s-old", when).unwrap();
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
    fn a_record_that_isnt_json_or_names_another_pane_is_no_record() {
        let state = handed_off("2026-11-18T10:05:00Z");
        fs::write(record_path(state.path()), "not json").unwrap();
        assert_eq!(
            resume(state.path(), &place(), &start("clear")).unwrap(),
            None
        );

        let other = r#"{"pane": "w1:p2", "socket": "/run/herdr.sock"}"#;
        fs::write(record_path(state.path()), other).unwrap();
        assert_eq!(
            resume(state.path(), &place(), &start("clear")).unwrap(),
            None
        );
        assert!(handoff_path(state.path()).is_file());
    }

    #[test]
    fn a_broken_record_for_this_pane_and_socket_is_an_error_not_a_resume() {
        let state = handed_off("2026-11-18T10:05:00Z");
        let broken = r#"{"pane": "w1:p1", "socket": "/run/herdr.sock", "used": "no"}"#;
        fs::write(record_path(state.path()), broken).unwrap();
        assert!(resume(state.path(), &place(), &start("clear")).is_err());
        assert!(handoff_path(state.path()).is_file());
    }

    #[test]
    fn a_clear_of_another_session_in_the_same_claude_adds_nothing() {
        let state = handed_off("2026-11-18T10:05:00Z");
        let resumed = start_of("resume", "unrelated");
        assert_eq!(resume(state.path(), &place(), &resumed).unwrap(), None);

        assert_eq!(
            resume(state.path(), &place(), &start("clear")).unwrap(),
            None
        );
        assert!(handoff_path(state.path()).is_file());
        assert_eq!(saved(state.path())["used"], false);
    }

    #[test]
    fn a_clear_after_an_earlier_clear_moved_the_claude_on_adds_nothing() {
        let state = tempfile::tempdir().unwrap();
        record(state.path(), &place(), None, "s-old", at(RECORDED_AT)).unwrap();
        assert_eq!(
            resume(state.path(), &place(), &start_of("clear", "s-mid")).unwrap(),
            None
        );
        write_handoff(state.path(), "2026-11-18T10:05:00Z");

        assert_eq!(
            resume(state.path(), &place(), &start("clear")).unwrap(),
            None
        );
        assert_eq!(saved(state.path())["current"], "new");
    }

    #[test]
    fn returning_to_the_recorded_session_before_the_clear_resumes() {
        let state = handed_off("2026-11-18T10:05:00Z");
        resume(state.path(), &place(), &start_of("resume", "unrelated")).unwrap();
        resume(state.path(), &place(), &start_of("resume", "s-old")).unwrap();

        assert!(
            resume(state.path(), &place(), &start("clear"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_clear_of_a_session_the_recorded_one_continued_in_resumes() {
        let continued = |s: &str| match s {
            "s-old" => "s-cont".to_string(),
            other => other.to_string(),
        };
        let state = handed_off("2026-11-18T10:05:00Z");
        session_started(
            state.path(),
            &place(),
            &start_of("resume", "s-cont"),
            &continued,
        )
        .unwrap();

        let told = session_started(state.path(), &place(), &start("clear"), &continued).unwrap();
        assert!(told.is_some());
    }
}
