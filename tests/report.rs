//! `loopherd report` and `loopherd reports` through the binary.

mod common;

use common::feed_stdin;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SOCKET: &str = "/run/herdr/herdr.sock";

/// lh-1 recorded without a socket, lh-2 in the herdr at [`SOCKET`].
fn write_ledger(state: &Path) {
    fs::create_dir_all(state.join("loopherd")).unwrap();
    let entry = |name: &str, pane: &str, socket: Option<&str>| {
        let socket = socket.map_or("null".into(), |s| format!("\"{s}\""));
        format!(
            r#""{name}": {{"name": "{name}", "pane_id": "{pane}", "session_id": null,
                "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
                "prompt_sent": true, "created": "2026-11-01T00:00:00Z", "socket": {socket}}}"#
        )
    };
    fs::write(
        state.join("loopherd/ledger.json"),
        format!(
            "{{{}, {}}}",
            entry("lh-1", "w1:p1", None),
            entry("lh-2", "w1:p2", Some(SOCKET))
        ),
    )
    .unwrap();
}

/// Run from `pane` of the herdr at [`SOCKET`], or outside herdr.
fn loopherd(state: &Path, pane: Option<&str>, args: &[&str]) -> Command {
    loopherd_in(state, pane.map(|p| (p, SOCKET)), args)
}

fn loopherd_in(state: &Path, pane: Option<(&str, &str)>, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    cmd.args(args)
        .env("XDG_STATE_HOME", state)
        // Never the real ~/.claude/settings.json.
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("PATH", "")
        // Printed times are UTC whatever the local zone.
        .env("TZ", "America/New_York")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH");
    if let Some((pane, socket)) = pane {
        cmd.env("HERDR_PANE_ID", pane)
            .env("HERDR_SOCKET_PATH", socket);
    }
    cmd
}

fn run(state: &Path, pane: Option<&str>, args: &[&str]) -> Output {
    loopherd(state, pane, args).output().unwrap()
}

/// Into `state`'s own Claude settings, which `watch` requires.
fn install_hook(state: &Path) {
    let out = run(state, None, &["install-hook"]);
    assert!(out.status.success(), "{out:?}");
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
fn reports_and_watch_label_each_time_utc_and_keep_an_uncut_time_as_stored() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    install_hook(state.path());
    let mailbox = state.path().join("loopherd/reports");
    fs::create_dir_all(&mailbox).unwrap();
    fs::write(
        mailbox.join("lh-1.jsonl"),
        concat!(
            r#"{"at":"2026-11-10T10:00:00Z","kind":"start"}"#,
            "\n",
            r#"{"at":"2026-11-10T11:00:00Z","kind":"done","text":"PR #9"}"#,
            "\n",
            r#"{"at":"2026-11-10T11:05:00Z","kind":"stop","background":{"shells":0,"subagents":0,"other":0},"crons":0}"#,
            "\n",
            r#"{"at":"soon","kind":"note","text":"odd"}"#,
            "\n",
        ),
    )
    .unwrap();

    let all = run(state.path(), None, &["reports", "--all"]);
    assert_eq!(
        String::from_utf8_lossy(&all.stdout),
        "lh-1 start 2026-11-10 10:00 UTC\n\
         lh-1 done 2026-11-10 11:00 UTC PR #9\n\
         lh-1 stop 2026-11-10 11:05 UTC finished\n\
         lh-1 note soon odd\n"
    );

    let watched = run(state.path(), None, &["watch", "--timeout", "0"]);
    let watched = String::from_utf8_lossy(&watched.stdout);
    assert!(
        watched.contains("lh-1 done 2026-11-10 11:00 UTC PR #9\n"),
        "{watched}"
    );
}

#[test]
fn a_stray_pane_is_refused_and_nothing_is_written() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());

    let out = run(state.path(), Some("w1:p7"), &["report", "hello"]);

    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        format!("loopherd report: pane w1:p7 of {SOCKET} is not a member\n")
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

