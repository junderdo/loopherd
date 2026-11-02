//! `loopherd stop-hook`, as a member's Claude Stop hook runs it.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn write_ledger(state: &Path) -> std::path::PathBuf {
    fs::create_dir_all(state.join("loopherd")).unwrap();
    let path = state.join("loopherd/ledger.json");
    fs::write(
        &path,
        r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z"}}"#,
    )
    .unwrap();
    path
}

fn stop_hook(ledger: &Path, pane: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["stop-hook", "--ledger", ledger.to_str().unwrap()])
        .env("HERDR_PANE_ID", pane)
        .env_remove("XDG_STATE_HOME")
        .env("HOME", "/nonexistent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn the_hook_records_the_stop_beside_the_ledger_it_was_given() {
    let state = tempfile::tempdir().unwrap();
    let ledger = write_ledger(state.path());

    let out = stop_hook(
        &ledger,
        "w1:p1",
        r#"{"background_tasks":[{"type":"shell"}],"session_crons":[]}"#,
    );

    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
    let record = fs::read_to_string(state.path().join("loopherd/stops/lh-1.json")).unwrap();
    assert!(
        record.contains(r#""background_tasks":[{"type":"shell"}]"#),
        "{record}"
    );
}

#[test]
fn a_failing_hook_never_blocks_claude_from_stopping() {
    let state = tempfile::tempdir().unwrap();
    let ledger = write_ledger(state.path());

    let out = stop_hook(&ledger, "w1:p9", "{}");

    // Exit 2 would block the stop; any error exits 0 and says why on stderr.
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a member"));
}
