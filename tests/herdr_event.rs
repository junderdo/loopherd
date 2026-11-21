//! `loopherd herdr-event-hook`, as the herdr plugin's event hooks run it,
//! with herdr off PATH except a fake that answers a move's shell lookup.

mod fakes;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SOCKET: &str = "/run/herdr/herdr.sock";

/// Members of the herdr at [`SOCKET`]: lh-1 in pane w1:p1 of tab w1:t1, the
/// retired old-1 in w1:p9 of the same tab, lh-0 in w1:p5 with no tab recorded,
/// lh-2 in w2:p1 of tab w2:t1, and lead-1, the lead of herd h with no herd
/// file, so every switch off, in w4:p1 of tab w4:t1.
fn sandbox() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state/loopherd");
    fs::create_dir_all(&state).unwrap();
    let entry = |name: &str, pane: &str, tab: Option<&str>, retired: bool| {
        let role = match name.starts_with("lead") {
            true => r#""role": "lead", "herd": "h","#,
            false => "",
        };
        let workspace = pane.split(':').next().unwrap();
        let tab = tab
            .map(|t| format!(r#""tab_id": "{t}","#))
            .unwrap_or_default();
        format!(
            r#""{name}": {{"name": "{name}", "pane_id": "{pane}", {tab} {role} "session_id": null,
                "repo": "/", "worktree": null, "workspace": "{workspace}",
                "workspace_label": "label-{workspace}", "prompt": null,
                "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
                "socket": "{SOCKET}", "retired": {retired}}}"#
        )
    };
    fs::write(
        state.join("ledger.json"),
        format!(
            "{{{}, {}, {}, {}, {}}}",
            entry("lh-1", "w1:p1", Some("w1:t1"), false),
            entry("old-1", "w1:p9", Some("w1:t1"), true),
            entry("lh-0", "w1:p5", None, false),
            entry("lh-2", "w2:p1", Some("w2:t1"), false),
            entry("lead-1", "w4:p1", Some("w4:t1"), false)
        ),
    )
    .unwrap();
    fs::create_dir(dir.path().join("bin")).unwrap();
    fakes::executable(
        &dir.path().join("bin/herdr"),
        "#!/bin/sh\nprintf '{\"result\":{\"process_info\":{\"shell_pid\":%s}}}' \"$FAKE_SHELL_PID\"\n",
    );
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

/// herdr's `pane.moved` for a pane moved from `from` in tab `from_tab` to
/// `to` in tab `to_tab`; a move to another workspace gives the pane a new id.
/// The pane's shell is this test process, so every command it runs is under it.
fn moved(dir: &Path, from: (&str, &str), to: (&str, &str)) -> Output {
    moved_under(dir, std::process::id(), from, to)
}

/// [`moved`] with herdr naming `shell` as the pane's shell.
fn moved_under(
    dir: &Path,
    shell: u32,
    (from, from_tab): (&str, &str),
    (to, to_tab): (&str, &str),
) -> Output {
    let workspace = |id: &str| id.split(':').next().unwrap().to_string();
    let data = format!(
        r#"{{"pane":{{"pane_id":"{to}","tab_id":"{to_tab}","workspace_id":"{}"}},
                "previous_pane_id":"{from}","previous_tab_id":"{from_tab}",
                "previous_workspace_id":"{}","type":"pane_moved"}}"#,
        workspace(to),
        workspace(from)
    );
    loopherd(dir, &["herdr-event-hook"])
        .env("PATH", dir.join("bin"))
        .env("FAKE_SHELL_PID", shell.to_string())
        .env("HERDR_PLUGIN_EVENT", "pane.moved")
        .env(
            "HERDR_PLUGIN_EVENT_JSON",
            format!(r#"{{"event":"pane_moved","data":{data}}}"#),
        )
        .output()
        .unwrap()
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
    fs::create_dir_all(dir.path().join("state/loopherd/reports/lh-1/mailbox.jsonl")).unwrap();

    failed_quietly(
        &closed(dir.path(), "pane.closed", "w1:p1"),
        "lh-1/mailbox.jsonl",
    );
    failed_quietly(
        &status(dir.path(), "w1:p1", "blocked"),
        "lh-1/mailbox.jsonl",
    );
    let mark = dir
        .path()
        .join("state/loopherd/reports/lh-1.block-recorded");
    assert!(!mark.exists(), "the failed block left its mark");
}

