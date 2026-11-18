//! `loopherd herdr-event-hook`, as the herdr plugin's event hooks run it,
//! with herdr off PATH.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SOCKET: &str = "/run/herdr/herdr.sock";

/// Members of the herdr at [`SOCKET`]: lh-1 in pane w1:p1 of tab w1:t1, the
/// retired old-1 in w1:p9 of the same tab, lh-0 in w1:p5 with no tab recorded,
/// and lh-2 in w2:p1 of tab w2:t1.
fn sandbox() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state/loopherd");
    fs::create_dir_all(&state).unwrap();
    let entry = |name: &str, pane: &str, tab: Option<&str>, retired: bool| {
        let workspace = pane.split(':').next().unwrap();
        let tab = tab
            .map(|t| format!(r#""tab_id": "{t}","#))
            .unwrap_or_default();
        format!(
            r#""{name}": {{"name": "{name}", "pane_id": "{pane}", {tab} "session_id": null,
                "repo": "/", "worktree": null, "workspace": "{workspace}", "prompt": null,
                "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
                "socket": "{SOCKET}", "retired": {retired}}}"#
        )
    };
    fs::write(
        state.join("ledger.json"),
        format!(
            "{{{}, {}, {}, {}}}",
            entry("lh-1", "w1:p1", Some("w1:t1"), false),
            entry("old-1", "w1:p9", Some("w1:t1"), true),
            entry("lh-0", "w1:p5", None, false),
            entry("lh-2", "w2:p1", Some("w2:t1"), false)
        ),
    )
    .unwrap();
    dir
}

fn loopherd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    cmd.args(args)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("LOOPHERD_CONFIG_DIR", dir.join("config"))
        .env("PATH", "")
        .env_remove("HERDR_PANE_ID")
        .env_remove("CLAUDE_PID")
        .env("HERDR_SOCKET_PATH", SOCKET);
    cmd
}

