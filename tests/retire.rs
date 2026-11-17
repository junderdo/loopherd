//! `loopherd retire` without `--close`, through the binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_ledger(state: &Path) {
    fs::create_dir_all(state.join("loopherd")).unwrap();
    fs::write(
        state.join("loopherd/ledger.json"),
        r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z"}}"#,
    )
    .unwrap();
}

fn write_marks(mailboxes: &Path, member: &str) {
    fs::create_dir_all(mailboxes).unwrap();
    fs::write(
        mailboxes.join(format!("{member}.notified")),
        "question t1\n",
    )
    .unwrap();
    fs::write(mailboxes.join(format!("{member}.blocked")), "").unwrap();
}

fn retire(state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["retire", "lh-1"])
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("PATH", "")
        .env_remove("HERDR_PANE_ID")
        .output()
        .unwrap()
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn retire_removes_the_members_notify_marks_and_leaves_other_files() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    let mailboxes = state.path().join("loopherd/reports");
    write_marks(&mailboxes, "lh-1");
    write_marks(&mailboxes, "lh-2");
    fs::write(mailboxes.join("lh-1.jsonl"), "{}\n").unwrap();

    let out = retire(state.path());

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lh-1\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(
        files_in(&mailboxes),
        ["lh-1.jsonl", "lh-2.blocked", "lh-2.notified"]
    );
}

#[test]
fn retire_without_notify_marks_is_not_an_error() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());

    let out = retire(state.path());

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lh-1\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

#[test]
fn a_mark_that_cannot_be_removed_warns_and_the_retire_stands() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    let stuck = state.path().join("loopherd/reports/lh-1.notified");
    fs::create_dir_all(stuck.join("inside")).unwrap();

    let out = retire(state.path());

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lh-1\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with(&format!("warning: remove {}: ", stuck.display())),
        "{stderr}"
    );
    let ledger = fs::read_to_string(state.path().join("loopherd/ledger.json")).unwrap();
    assert!(ledger.contains("\"retired\": true"), "{ledger}");
}
