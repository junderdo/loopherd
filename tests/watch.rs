//! `loopherd watch` through the binary, with herdr off PATH.

mod fakes;

use fakes::executable;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// lh-1 in pane w1:p1 of tab w1:t1, with a paused stop in its mailbox.
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state/loopherd");
        fs::create_dir_all(state.join("reports")).unwrap();
        fs::write(
            state.join("ledger.json"),
            r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "tab_id": "w1:t1", "session_id": null,
                "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
                "prompt_sent": true, "created": "2026-11-01T00:00:00Z"}}"#,
        )
        .unwrap();
        fs::write(
            state.join("reports/lh-1.jsonl"),
            r#"{"at":"2026-11-02T09:00:00Z","kind":"stop","background":{"shells":1,"subagents":0,"other":0},"crons":0}
"#,
        )
        .unwrap();
        Sandbox { dir }
    }

    fn settings(&self) -> PathBuf {
        self.dir.path().join("settings.json")
    }

    fn install_hook(&self) {
        let out = self
            .loopherd(&[
                "install-hook",
                "--settings",
                self.settings().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    fn loopherd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        cmd.args(args)
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("CLAUDE_CONFIG_DIR", self.dir.path())
            .env("LOOPHERD_CONFIG_DIR", self.dir.path().join("config"))
            .env_remove("CLAUDE_PID")
            .env("PATH", "")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH");
        cmd
    }

    fn watch(&self, args: &[&str]) -> Output {
        self.loopherd(&[&["watch"], args].concat())
            .output()
            .unwrap()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn without_the_hook_it_refuses_and_points_at_scripts_install() {
    let sandbox = Sandbox::new();

    let out = sandbox.watch(&["--timeout", "0"]);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).contains("scripts/install"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&sandbox.settings().display().to_string()),
        "{}",
        stderr(&out)
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn with_the_hook_a_paused_stop_wakes_it() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();

    let out = sandbox.watch(&["--timeout", "0"]);

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\n"
    );
}

#[test]
fn a_pane_closed_while_no_watch_ran_wakes_the_next_watch_gone_once() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let closed = sandbox
        .loopherd(&["herdr-event-hook"])
        .env("HERDR_PLUGIN_EVENT", "pane.closed")
        .env(
            "HERDR_PLUGIN_EVENT_JSON",
            r#"{"event":"pane_closed","data":{"pane_id":"w1:p1","workspace_id":"w1"}}"#,
        )
        .output()
        .unwrap();
    assert_eq!(closed.status.code(), Some(0), "{closed:?}");

    let out = sandbox.watch(&["--timeout", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\nlh-1 gone\n"
    );

    let again = sandbox.watch(&["--timeout", "0"]);
    assert_eq!(again.status.code(), Some(2), "{}", stderr(&again));
    assert!(again.stdout.is_empty(), "{again:?}");
}

#[test]
fn a_tab_closed_while_no_watch_ran_wakes_the_next_watch_gone_once() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let closed = sandbox
        .loopherd(&["herdr-event-hook"])
        .env("HERDR_PLUGIN_EVENT", "tab.closed")
        .env(
            "HERDR_PLUGIN_EVENT_JSON",
            r#"{"event":"tab_closed","data":{"tab_id":"w1:t1","workspace_id":"w1"}}"#,
        )
        .output()
        .unwrap();
    assert_eq!(closed.status.code(), Some(0), "{closed:?}");

    let out = sandbox.watch(&["--timeout", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\nlh-1 gone\n"
    );

    let again = sandbox.watch(&["--timeout", "0"]);
    assert_eq!(again.status.code(), Some(2), "{}", stderr(&again));
    assert!(again.stdout.is_empty(), "{again:?}");
}

impl Sandbox {
    fn herdr_event(&self, event: &str, data: &str) {
        let kind = event.replace('.', "_");
        let out = self
            .loopherd(&["herdr-event-hook"])
            .env("HERDR_PLUGIN_EVENT", event)
            .env(
                "HERDR_PLUGIN_EVENT_JSON",
                format!(r#"{{"event":"{kind}","data":{data}}}"#),
            )
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }

    fn tab_closed(&self, tab: &str) {
        self.herdr_event(
            "tab.closed",
            &format!(r#"{{"tab_id":"{tab}","workspace_id":"w1"}}"#),
        );
    }

    /// lh-1's pane moved from tab w1:t1, which the move emptied, to w1:t4.
    fn moved_out_of_its_tab(&self) {
        self.herdr_event(
            "pane.moved",
            r#"{"pane":{"pane_id":"w1:p1","tab_id":"w1:t4","workspace_id":"w1"},
                "previous_pane_id":"w1:p1","previous_tab_id":"w1:t1",
                "previous_workspace_id":"w1","closed_tab_id":"w1:t1","type":"pane_moved"}"#,
        );
    }
}

#[test]
fn a_pane_moved_out_of_its_tab_wakes_no_gone_whichever_hook_runs_first() {
    for close_first in [false, true] {
        let sandbox = Sandbox::new();
        sandbox.install_hook();
        if close_first {
            sandbox.tab_closed("w1:t1");
            sandbox.moved_out_of_its_tab();
        } else {
            sandbox.moved_out_of_its_tab();
            sandbox.tab_closed("w1:t1");
        }

        let out = sandbox.watch(&["--timeout", "0"]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "lh-1 paused  1 shell\n",
            "close first: {close_first}"
        );

        sandbox.tab_closed("w1:t4");
        let out = sandbox.watch(&["--timeout", "0"]);
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "lh-1 gone\n",
            "close first: {close_first}"
        );
    }
}

#[test]
fn settings_names_the_file_to_check() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let elsewhere = sandbox.dir.path().join("elsewhere.json");

    let out = sandbox.watch(&["--timeout", "0", "--settings", elsewhere.to_str().unwrap()]);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).contains("elsewhere.json"), "{}", stderr(&out));
}