#[test]
fn a_closed_workspace_records_every_member_it_can_and_names_the_ones_it_cannot() {
    let dir = sandbox();
    let reports = dir.path().join("state/loopherd/reports");
    fs::create_dir_all(reports.join("lh-0/mailbox.jsonl")).unwrap();

    failed_quietly(&workspace_closed(dir.path(), "w1"), "lh-0/mailbox.jsonl");

    let lh1 = fs::read_to_string(reports.join("lh-1/mailbox.jsonl")).unwrap();
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

#[test]
fn a_member_moved_to_another_tab_goes_with_its_new_tab_not_its_old_one() {
    let dir = sandbox();

    quiet_success(&moved(dir.path(), ("w1:p1", "w1:t1"), ("w1:p1", "w1:t4")));
    quiet_success(&tab_closed(dir.path(), "w1:t1"));
    assert!(recorded(dir.path()).is_empty());

    quiet_success(&tab_closed(dir.path(), "w1:t4"));
    assert_eq!(recorded(dir.path()), ["lh-1 gone"]);
}

#[test]
fn a_member_moved_to_another_workspace_goes_with_its_new_workspace_not_its_old_one() {
    let dir = sandbox();

    quiet_success(&moved(dir.path(), ("w2:p1", "w2:t1"), ("w3:p2", "w3:t1")));
    quiet_success(&workspace_closed(dir.path(), "w2"));
    quiet_success(&tab_closed(dir.path(), "w2:t1"));
    quiet_success(&closed(dir.path(), "pane.closed", "w2:p1"));
    assert!(recorded(dir.path()).is_empty());

    quiet_success(&workspace_closed(dir.path(), "w3"));
    assert_eq!(recorded(dir.path()), ["lh-2 gone"]);
}

#[test]
fn a_member_moved_to_another_workspace_goes_with_its_new_tab_and_pane() {
    for close in [
        |dir: &Path| tab_closed(dir, "w3:t1"),
        |dir: &Path| closed(dir, "pane.closed", "w3:p2"),
    ] {
        let dir = sandbox();

        quiet_success(&moved(dir.path(), ("w2:p1", "w2:t1"), ("w3:p2", "w3:t1")));
        quiet_success(&close(dir.path()));

        assert_eq!(recorded(dir.path()), ["lh-2 gone"]);
    }
}

#[test]
fn a_move_from_a_tab_the_member_already_left_changes_nothing() {
    let dir = sandbox();

    quiet_success(&moved(dir.path(), ("w1:p1", "w1:t1"), ("w1:p1", "w1:t4")));
    quiet_success(&moved(dir.path(), ("w1:p1", "w1:t1"), ("w1:p1", "w1:t2")));
    quiet_success(&tab_closed(dir.path(), "w1:t2"));
    assert!(recorded(dir.path()).is_empty());

    quiet_success(&tab_closed(dir.path(), "w1:t4"));
    assert_eq!(recorded(dir.path()), ["lh-1 gone"]);
}

#[test]
fn a_move_of_a_pane_loopherd_does_not_follow_changes_nothing() {
    let dir = sandbox();
    let ledger = dir.path().join("state/loopherd/ledger.json");
    let before = fs::read_to_string(&ledger).unwrap();

    quiet_success(&moved(dir.path(), ("w1:p9", "w1:t1"), ("w1:p9", "w1:t4")));
    quiet_success(&moved(dir.path(), ("w1:p7", "w1:t1"), ("w1:p7", "w1:t4")));
    let other_herdr = loopherd(dir.path(), &["herdr-event-hook"])
        .env("HERDR_SOCKET_PATH", "/run/other/herdr.sock")
        .env("HERDR_PLUGIN_EVENT", "pane.moved")
        .env(
            "HERDR_PLUGIN_EVENT_JSON",
            r#"{"event":"pane_moved","data":{"pane":{"pane_id":"w1:p1","tab_id":"w1:t4",
                "workspace_id":"w1"},"previous_pane_id":"w1:p1","previous_tab_id":"w1:t1"}}"#,
        )
        .output()
        .unwrap();
    quiet_success(&other_herdr);
    let no_previous_tab = event(
        dir.path(),
        "pane.moved",
        r#"{"pane":{"pane_id":"w1:p1","tab_id":"w1:t4","workspace_id":"w1"},
            "previous_pane_id":"w1:p1"}"#,
    );
    quiet_success(&no_previous_tab);

    assert_eq!(fs::read_to_string(&ledger).unwrap(), before);
}

