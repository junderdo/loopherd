//! `loopherd report` and `loopherd reports` through the binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_ledger(state: &Path) {
    fs::create_dir_all(state.join("loopherd")).unwrap();
    let entry = |name: &str, pane: &str| {
        format!(
            r#""{name}": {{"name": "{name}", "pane_id": "{pane}", "session_id": null,
                "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
                "prompt_sent": true, "created": "2026-11-01T00:00:00Z"}}"#
        )
    };
    fs::write(
        state.join("loopherd/ledger.json"),
        format!("{{{}, {}}}", entry("lh-1", "w1:p1"), entry("lh-2", "w1:p2")),
    )
    .unwrap();
}

fn loopherd(state: &Path, pane: Option<&str>, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    cmd.args(args)
        .env("XDG_STATE_HOME", state)
        .env("PATH", "")
        .env_remove("HERDR_PANE_ID");
    if let Some(pane) = pane {
        cmd.env("HERDR_PANE_ID", pane);
    }
    cmd
}

fn run(state: &Path, pane: Option<&str>, args: &[&str]) -> Output {
    loopherd(state, pane, args).output().unwrap()
}

#[test]
fn a_member_reports_and_the_shepherd_reads_it_once() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());

    let out = run(
        state.path(),
        Some("w1:p2"),
        &["report", "--kind", "done", "PR #9"],
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));

    let out = run(state.path(), Some("w1:p1"), &["reports"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("lh-2 done 20"), "{stdout}");
    assert!(stdout.ends_with(" PR #9\n"), "{stdout}");
    assert_eq!(stdout.lines().count(), 1);

    let out = run(state.path(), None, &["reports"]);
    assert_eq!(out.stdout, b"");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_stray_pane_is_refused_and_nothing_is_written() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());

    let out = run(state.path(), Some("w1:p7"), &["report", "hello"]);

    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd report: pane w1:p7 is not a member\n"
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(!state.path().join("loopherd/reports").exists());
}

#[test]
fn concurrent_reports_from_two_processes_do_not_interleave() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    // Large bodies, so an unsynchronised writer would split them across writes.
    let bodies: Vec<String> = ["a", "b"].iter().map(|c| c.repeat(100 * 1024)).collect();

    let children: Vec<_> = (0..8)
        .flat_map(|_| bodies.iter())
        .map(|body| {
            loopherd(state.path(), None, &["report", "--as", "lh-1", body])
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    let text = fs::read_to_string(state.path().join("loopherd/reports/lh-1.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 16);
    for line in text.lines() {
        let report: serde_json::Value = serde_json::from_str(line).unwrap();
        let body = report["text"].as_str().unwrap();
        assert!(bodies.iter().any(|b| b == body));
    }
}

#[test]
fn a_bad_line_warns_on_stderr_and_does_not_jam_the_mailbox() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    run(state.path(), None, &["report", "--as", "lh-1", "one"]);
    let mailbox = state.path().join("loopherd/reports/lh-1.jsonl");
    let mut text = fs::read_to_string(&mailbox).unwrap();
    text.push_str("{\"at\":");
    fs::write(&mailbox, text).unwrap();
    run(state.path(), None, &["report", "--as", "lh-1", "two"]);

    let out = run(state.path(), None, &["reports"]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("loopherd reports: warning: skipped a bad line in "),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
    assert_eq!(out.status.code(), Some(0));
}