#[test]
fn a_second_concurrent_watch_exits_naming_the_lock() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    // Empty the mailbox so the first watch stays armed.
    let reports = sandbox.dir.path().join("state/loopherd/reports");
    fs::write(reports.join("lh-1.jsonl"), "").unwrap();
    let mut first = sandbox
        .loopherd(&["watch", "--timeout", "30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // The lock holds the holder's PID once taken.
    let lock = reports.join("watch.lock");
    let pid = first.id().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_to_string(&lock).unwrap_or_default().trim() != pid {
        assert!(
            Instant::now() < deadline,
            "the first watch never took the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let refused = sandbox.watch(&["--timeout", "0"]);
    first.kill().unwrap();
    first.wait().unwrap();

    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(
        stderr(&refused).contains(&lock.display().to_string()),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains(&format!("(pid {pid})")),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn a_watch_that_fails_after_reading_stops_leaves_them_for_the_next() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let reports = sandbox.dir.path().join("state/loopherd/reports");
    fs::write(reports.join(".cursor"), "corrupt").unwrap();

    let out = sandbox.watch(&["--timeout", "0"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty());

    fs::remove_file(reports.join(".cursor")).unwrap();
    let out = sandbox.watch(&["--timeout", "0"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\n"
    );
}

#[test]
fn a_leads_watch_runs_beside_the_shepherds() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let state = sandbox.dir.path().join("state/loopherd");
    fs::write(
        state.join("ledger.json"),
        r#"{"lead-cards": {"name": "lead-cards", "pane_id": "w1:p0", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
            "herd": "cards", "role": "lead", "socket": "/run/live.sock"}}"#,
    )
    .unwrap();
    let mut shepherd = sandbox
        .loopherd(&["watch", "--timeout", "30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let lock = state.join("reports/watch.lock");
    let pid = shepherd.id().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_to_string(&lock).unwrap_or_default().trim() != pid {
        assert!(
            Instant::now() < deadline,
            "the shepherd's watch never took the lock"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let lead = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("HERDR_PANE_ID", "w1:p0")
        .env("HERDR_SOCKET_PATH", "/run/live.sock")
        .output()
        .unwrap();
    shepherd.kill().unwrap();
    shepherd.wait().unwrap();

    assert_eq!(lead.status.code(), Some(2), "{lead:?}");
    assert!(state.join("reports/watch-lead-cards.lock").is_file());
}

/// This test process stands in for the shepherd's Claude: the watch's parent,
/// with a session file and a transcript at `tokens` in context.
fn as_shepherds_claude(sandbox: &Sandbox, session: &str, tokens: u64) -> String {
    let me = std::process::id();
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let start = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    let sessions = sandbox.dir.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("{me}.json")),
        format!(r#"{{"pid":{me},"sessionId":"{session}","procStart":"{start}"}}"#),
    )
    .unwrap();
    let project = sandbox.dir.path().join("projects/-repo");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(format!("{session}.jsonl")),
        format!(r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{tokens}}}}}}}"#)
            + "\n",
    )
    .unwrap();
    me.to_string()
}