#[test]
fn a_member_with_no_recorded_tab_takes_the_tab_it_was_moved_to() {
    let dir = sandbox();

    quiet_success(&moved(dir.path(), ("w1:p5", "w1:t1"), ("w1:p5", "w1:t4")));
    quiet_success(&tab_closed(dir.path(), "w1:t4"));

    assert_eq!(recorded(dir.path()), ["lh-0 gone"]);
}

#[test]
fn a_member_moved_to_another_workspace_reports_from_its_old_or_new_pane_id() {
    let dir = sandbox();

    quiet_success(&moved(dir.path(), ("w2:p1", "w2:t1"), ("w3:p2", "w3:t1")));
    for own_pane in ["w2:p1", "w3:p2"] {
        let out = loopherd(dir.path(), &["report", "--kind", "done", own_pane])
            .env("HERDR_PANE_ID", own_pane)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
    quiet_success(&workspace_closed(dir.path(), "w3"));

    assert_eq!(
        recorded(dir.path()),
        ["lh-2 done", "lh-2 done", "lh-2 gone"]
    );
}

/// The PreToolUse hook's output for a Bash `command` run in `pane`.
fn tool_hook(dir: &Path, pane: &str, command: &str) -> String {
    let mut child = loopherd(dir, &["tool-hook"])
        .env("HERDR_PANE_ID", pane)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}});
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), input.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_lead_moved_to_another_workspace_keeps_its_guardrails() {
    let dir = sandbox();
    assert!(tool_hook(dir.path(), "w4:p1", "loopherd pause h").contains("deny"));

    quiet_success(&moved(dir.path(), ("w4:p1", "w4:t1"), ("w5:p3", "w5:t2")));

    assert!(tool_hook(dir.path(), "w4:p1", "loopherd pause h").contains("deny"));
    // A restored herdr gives the pane's new shell its current id.
    assert!(tool_hook(dir.path(), "w5:p3", "loopherd pause h").contains("deny"));
    quiet_success(&workspace_closed(dir.path(), "w5"));
    assert_eq!(recorded(dir.path()), ["lead-1 gone"]);
}

#[test]
fn a_member_moved_twice_keeps_its_first_pane_and_skips_a_late_first_move() {
    let dir = sandbox();

    quiet_success(&moved(dir.path(), ("w2:p1", "w2:t1"), ("w3:p2", "w3:t1")));
    quiet_success(&moved(dir.path(), ("w3:p2", "w3:t1"), ("w5:p1", "w5:t1")));
    quiet_success(&moved(dir.path(), ("w2:p1", "w2:t1"), ("w3:p2", "w3:t1")));
    quiet_success(&workspace_closed(dir.path(), "w3"));
    let out = loopherd(dir.path(), &["report", "--kind", "done", "shipped"])
        .env("HERDR_PANE_ID", "w2:p1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(recorded(dir.path()), ["lh-2 done"]);

    quiet_success(&workspace_closed(dir.path(), "w5"));
    assert_eq!(recorded(dir.path()), ["lh-2 done", "lh-2 gone"]);
}

#[test]
fn a_member_moved_to_another_workspace_drops_its_old_workspace_label() {
    let dir = sandbox();
    let ledger = dir.path().join("state/loopherd/ledger.json");

    quiet_success(&moved(dir.path(), ("w2:p1", "w2:t1"), ("w3:p2", "w3:t1")));

    let ledger: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(ledger).unwrap()).unwrap();
    assert_eq!(ledger["lh-2"]["workspace"], "w3");
    assert_eq!(ledger["lh-2"]["workspace_label"], serde_json::Value::Null);
}