#[test]
fn watch_wakes_on_an_unread_report_and_times_out_silently_once_read() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    install_hook(state.path());
    run(state.path(), None, &["report", "--as", "lh-2", "PR #9"]);

    let out = run(state.path(), None, &["watch", "--timeout", "0"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("lh-2 note 20"), "{stdout}");
    assert!(stdout.ends_with(" PR #9\n"), "{stdout}");
    assert_eq!(out.status.code(), Some(0));

    let out = run(state.path(), None, &["watch", "--timeout", "0"]);
    assert_eq!(out.stdout, b"");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_handoff_report_wakes_watch_with_a_handoff_line() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    install_hook(state.path());
    run(
        state.path(),
        None,
        &[
            "report",
            "--kind",
            "handoff",
            "--as",
            "lh-2",
            "mid-refactor",
        ],
    );

    let out = run(state.path(), None, &["watch", "--timeout", "0"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("lh-2 handoff 20"), "{stdout}");
    assert!(stdout.ends_with(" mid-refactor\n"), "{stdout}");
}

#[test]
fn watch_warns_about_a_herd_with_no_herd_file() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    install_hook(state.path());
    fs::create_dir_all(config.path().join("herds")).unwrap();
    fs::write(config.path().join("herds/cards.toml"), "").unwrap();

    let out = loopherd(
        state.path(),
        None,
        &["watch", "cards", "crads", "--timeout", "0"],
    )
    .env("LOOPHERD_CONFIG_DIR", config.path())
    .output()
    .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("loopherd watch: warning: no herd file for crads"),
        "{stderr}"
    );
    assert!(!stderr.contains("for cards"), "{stderr}");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_pane_of_another_herdr_is_not_a_member_with_or_without_a_recorded_socket() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());

    let out = loopherd_in(
        state.path(),
        Some(("w1:p2", "/run/other/herdr.sock")),
        &["report", "hello"],
    )
    .output()
    .unwrap();

    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd report: pane w1:p2 of /run/other/herdr.sock is not a member\n"
    );
    assert!(!state.path().join("loopherd/reports").exists());

    let out = loopherd_in(
        state.path(),
        Some(("w1:p1", "/run/other/herdr.sock")),
        &["report", "hello"],
    )
    .output()
    .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd report: pane w1:p1 of /run/other/herdr.sock is not a member\n",
        "lh-1 has no socket, so a pane of any herdr is not it"
    );
    assert!(!state.path().join("loopherd/reports").exists());
}