fn with_shepherd_limit(sandbox: &Sandbox, limit: u64) {
    let config = sandbox.dir.path().join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        format!("[shepherd]\ncontext_limit = {limit}\n"),
    )
    .unwrap();
}

#[test]
fn the_shepherds_watch_tells_it_to_hand_off_once_per_session_over_its_limit() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    with_shepherd_limit(&sandbox, 50_000);
    let pid = as_shepherds_claude(&sandbox, "s1", 64_200);

    let out = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("CLAUDE_PID", &pid)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\nshepherd handoff 64k over 50k\n"
    );

    let again = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("CLAUDE_PID", &pid)
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(2), "{again:?}");
    assert!(again.stdout.is_empty());
}

#[test]
fn a_timed_out_watch_still_tells_the_shepherd() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let reports = sandbox.dir.path().join("state/loopherd/reports");
    fs::write(reports.join("lh-1.jsonl"), "").unwrap();
    let pid = as_shepherds_claude(&sandbox, "s1", 170_000);

    let out = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("CLAUDE_PID", &pid)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "shepherd handoff 170k over 160k\n"
    );
}

#[test]
fn under_its_limit_or_from_a_members_pane_the_watch_says_nothing_more() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let pid = as_shepherds_claude(&sandbox, "s1", 160_000);
    let out = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("CLAUDE_PID", &pid)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\n"
    );

    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let pid = as_shepherds_claude(&sandbox, "s1", 170_000);
    let out = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("CLAUDE_PID", &pid)
        .env("HERDR_PANE_ID", "w1:p1")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\n"
    );
}

#[test]
fn a_bad_shepherd_limit_warns_and_says_nothing_more() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    with_shepherd_limit(&sandbox, 5);
    let pid = as_shepherds_claude(&sandbox, "s1", 170_000);

    let out = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("CLAUDE_PID", &pid)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\n"
    );
    assert!(
        stderr(&out).contains("shepherd.context_limit must be at least 10000"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_leads_watch_over_the_shepherds_limit_says_nothing_more() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let state = sandbox.dir.path().join("state/loopherd");
    fs::write(
        state.join("ledger.json"),
        r#"{"lead-cards": {"name": "lead-cards", "pane_id": "w1:p0", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
            "herd": "cards", "role": "lead", "socket": "/run/live.sock"},
           "lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
            "herd": "cards", "socket": "/run/live.sock"}}"#,
    )
    .unwrap();
    let pid = as_shepherds_claude(&sandbox, "s-lead", 170_000);

    let out = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("CLAUDE_PID", &pid)
        .env("HERDR_PANE_ID", "w1:p0")
        .env("HERDR_SOCKET_PATH", "/run/live.sock")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lh-1 paused  1 shell\n"
    );
    assert!(!state.join("handoff-asked/s-lead").exists());
}

impl Sandbox {
    /// `[notify]` enabled with `table`'s extra lines, a `herdr` on PATH that
    /// records a notification's socket and arguments in `herdr.out` (and
    /// fails anything else, as an unreachable herdr does), and `question`
    /// added to lh-1's mailbox.
    fn notifying(&self, table: &str, at: &str, question: &str) {
        self.notify_config(table);
        self.ask(at, question);
    }

