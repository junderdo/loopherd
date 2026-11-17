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

const SOCKET: &str = "/run/herdr-test.sock";
const LEAD_PANE: &str = "w1:p1";

fn write_herd_ledger(state: &Path) {
    fs::create_dir_all(state.join("loopherd")).unwrap();
    let entry = |name: &str, pane: &str, role: &str| {
        format!(
            r#""{name}": {{"name": "{name}", "pane_id": "{pane}", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
            "socket": "{SOCKET}", "herd": "k"{role}}}"#
        )
    };
    let ledger = format!(
        "{{{}, {}}}",
        entry("lead-k", LEAD_PANE, r#", "role": "lead""#),
        entry("lh-1", "w1:p2", ""),
    );
    fs::write(state.join("loopherd/ledger.json"), ledger).unwrap();
}

fn ledger_text(state: &Path) -> String {
    fs::read_to_string(state.join("loopherd/ledger.json")).unwrap()
}

/// Runs `script` under `sh` in `pane`, with the built binary on PATH.
fn shell_in(state: &Path, pane: Option<&str>, script: &str) -> Output {
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_loopherd")).parent().unwrap();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", script])
        .current_dir(state)
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
        .env("HERDR_SOCKET_PATH", SOCKET);
    match pane {
        Some(pane) => cmd.env("HERDR_PANE_ID", pane),
        None => cmd.env_remove("HERDR_PANE_ID"),
    };
    cmd.output().unwrap()
}

fn assert_refused(state: &Path, out: &Output) {
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("retiring a lead is the shepherd's or the human's job"),
        "{stderr}"
    );
    assert!(!ledger_text(state).contains("\"retired\": true"));
}

#[test]
fn a_lead_cannot_retire_itself_in_its_own_pane() {
    let state = tempfile::tempdir().unwrap();
    write_herd_ledger(state.path());
    for script in ["loopherd retire lead-k", "loopherd retire --close lead-k"] {
        let out = shell_in(state.path(), Some(LEAD_PANE), script);
        assert_refused(state.path(), &out);
    }
}

#[test]
fn xargs_and_quoting_spellings_of_a_self_retire_are_refused() {
    let state = tempfile::tempdir().unwrap();
    write_herd_ledger(state.path());
    let before_i = [
        "--max-args 1",
        "--max-procs 2",
        // GNU xargs takes these two values only after `=`.
        "--max-lines=1",
        "--eof=x",
        "--process-slot-var N",
        "--max-chars 100",
    ];
    let mut scripts: Vec<String> = before_i
        .iter()
        .map(|opt| format!("echo lead-k | xargs {opt} -I X loopherd retire X"))
        .collect();
    scripts.push("printf lead-k | xargs --delimiter , -I X loopherd retire X".into());
    scripts.push("echo lead-k > names; xargs --arg-file names -I X loopherd retire X".into());
    scripts.push(r#"echo lead-k | xargs -I X sh -c 'loopherd r""etire X'"#.into());
    scripts.push(r#"echo lead-k | xargs -I X sh -c 'loopherd "re"tire X'"#.into());
    for script in scripts {
        let out = shell_in(state.path(), Some(LEAD_PANE), &script);
        assert!(!out.status.success(), "{script}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("retiring a lead is the shepherd's or the human's job"),
            "{script}: {stderr}"
        );
        assert!(
            !ledger_text(state.path()).contains("\"retired\": true"),
            "{script}"
        );
    }
}

#[test]
fn a_lead_can_be_retired_from_another_pane() {
    for pane in [None, Some("w9:p9")] {
        let state = tempfile::tempdir().unwrap();
        write_herd_ledger(state.path());
        let out = shell_in(state.path(), pane, "loopherd retire lead-k");
        assert!(out.status.success(), "{pane:?}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lead-k\n");
    }
}

#[test]
fn a_lead_can_retire_its_own_workers() {
    let state = tempfile::tempdir().unwrap();
    write_herd_ledger(state.path());
    let out = shell_in(state.path(), Some(LEAD_PANE), "loopherd retire lh-1");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lh-1\n");
}