fn stop_hook(state: &Path, pane: Option<(&str, &str)>, input: &str) -> Output {
    let mut child = loopherd_in(state, pane, &["stop-hook"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    feed_stdin(child.stdin.take().unwrap(), input.as_bytes());
    child.wait_with_output().unwrap()
}

#[test]
fn the_stop_hook_files_a_members_stops_which_only_reports_all_shows() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    let paused =
        r#"{"hook_event_name":"Stop","background_tasks":[{"type":"shell"}],"session_crons":[]}"#;
    let finished = r#"{"hook_event_name":"Stop","background_tasks":[],"session_crons":[]}"#;

    let out = stop_hook(state.path(), Some(("w1:p2", SOCKET)), paused);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
    run(
        state.path(),
        Some("w1:p2"),
        &["report", "--kind", "done", "PR #9"],
    );
    stop_hook(state.path(), Some(("w1:p2", SOCKET)), finished);

    let unread = run(state.path(), None, &["reports"]);
    let unread = String::from_utf8_lossy(&unread.stdout);
    assert_eq!(unread.lines().count(), 1, "{unread}");
    assert!(unread.contains("lh-2 done "), "{unread}");

    let all = run(state.path(), None, &["reports", "--all"]);
    let kinds: Vec<String> = String::from_utf8_lossy(&all.stdout)
        .lines()
        .map(|l| {
            let (_, rest) = l.split_once(' ').unwrap();
            let (kind, rest) = rest.split_once(' ').unwrap();
            format!("{kind}{}", rest.get(20..).unwrap_or_default())
        })
        .collect();
    assert_eq!(
        kinds,
        ["stop paused: 1 shell", "done PR #9", "stop finished"]
    );
}

#[test]
fn the_stop_hook_records_nothing_outside_a_members_pane_and_exits_0() {
    for pane in [
        None,
        Some(("w1:p2", "/run/other/herdr.sock")),
        Some(("w1:p9", SOCKET)),
    ] {
        let state = tempfile::tempdir().unwrap();
        write_ledger(state.path());

        let out = stop_hook(state.path(), pane, "{}");

        assert_eq!(out.status.code(), Some(0), "{pane:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{pane:?}");
        assert!(!state.path().join("loopherd/reports").exists(), "{pane:?}");
    }
}

#[test]
fn past_its_limit_the_stop_hook_prints_one_block_decision_and_files_nothing() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    fs::write(
        config.path().join("config.toml"),
        "default_context_limit = 10000\n",
    )
    .unwrap();
    let transcript = state.path().join("t.jsonl");
    fs::write(
        &transcript,
        r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":20000}}}"#,
    )
    .unwrap();
    let input = format!(
        r#"{{"hook_event_name":"Stop","session_id":"s1","transcript_path":"{}","background_tasks":[],"session_crons":[]}}"#,
        transcript.display()
    );
    let hook = || {
        let mut cmd = loopherd_in(state.path(), Some(("w1:p2", SOCKET)), &["stop-hook"]);
        cmd.env("LOOPHERD_CONFIG_DIR", config.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        feed_stdin(child.stdin.take().unwrap(), input.as_bytes());
        child.wait_with_output().unwrap()
    };

    let out = hook();
    assert_eq!(out.status.code(), Some(0));
    let decision: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(decision["decision"], "block");
    let reason = decision["reason"].as_str().unwrap();
    let handoff = state.path().join("loopherd/handoffs/lh-2.md");
    assert!(
        reason.contains("20005 tokens, past your limit of 10000"),
        "{reason}"
    );
    assert!(reason.contains(&handoff.display().to_string()), "{reason}");
    let all = run(state.path(), None, &["reports", "--all"]);
    assert_eq!(all.stdout, b"");

    let out = hook();
    assert_eq!(out.stdout, b"");
    let all = run(state.path(), None, &["reports", "--all"]);
    assert!(
        String::from_utf8_lossy(&all.stdout).contains("lh-2 stop "),
        "{all:?}"
    );
}

/// A Stop of session `session` at `tokens` tokens in context, from lh-2's
/// pane, with `config` as `$LOOPHERD_CONFIG_DIR`.
fn stop_at(state: &Path, config: &Path, session: &str, tokens: u64) -> Output {
    let transcript = state.join(format!("{session}.jsonl"));
    fs::write(
        &transcript,
        format!(
            r#"{{"type":"assistant","isSidechain":false,"message":{{"usage":{{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":{tokens}}}}}}}"#
        ),
    )
    .unwrap();
    let input = format!(
        r#"{{"hook_event_name":"Stop","session_id":"{session}","transcript_path":"{}","background_tasks":[],"session_crons":[]}}"#,
        transcript.display()
    );
    let mut child = loopherd_in(state, Some(("w1:p2", SOCKET)), &["stop-hook"])
        .env("LOOPHERD_CONFIG_DIR", config)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    feed_stdin(child.stdin.take().unwrap(), input.as_bytes());
    child.wait_with_output().unwrap()
}

fn notes(state: &Path) -> Vec<String> {
    let all = run(state, None, &["reports", "--all"]);
    String::from_utf8_lossy(&all.stdout)
        .lines()
        .filter(|l| l.starts_with("lh-2 note "))
        .map(str::to_string)
        .collect()
}

#[test]
fn an_invalid_config_toml_falls_back_to_the_built_in_limit_and_notes_it_once_per_session() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    let file = config.path().join("config.toml");
    fs::write(&file, "context_windw = 1000000\n").unwrap();

    let under = stop_at(state.path(), config.path(), "s1", 160_000);
    assert_eq!(under.stdout, b"", "{under:?}");
    let notes_s1 = notes(state.path());
    assert_eq!(notes_s1.len(), 1, "{notes_s1:?}");
    assert!(
        notes_s1[0].contains(&file.display().to_string()) && notes_s1[0].contains("context_windw"),
        "{notes_s1:?}"
    );
    assert!(notes_s1[0].contains("160000"), "{notes_s1:?}");

    let over = stop_at(state.path(), config.path(), "s1", 160_001);
    let decision: serde_json::Value = serde_json::from_slice(&over.stdout).unwrap();
    assert!(
        decision["reason"]
            .as_str()
            .unwrap()
            .contains("160001 tokens, past your limit of 160000"),
        "{decision}"
    );
    assert_eq!(notes(state.path()).len(), 1);

    stop_at(state.path(), config.path(), "s2", 100);
    assert_eq!(notes(state.path()).len(), 2);
}