#[test]
fn a_move_out_of_a_tab_that_then_closes_records_no_gone_in_either_hook_order() {
    let move_first = sandbox();
    quiet_success(&moved(
        move_first.path(),
        ("w1:p1", "w1:t1"),
        ("w1:p1", "w1:t4"),
    ));
    quiet_success(&tab_closed(move_first.path(), "w1:t1"));
    assert!(recorded(move_first.path()).is_empty());

    let close_first = sandbox();
    quiet_success(&tab_closed(close_first.path(), "w1:t1"));
    quiet_success(&moved(
        close_first.path(),
        ("w1:p1", "w1:t1"),
        ("w1:p1", "w1:t4"),
    ));
    assert_eq!(recorded(close_first.path()), ["lh-1 gone", "lh-1 moved"]);

    quiet_success(&tab_closed(close_first.path(), "w1:t4"));
    assert_eq!(
        recorded(close_first.path()),
        ["lh-1 gone", "lh-1 moved", "lh-1 gone"]
    );
}

#[test]
fn a_move_retracts_no_gone_but_one_its_old_tab_closing_recorded() {
    let dir = sandbox();

    quiet_success(&closed(dir.path(), "pane.exited", "w1:p1"));
    quiet_success(&moved(dir.path(), ("w1:p1", "w1:t1"), ("w1:p1", "w1:t4")));

    assert_eq!(recorded(dir.path()), ["lh-1 gone"]);
}

#[test]
fn a_move_that_empties_its_workspace_records_no_gone_in_any_hook_order() {
    let run = |dir: &Path, step: &str| match step {
        "move" => quiet_success(&moved(dir, ("w2:p1", "w2:t1"), ("w3:p2", "w3:t1"))),
        "tab" => quiet_success(&tab_closed(dir, "w2:t1")),
        _ => quiet_success(&workspace_closed(dir, "w2")),
    };
    let orders = [
        ["move", "tab", "workspace"],
        ["move", "workspace", "tab"],
        ["tab", "move", "workspace"],
        ["workspace", "move", "tab"],
        ["tab", "workspace", "move"],
        ["workspace", "tab", "move"],
    ];
    for order in orders {
        let dir = sandbox();
        order.iter().for_each(|step| run(dir.path(), step));
        let before = recorded(dir.path());
        assert!(
            before.is_empty() || before == ["lh-2 gone", "lh-2 moved"],
            "{order:?}: {before:?}"
        );

        workspace_closed(dir.path(), "w3");
        assert_eq!(
            recorded(dir.path()).last().map(String::as_str),
            Some("lh-2 gone"),
            "{order:?}"
        );
    }
}

/// A moved member's old pane id after herdr restarts and a reused workspace id
/// hands it to a new pane: the shell it was kept for is no ancestor of the
/// caller there.
#[test]
fn a_stranger_in_a_pane_that_took_a_moved_members_old_id_is_no_member() {
    let dir = sandbox();
    let mut old_shell = Command::new("sleep").arg("60").spawn().unwrap();

    quiet_success(&moved_under(
        dir.path(),
        old_shell.id(),
        ("w2:p1", "w2:t1"),
        ("w3:p2", "w3:t1"),
    ));
    quiet_success(&moved_under(
        dir.path(),
        old_shell.id(),
        ("w4:p1", "w4:t1"),
        ("w5:p3", "w5:t2"),
    ));
    let report = loopherd(dir.path(), &["report", "--kind", "done", "not mine"])
        .env("HERDR_PANE_ID", "w2:p1")
        .output()
        .unwrap();
    let guarded = tool_hook(dir.path(), "w4:p1", "loopherd pause h");
    let _ = old_shell.kill();
    let _ = old_shell.wait();

    assert!(!report.status.success(), "{report:?}");
    assert!(recorded(dir.path()).is_empty());
    assert!(!guarded.contains("deny"), "{guarded}");
    assert!(tool_hook(dir.path(), "w5:p3", "loopherd pause h").contains("deny"));
}
