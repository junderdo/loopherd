//! `loopherd retire` without `--close`, through the binary.
//! Without a stub `herdr` on PATH, herdr can't be reached and no member is at work.

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

#[test]
fn a_lead_can_be_retired_from_its_pane_id_on_another_socket() {
    let state = tempfile::tempdir().unwrap();
    write_herd_ledger(state.path());
    // --force: from another session retire refuses the lead for that alone.
    let out = shell_in(
        state.path(),
        Some(LEAD_PANE),
        "HERDR_SOCKET_PATH=/other.sock loopherd retire --force lead-k",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lead-k\n");
}

/// Runs `loopherd retire <args> lh-1` with a running `herdr` on PATH that
/// shows `lh-1` in `status` in pane `w1:p1` and logs each call to `bin/calls`.
fn retire_while_herdr_shows_it(status: &str, state: &Path, args: &[&str]) -> Output {
    let bin = state.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let herdr = bin.join("herdr");
    let agents = r#"{"result": {"agents": [{"name": "lh-1", "pane_id": "w1:p1", "tab_id": "w1:t1", "agent_status": "STATUS", "agent_session": null}]}}"#
        .replace("STATUS", status);
    fs::write(
        &herdr,
        format!(
            r#"#!/bin/sh
echo "$@" >> {calls}
case "$1 $2" in
  "status server") echo '{{"running": true}}' ;;
  "agent list") echo '{agents}' ;;
  "workspace list") echo '{{"result": {{"workspaces": []}}}}' ;;
  *) echo '{{"result": {{}}}}' ;;
esac
"#,
            calls = bin.join("calls").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("retire")
        .args(args)
        .arg("lh-1")
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH")
        .output()
        .unwrap()
}

fn write_mailbox(state: &Path, lines: &[&str]) {
    let reports = state.join("loopherd/reports");
    fs::create_dir_all(&reports).unwrap();
    fs::write(reports.join("lh-1.jsonl"), lines.join("\n") + "\n").unwrap();
}

const STARTED: &str = r#"{"at": "2026-11-01T09:00:00Z", "kind": "start"}"#;
const DONE: &str = r#"{"at": "2026-11-01T10:00:00Z", "kind": "done", "text": "PR #9"}"#;

#[test]
fn an_idle_member_without_a_report_since_its_start_is_refused_and_left_alone() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    write_mailbox(state.path(), &[DONE, STARTED]);

    for args in [&[][..], &["--close"]] {
        let out = retire_while_herdr_shows_it("idle", state.path(), args);

        assert!(!out.status.success(), "{args:?}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "loopherd retire: herdr still shows lh-1 (idle) and it has no done, question or handoff report since its last start; pass --force to retire it anyway\n"
        );
        assert!(!ledger_text(state.path()).contains("\"retired\": true"));
    }
}

#[test]
fn force_retires_a_working_member() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());

    let out = retire_while_herdr_shows_it("working", state.path(), &["--force"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lh-1\n");
    assert!(ledger_text(state.path()).contains("\"retired\": true"));
}

#[test]
fn force_with_close_retires_a_working_member_and_looks_for_its_tab() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());

    let out = retire_while_herdr_shows_it("working", state.path(), &["--force", "--close"]);

    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("retired lh-1\n"), "{stdout}");
    assert!(ledger_text(state.path()).contains("\"retired\": true"));
    let calls = fs::read_to_string(state.path().join("bin/calls")).unwrap();
    assert!(calls.contains("workspace list"), "{calls}");
}

#[test]
fn an_idle_member_that_reported_done_since_its_start_retires_without_force() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    write_mailbox(state.path(), &[STARTED, DONE]);

    let out = retire_while_herdr_shows_it("idle", state.path(), &[]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "retired lh-1\n");
    assert!(ledger_text(state.path()).contains("\"retired\": true"));
}

const STOPPED: &str = r#"{"at": "2026-11-01T09:30:00Z", "kind": "stop", "session": "s1", "crons": 0, "background": {"shells": 0, "subagents": 0, "other": 0}}"#;

fn write_transcript(state: &Path) {
    let project = state.join("claude/projects/-repo");
    fs::create_dir_all(&project).unwrap();
    let line = r#"{"type": "assistant", "timestamp": "2026-11-01T09:10:00Z", "message": {"id": "m1", "model": "opus", "usage": {"input_tokens": 1, "cache_creation_input_tokens": 2, "cache_read_input_tokens": 3, "output_tokens": 4}}}"#;
    fs::write(project.join("s1.jsonl"), format!("{line}\n{line}\n")).unwrap();
}

fn last_entry(state: &Path) -> serde_json::Value {
    let text = fs::read_to_string(state.join("loopherd/reports/lh-1.jsonl")).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

#[test]
fn retiring_records_the_members_tokens_in_its_mailbox() {
    for args in [&["--force"][..], &["--force", "--close"]] {
        let state = tempfile::tempdir().unwrap();
        write_ledger(state.path());
        write_mailbox(state.path(), &[STARTED, STOPPED]);
        write_transcript(state.path());

        let out = retire_while_herdr_shows_it("working", state.path(), args);

        assert!(out.status.success(), "{args:?}: {out:?}");
        let entry = last_entry(state.path());
        assert_eq!(entry["kind"], "usage", "{args:?}: {entry}");
        assert_eq!(
            entry["models"],
            serde_json::json!({
                "opus": {"input": 1, "cache_write": 2, "cache_read": 3, "output": 4},
            }),
            "{args:?}"
        );
    }
}

#[test]
fn retiring_a_member_with_no_transcript_records_no_tokens() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    write_mailbox(state.path(), &[STARTED, STOPPED]);

    let out = retire_while_herdr_shows_it("working", state.path(), &["--force"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(last_entry(state.path())["kind"], "stop");
}

#[test]
fn rerunning_close_records_tokens_a_failed_retire_left_out_and_never_twice() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    write_mailbox(state.path(), &[STARTED, STOPPED]);
    let out = retire_while_herdr_shows_it("working", state.path(), &["--force"]);
    assert!(out.status.success(), "{out:?}");
    write_mailbox(state.path(), &[STARTED, STOPPED]);
    write_transcript(state.path());

    for _ in 0..2 {
        let out = retire_while_herdr_shows_it("working", state.path(), &["--close"]);
        assert!(out.status.success(), "{out:?}");
    }

    let text = fs::read_to_string(state.path().join("loopherd/reports/lh-1.jsonl")).unwrap();
    assert_eq!(text.matches("\"kind\":\"usage\"").count(), 1, "{text}");
    let ledger: serde_json::Value = serde_json::from_str(&ledger_text(state.path())).unwrap();
    let retired_at = ledger["lh-1"]["retired_at"]
        .as_str()
        .unwrap()
        .trim_end_matches('Z');
    assert!(
        last_entry(state.path())["at"]
            .as_str()
            .unwrap()
            .starts_with(retired_at),
        "stamped at the retirement, where stats reads it: {text}"
    );
}