#[test]
fn an_unparseable_herd_file_falls_back_to_the_built_in_limit_and_notes_it() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    let ledger = state.path().join("loopherd/ledger.json");
    let text = fs::read_to_string(&ledger).unwrap().replace(
        r#""socket": "/run/herdr/herdr.sock""#,
        r#""socket": "/run/herdr/herdr.sock", "herd": "cards", "template": "t""#,
    );
    fs::write(&ledger, text).unwrap();
    let herd = config.path().join("herds/cards.toml");
    fs::create_dir_all(herd.parent().unwrap()).unwrap();
    fs::write(&herd, "repo = ").unwrap();

    let over = stop_at(state.path(), config.path(), "s1", 160_001);
    let decision: serde_json::Value = serde_json::from_slice(&over.stdout).unwrap();
    assert_eq!(decision["decision"], "block", "{over:?}");
    assert_eq!((over.status.code(), &over.stderr[..]), (Some(0), &b""[..]));
    let notes = notes(state.path());
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains(
            "cards.toml: TOML parse error at line 1, column 8 string values must be quoted"
        ),
        "{notes:?}"
    );
}

#[test]
fn an_unreadable_mailbox_warns_and_every_other_members_reports_still_print() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    run(state.path(), None, &["report", "--as", "lh-1", "PR #9"]);
    let unreadable = state.path().join("loopherd/reports/lh-2.jsonl");
    fs::create_dir(&unreadable).unwrap();
    let warning = format!(
        "warning: could not read {}: Is a directory (os error 21)\n",
        unreadable.display()
    );

    for args in [&["reports", "--all"][..], &["reports"]] {
        let out = run(state.path(), None, args);

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(stderr, format!("loopherd reports: {warning}"), "{args:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.starts_with("lh-1 note 20"), "{args:?}: {stdout}");
        assert!(stdout.ends_with(" PR #9\n"), "{args:?}: {stdout}");
        assert_eq!(out.status.code(), Some(0), "{args:?}");
    }
}

#[test]
fn an_unreadable_mailboxs_reports_are_read_once_it_is_readable_again() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root reads a mode 000 file");
        return;
    }
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    run(state.path(), None, &["report", "--as", "lh-1", "one"]);
    run(state.path(), None, &["report", "--as", "lh-2", "two"]);
    let mailbox = state.path().join("loopherd/reports/lh-2.jsonl");
    let mode = |m| fs::set_permissions(&mailbox, fs::Permissions::from_mode(m)).unwrap();
    mode(0o000);

    let out = run(state.path(), None, &["reports"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr,
        format!(
            "loopherd reports: warning: could not read {}: Permission denied (os error 13)\n",
            mailbox.display()
        )
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("lh-1 note "), "{stdout}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert_eq!(out.status.code(), Some(0));

    mode(0o644);
    let out = run(state.path(), None, &["reports"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("lh-2 note "), "{stdout}");
    assert!(stdout.ends_with(" two\n"), "{stdout}");
}

#[test]
fn watch_warns_about_an_unreadable_mailbox_and_wakes_on_another_members_report() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    install_hook(state.path());
    run(state.path(), None, &["report", "--as", "lh-1", "PR #9"]);
    let unreadable = state.path().join("loopherd/reports/lh-2.jsonl");
    fs::create_dir(&unreadable).unwrap();

    let out = run(state.path(), None, &["watch", "--timeout", "0"]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "warning: could not read {}: Is a directory (os error 21)\n",
            unreadable.display()
        )),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("lh-1 note 20"), "{stdout}");
    assert_eq!(out.status.code(), Some(0));
}