    fn notify_config(&self, table: &str) {
        let config = self.dir.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            format!("[notify]\nenabled = true\n{table}"),
        )
        .unwrap();
        let out = self.path("herdr.out");
        self.script(
            "bin/herdr",
            &format!(
                "[ \"$1\" = notification ] || exit 1\n\
                 printf '%s\\n' \"$HERDR_SOCKET_PATH\" \"$@\" > {}.tmp && /bin/mv {0}.tmp {0}",
                out.display()
            ),
        );
    }

    fn ask(&self, at: &str, question: &str) {
        let entry = serde_json::json!({"at": at, "kind": "question", "text": question});
        let mut mailbox = fs::OpenOptions::new()
            .append(true)
            .open(self.mailbox())
            .unwrap();
        std::io::Write::write_all(&mut mailbox, format!("{entry}\n").as_bytes()).unwrap();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn mailbox(&self) -> PathBuf {
        self.path("state/loopherd/reports/lh-1.jsonl")
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        executable(&path, format!("#!/bin/sh\n{body}\n"));
        path
    }

    fn notify_watch(&self) -> Output {
        self.loopherd(&["watch", "--timeout", "0"])
            .env("PATH", self.path("bin"))
            .env("HERDR_SOCKET_PATH", "/run/watched.sock")
            .output()
            .unwrap()
    }
}

/// A copy of the loopherd under test at `to`.
fn loopherd_copy(to: &std::path::Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    executable(to, fs::read(env!("CARGO_BIN_EXE_loopherd")).unwrap());
}

/// `path`'s text once something writes it, failing after 10s.
fn eventually(path: &std::path::Path, ready: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if ready(&text) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} stayed {text:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_question_is_sent_to_herdrs_toast_on_the_watched_socket_and_to_the_command() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let out = sandbox.path("command.out");
    let command = sandbox.script(
        "notify-stub",
        &format!(
            "printf '%s|' \"$@\" > {}.tmp && /bin/mv {0}.tmp {0}",
            out.display()
        ),
    );
    sandbox.notifying(
        &format!(
            "command = '{} {{herd}} {{member}} {{kind}} {{text}}'\n",
            command.display()
        ),
        "2026-11-02T09:01:00Z",
        "Merge PR #107?\nIt adds notify.",
    );

    let watched = sandbox.notify_watch();

    assert_eq!(watched.status.code(), Some(0), "{}", stderr(&watched));
    assert_eq!(
        eventually(&sandbox.path("herdr.out"), |t| !t.is_empty()),
        "/run/watched.sock\nnotification\nshow\nlh-1\n--body=Merge PR #107?\n--sound\nrequest\n"
    );
    assert_eq!(
        eventually(&out, |t| !t.is_empty()),
        "|lh-1|question|Merge PR #107?|"
    );
}

#[test]
fn a_slow_command_does_not_hold_up_the_watch_and_a_failing_one_is_noted() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let slow = sandbox.script("slow", "/bin/sleep 5");
    sandbox.notifying(
        &format!("herdr = false\ncommand = '{}'\n", slow.display()),
        "2026-11-02T09:01:00Z",
        "Which way?",
    );
    let started = Instant::now();

    let watched = sandbox.notify_watch();

    assert_eq!(watched.status.code(), Some(0), "{}", stderr(&watched));
    assert!(started.elapsed() < Duration::from_secs(4));

    let failing = sandbox.script("failing", "echo refused >&2; exit 7");
    sandbox.notifying(
        &format!("herdr = false\ncommand = '{}'\n", failing.display()),
        "2026-11-02T09:02:00Z",
        "Which way now?",
    );
    let watched = sandbox.notify_watch();

    assert_eq!(watched.status.code(), Some(0), "{}", stderr(&watched));
    let mailbox = eventually(&sandbox.mailbox(), |t| t.contains("\"note\""));
    assert!(
        mailbox.contains("notify command failed (exit 7: refused)"),
        "{mailbox}"
    );
}

#[test]
fn a_broken_notify_table_sends_nothing_and_warns() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    sandbox.notifying("kinds = ['done']\n", "2026-11-02T09:01:00Z", "Which way?");

    let watched = sandbox.notify_watch();

    assert_eq!(watched.status.code(), Some(0), "{}", stderr(&watched));
    assert!(
        stderr(&watched).contains("sent no notification"),
        "{}",
        stderr(&watched)
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(!sandbox.path("herdr.out").exists());
}