/// The hook as herdr runs it for `event` with `data`.
fn event(dir: &Path, event: &str, data: &str) -> Output {
    let kind = event.replace('.', "_");
    loopherd(dir, &["herdr-event-hook"])
        .env("HERDR_PLUGIN_EVENT", event)
        .env(
            "HERDR_PLUGIN_EVENT_JSON",
            format!(r#"{{"event":"{kind}","data":{data}}}"#),
        )
        .output()
        .unwrap()
}

fn status(dir: &Path, pane: &str, status: &str) -> Output {
    event(
        dir,
        "pane.agent_status_changed",
        &format!(r#"{{"pane_id":"{pane}","workspace_id":"w1","agent_status":"{status}"}}"#),
    )
}

fn closed(dir: &Path, event_name: &str, pane: &str) -> Output {
    event(
        dir,
        event_name,
        &format!(r#"{{"pane_id":"{pane}","workspace_id":"w1"}}"#),
    )
}

fn tab_closed(dir: &Path, tab: &str) -> Output {
    let workspace = tab.split(':').next().unwrap();
    event(
        dir,
        "tab.closed",
        &format!(r#"{{"tab_id":"{tab}","workspace_id":"{workspace}"}}"#),
    )
}

fn workspace_closed(dir: &Path, workspace: &str) -> Output {
    event(
        dir,
        "workspace.closed",
        &format!(r#"{{"workspace_id":"{workspace}"}}"#),
    )
}

/// `reports --all`, each line cut to its member and kind.
fn recorded(dir: &Path) -> Vec<String> {
    let out = loopherd(dir, &["reports", "--all"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.split(' ').take(2).collect::<Vec<_>>().join(" "))
        .collect()
}

fn quiet_success(out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
}

#[test]
fn a_member_going_blocked_is_recorded_once_until_it_unblocks() {
    let dir = sandbox();

    quiet_success(&status(dir.path(), "w1:p1", "working"));
    quiet_success(&status(dir.path(), "w1:p1", "blocked"));
    quiet_success(&status(dir.path(), "w1:p1", "blocked"));
    assert_eq!(recorded(dir.path()), ["lh-1 blocked"]);

    quiet_success(&status(dir.path(), "w1:p1", "idle"));
    quiet_success(&status(dir.path(), "w1:p1", "blocked"));
    assert_eq!(recorded(dir.path()), ["lh-1 blocked", "lh-1 blocked"]);
}

#[test]
fn a_members_pane_exiting_and_closing_records_one_gone() {
    let dir = sandbox();

    quiet_success(&closed(dir.path(), "pane.exited", "w1:p1"));
    quiet_success(&closed(dir.path(), "pane.closed", "w1:p1"));

    assert_eq!(recorded(dir.path()), ["lh-1 gone"]);
}

#[test]
fn a_members_tab_closing_records_gone_for_the_open_members_in_it() {
    let dir = sandbox();

    quiet_success(&tab_closed(dir.path(), "w1:t1"));
    quiet_success(&tab_closed(dir.path(), "w1:t1"));

    assert_eq!(recorded(dir.path()), ["lh-1 gone"]);
}

#[test]
fn a_workspace_closing_records_gone_for_every_open_member_in_it() {
    let dir = sandbox();

    quiet_success(&workspace_closed(dir.path(), "w1"));
    quiet_success(&tab_closed(dir.path(), "w1:t1"));

    let mut gone = recorded(dir.path());
    gone.sort();
    assert_eq!(gone, ["lh-0 gone", "lh-1 gone"]);
}

#[test]
fn a_tab_or_workspace_of_another_herdr_records_nothing() {
    let dir = sandbox();

    for (event, data) in [
        ("tab.closed", r#"{"tab_id":"w1:t1","workspace_id":"w1"}"#),
        ("workspace.closed", r#"{"workspace_id":"w1"}"#),
    ] {
        let other_herdr = loopherd(dir.path(), &["herdr-event-hook"])
            .env("HERDR_SOCKET_PATH", "/run/other/herdr.sock")
            .env("HERDR_PLUGIN_EVENT", event)
            .env(
                "HERDR_PLUGIN_EVENT_JSON",
                format!(r#"{{"event":"{}","data":{data}}}"#, event.replace('.', "_")),
            )
            .output()
            .unwrap();
        quiet_success(&other_herdr);
    }
    quiet_success(&tab_closed(dir.path(), "w1:t7"));
    quiet_success(&workspace_closed(dir.path(), "w9"));

    assert!(recorded(dir.path()).is_empty());
}

#[test]
fn panes_loopherd_does_not_know_record_nothing() {
    let dir = sandbox();

    quiet_success(&closed(dir.path(), "pane.closed", "w1:p7"));
    quiet_success(&status(dir.path(), "w1:p9", "blocked"));
    let other_herdr = loopherd(dir.path(), &["herdr-event-hook"])
        .env("HERDR_SOCKET_PATH", "/run/other/herdr.sock")
        .env("HERDR_PLUGIN_EVENT", "pane.closed")
        .env(
            "HERDR_PLUGIN_EVENT_JSON",
            r#"{"event":"pane_closed","data":{"pane_id":"w1:p1","workspace_id":"w1"}}"#,
        )
        .output()
        .unwrap();
    quiet_success(&other_herdr);

    assert!(recorded(dir.path()).is_empty());
}

fn failed_quietly(out: &Output, error: &str) {
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("loopherd herdr-event-hook: ") && stderr.contains(error),
        "{out:?}"
    );
}

#[test]
fn a_malformed_or_missing_event_exits_0_with_the_error() {
    let dir = sandbox();

    let malformed = loopherd(dir.path(), &["herdr-event-hook"])
        .env("HERDR_PLUGIN_EVENT", "pane.closed")
        .env("HERDR_PLUGIN_EVENT_JSON", "{not json")
        .output()
        .unwrap();
    failed_quietly(&malformed, "parse HERDR_PLUGIN_EVENT_JSON");

    let missing = loopherd(dir.path(), &["herdr-event-hook"])
        .env_remove("HERDR_PLUGIN_EVENT")
        .env_remove("HERDR_PLUGIN_EVENT_JSON")
        .output()
        .unwrap();
    failed_quietly(&missing, "no HERDR_PLUGIN_EVENT");

    assert!(recorded(dir.path()).is_empty());
}

#[test]
fn a_mailbox_it_cannot_write_exits_0_with_the_error() {
    let dir = sandbox();
    fs::create_dir_all(dir.path().join("state/loopherd/reports/lh-1.jsonl")).unwrap();

    failed_quietly(&closed(dir.path(), "pane.closed", "w1:p1"), "lh-1.jsonl");
    failed_quietly(&status(dir.path(), "w1:p1", "blocked"), "lh-1.jsonl");
    let mark = dir
        .path()
        .join("state/loopherd/reports/lh-1.block-recorded");
    assert!(!mark.exists(), "the failed block left its mark");
}

#[test]
fn a_closed_workspace_records_every_member_it_can_and_names_the_ones_it_cannot() {
    let dir = sandbox();
    let reports = dir.path().join("state/loopherd/reports");
    fs::create_dir_all(reports.join("lh-0.jsonl")).unwrap();

    failed_quietly(&workspace_closed(dir.path(), "w1"), "lh-0.jsonl");

    let lh1 = fs::read_to_string(reports.join("lh-1.jsonl")).unwrap();
    assert!(lh1.contains(r#""kind":"gone""#), "{lh1}");
}

#[test]
fn a_member_that_shows_life_after_its_gone_records_gone_again() {
    let dir = sandbox();

    quiet_success(&closed(dir.path(), "pane.exited", "w1:p1"));
    let report = loopherd(
        dir.path(),
        &["report", "--as", "lh-1", "--kind", "question", "which DB?"],
    )
    .output()
    .unwrap();
    assert!(report.status.success(), "{report:?}");
    quiet_success(&closed(dir.path(), "pane.closed", "w1:p1"));

    assert_eq!(
        recorded(dir.path()),
        ["lh-1 gone", "lh-1 question", "lh-1 gone"]
    );
}

#[test]
fn a_note_after_a_gone_does_not_record_gone_again() {
    let dir = sandbox();

    quiet_success(&closed(dir.path(), "pane.exited", "w1:p1"));
    let note = loopherd(dir.path(), &["report", "--as", "lh-1", "revived"])
        .output()
        .unwrap();
    assert!(note.status.success(), "{note:?}");
    quiet_success(&closed(dir.path(), "pane.closed", "w1:p1"));

    assert_eq!(recorded(dir.path()), ["lh-1 gone", "lh-1 note"]);
}