/// lh-1 retired from herd `old`, which has no herd file.
fn write_herd_ledger(state: &Path) {
    fs::create_dir_all(state.join("loopherd")).unwrap();
    fs::write(
        state.join("loopherd/ledger.json"),
        r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
            "retired": true, "herd": "old"}}"#,
    )
    .unwrap();
}

fn reports_of(state: &Path, config: &Path, herd: &str) -> Output {
    loopherd(state, None, &["reports", "--herd", herd, "--all"])
        .env("LOOPHERD_CONFIG_DIR", config)
        .output()
        .unwrap()
}

#[test]
fn reports_refuses_an_unknown_herd_by_name() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_herd_ledger(state.path());

    let out = reports_of(state.path(), config.path(), "nope");

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd reports: no herd nope\n"
    );
}

#[test]
fn reports_of_a_herd_with_a_file_or_only_retired_records_are_empty_not_refused() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_herd_ledger(state.path());
    fs::create_dir_all(config.path().join("herds")).unwrap();
    fs::write(config.path().join("herds/quiet.toml"), "").unwrap();

    for herd in ["quiet", "old"] {
        let out = reports_of(state.path(), config.path(), herd);
        assert_eq!(out.status.code(), Some(0), "{herd}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "{herd}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{herd}");
    }
}

fn write_notes(state: &Path) {
    let mailbox = state.join("loopherd/reports");
    fs::create_dir_all(&mailbox).unwrap();
    fs::write(
        mailbox.join("lh-1.jsonl"),
        concat!(
            r#"{"at":"2026-11-10T10:00:00Z","kind":"note","text":"one"}"#,
            "\n",
            r#"{"at":"2026-11-10T11:00:00Z","kind":"note","text":"two"}"#,
            "\n",
            r#"{"at":"2026-11-10T12:00:00Z","kind":"note","text":"three"}"#,
            "\n",
        ),
    )
    .unwrap();
}

#[test]
fn reports_all_takes_since_and_tail() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    write_notes(state.path());

    let out = run(
        state.path(),
        None,
        &[
            "reports",
            "--all",
            "--since",
            "2026-11-10T11:00:00Z",
            "--tail",
            "1",
        ],
    );

    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 note 2026-11-10 12:00 UTC three\n"
    );
}

#[test]
fn a_bad_since_or_a_zero_tail_is_refused() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    write_notes(state.path());

    for (args, error) in [
        (["--since", "soon"], "or span back from now (30m, 2h, 1d)"),
        (["--tail", "0"], "must be at least 1"),
    ] {
        let out = run(
            state.path(),
            None,
            &[&["reports", "--all"][..], &args].concat(),
        );
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert_eq!(out.stdout, b"", "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(error), "{args:?}: {stderr}");
    }
}

#[test]
fn since_and_tail_without_all_are_refused_and_mark_nothing_read() {
    let state = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    write_notes(state.path());

    for args in [["--since", "1d"], ["--tail", "40"]] {
        let out = run(state.path(), None, &[&["reports"][..], &args].concat());
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert_eq!(out.stdout, b"", "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("--all"), "{args:?}: {stderr}");
    }

    let unread = run(state.path(), None, &["reports"]);
    assert_eq!(String::from_utf8_lossy(&unread.stdout).lines().count(), 3);
}

#[test]
fn reports_help_documents_since_and_tail() {
    let state = tempfile::tempdir().unwrap();
    let out = run(state.path(), None, &["reports", "--help"]);
    let help = String::from_utf8_lossy(&out.stdout);

    for flag in [
        "--since <SINCE>",
        "30m, 2h, 1d",
        "--tail <N>",
        "Needs --all",
    ] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
}