impl Sandbox {
    /// A watch run from a copy of loopherd that is replaced, as a reinstall
    /// does, before `question` arrives.
    fn notify_watch_from_a_replaced_binary(&self, question: &str) -> Output {
        self.install_hook();
        self.notify_config("");
        fs::write(self.mailbox(), "").unwrap();
        let old = self.path("old/loopherd");
        loopherd_copy(&old);
        let watch = self.loopherd(&["watch", "--timeout", "10"]);
        let watch = Command::new(&old)
            .args(watch.get_args())
            .envs(watch.get_envs().filter_map(|(k, v)| Some((k, v?))))
            .env("PATH", self.path("bin"))
            .env("HERDR_SOCKET_PATH", "/run/watched.sock")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        fs::write(self.path("new"), b"").unwrap();
        fs::rename(self.path("new"), &old).unwrap();
        self.ask("2026-11-02T09:01:00Z", question);
        watch.wait_with_output().unwrap()
    }
}

#[test]
fn a_watch_whose_binary_was_replaced_sends_through_loopherd_on_path() {
    let sandbox = Sandbox::new();
    loopherd_copy(&sandbox.path("bin/loopherd"));

    let watched = sandbox.notify_watch_from_a_replaced_binary("Which way?");

    assert_eq!(watched.status.code(), Some(0), "{}", stderr(&watched));
    assert!(
        eventually(&sandbox.path("herdr.out"), |t| !t.is_empty()).contains("--body=Which way?")
    );
}

#[test]
fn a_notification_that_cannot_start_is_noted_in_the_mailbox() {
    let sandbox = Sandbox::new();

    let watched = sandbox.notify_watch_from_a_replaced_binary("Which way?");

    assert_eq!(watched.status.code(), Some(0), "{}", stderr(&watched));
    let mailbox = fs::read_to_string(sandbox.mailbox()).unwrap();
    assert!(
        mailbox.contains("herdr's toast failed (could not start loopherd"),
        "{mailbox}"
    );
}

#[test]
fn notify_run_leads_its_own_process_group_so_the_watch_ending_leaves_it_running() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let out = sandbox.path("group.out");
    let command = sandbox.script(
        "group-stub",
        &format!(
            "read -r pid comm state ppid pgid rest < /proc/$PPID/stat\n\
             echo \"$PPID $pgid\" > {}.tmp && /bin/mv {0}.tmp {0}",
            out.display()
        ),
    );
    sandbox.notifying(
        &format!("herdr = false\ncommand = '{}'\n", command.display()),
        "2026-11-02T09:01:00Z",
        "Which way?",
    );

    let watched = sandbox.notify_watch();

    assert_eq!(watched.status.code(), Some(0), "{}", stderr(&watched));
    let ids = eventually(&out, |t| !t.is_empty());
    let (notify_run, group) = ids.trim().split_once(' ').unwrap();
    assert_eq!(notify_run, group, "{ids}");
}

#[test]
fn a_leads_watch_with_no_shepherds_watch_sends_the_leads_own_question_once() {
    let sandbox = Sandbox::new();
    sandbox.install_hook();
    let state = sandbox.path("state/loopherd");
    fs::write(
        state.join("ledger.json"),
        r#"{"lead-cards": {"name": "lead-cards", "pane_id": "w1:p0", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
            "herd": "cards", "role": "lead", "socket": "/run/watched.sock"}}"#,
    )
    .unwrap();
    let out = sandbox.path("command.out");
    let command = sandbox.script(
        "notify-stub",
        &format!("printf '%s|' \"$@\" >> {}; echo >> {0}", out.display()),
    );
    sandbox.notify_config(&format!(
        "herdr = false\ncommand = '{} {{herd}} {{member}} {{kind}} {{text}}'\n",
        command.display()
    ));
    fs::write(
        state.join("reports/lead-cards.jsonl"),
        r#"{"at":"2026-11-02T09:01:00Z","kind":"question","text":"Merge #107?"}
"#,
    )
    .unwrap();

    let watched = sandbox
        .loopherd(&["watch", "--timeout", "0"])
        .env("PATH", sandbox.path("bin"))
        .env("HERDR_PANE_ID", "w1:p0")
        .env("HERDR_SOCKET_PATH", "/run/watched.sock")
        .output()
        .unwrap();

    assert_eq!(watched.status.code(), Some(2), "{}", stderr(&watched));
    let sent = "cards|lead-cards|question|Merge #107?|\n";
    assert_eq!(eventually(&out, |t| !t.is_empty()), sent);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fs::read_to_string(&out).unwrap(), sent);
}
