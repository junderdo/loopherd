//! A lead's report, its pane label through its broker, and `[notify] pane_label` through the
//! binary: a lead's question labels its pane, and the prompt and session hooks
//! clear it, with a `herdr` on PATH that records each call and, like herdr's
//! `pane report-metadata`, prints nothing.

mod brokers;
mod common;
mod fakes;

use brokers::Broker;
use common::feed_stdin;
use fakes::executable;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const SOCKET: &str = "/run/herdr/herdr.sock";
const LABEL: &str = "pane report-metadata w1:p0 --source loopherd --agent claude \
                     --display-agent lead-cards asks: which DB?";
const CLEAR: &str = "pane report-metadata w1:p0 --source loopherd --clear-display-agent";

struct World {
    broker: Option<Broker>,
    root: tempfile::TempDir,
}

impl World {
    /// lead-cards leads cards from pane w1:p0, with its broker running; lh-1 works in w1:p1.
    fn new(notify: &str) -> World {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state/loopherd");
        fs::create_dir_all(&state).unwrap();
        let entry = |name: &str, pane: &str, role: &str| {
            format!(
                r#""{name}": {{"name": "{name}", "pane_id": "{pane}", "session_id": null,
                    "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
                    "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
                    "socket": "{SOCKET}", "herd": "cards", "role": {role}}}"#
            )
        };
        fs::write(
            state.join("ledger.json"),
            format!(
                "{{{}, {}}}",
                entry("lead-cards", "w1:p0", r#""lead""#),
                entry("lh-1", "w1:p1", "null")
            ),
        )
        .unwrap();
        fs::create_dir_all(root.path().join("config/herds")).unwrap();
        fs::write(root.path().join("config/config.toml"), notify).unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        executable(
            &bin.join("herdr"),
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\n",
                root.path().join("calls").display()
            ),
        );
        let mut world = World { broker: None, root };
        let config = world.root.path().join("config");
        world.broker = Some(Broker::start(
            world.loopherd("", &[]),
            &config,
            "lead-cards",
            "cards",
        ));
        world
    }

    fn mailbox(&self, name: &str) -> String {
        let path = self
            .root
            .path()
            .join(format!("state/loopherd/reports/{name}/mailbox.jsonl"));
        fs::read_to_string(path).unwrap_or_default()
    }

    fn loopherd(&self, pane: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        cmd.args(args)
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", self.root.path().join("config"))
            .env("CLAUDE_CONFIG_DIR", self.root.path().join("claude"))
            .env("PATH", self.root.path().join("bin"))
            .env("HERDR_PANE_ID", pane)
            .env("HERDR_SOCKET_PATH", SOCKET)
            .env_remove("CLAUDE_PID");
        cmd
    }

    fn run(&self, pane: &str, args: &[&str]) -> Output {
        let out = self.loopherd(pane, args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "");
        out
    }

    /// Runs a hook, feeding it `input` as Claude does.
    fn hook(&self, pane: &str, hook: &str, input: &str) -> Output {
        let mut child = self
            .loopherd(pane, &[hook])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        feed_stdin(child.stdin.take().unwrap(), input.as_bytes());
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "");
        out
    }

    fn calls(&self) -> Vec<String> {
        let path: PathBuf = self.root.path().join("calls");
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

const ON: &str = "[notify]\nenabled = true\nherdr = false\npane_label = true\n";
// UserPromptSubmit inputs captured from Claude Code 2.1.327, paths made generic.
const PROMPT: &str = include_str!("fixtures/prompt-hook/typed.json");
const TASK_DONE: &str = include_str!("fixtures/prompt-hook/task-notification.json");
const SESSION: &str = r#"{"hook_event_name":"SessionStart","source":"startup","session_id":"s1"}"#;

#[test]
fn a_leads_question_labels_its_pane_until_the_human_prompts_there() {
    let world = World::new(ON);

    world.run("w1:p0", &["report", "--kind", "question", "which DB?"]);
    assert_eq!(world.calls(), [LABEL]);
    world.hook("w1:p0", "prompt-hook", TASK_DONE);
    assert_eq!(world.calls(), [LABEL]);

    let out = world.hook("w1:p0", "prompt-hook", PROMPT);
    assert_eq!(
        out.stdout, b"",
        "a UserPromptSubmit hook's stdout reaches Claude"
    );
    world.hook("w1:p0", "prompt-hook", PROMPT);
    assert_eq!(world.calls(), [LABEL, CLEAR]);
}

#[test]
fn a_session_start_in_a_labelled_pane_clears_its_label() {
    let world = World::new(ON);
    world.run("w1:p0", &["report", "--kind", "question", "which DB?"]);

    world.hook("w1:p0", "session-hook", SESSION);

    assert_eq!(world.calls(), [LABEL, CLEAR]);
}

#[test]
fn nothing_is_labelled_off_by_default_or_for_a_worker_or_a_lead_s_other_reports() {
    let world = World::new("[notify]\nenabled = true\nherdr = false\n");
    world.run("w1:p0", &["report", "--kind", "question", "which DB?"]);

    let on = World::new(ON);
    on.run("w1:p1", &["report", "--kind", "question", "which DB?"]);
    on.run("w1:p0", &["report", "--kind", "done", "PR #9"]);
    on.hook("w1:p0", "prompt-hook", PROMPT);
    on.hook("w1:p0", "session-hook", SESSION);

    assert_eq!(world.calls(), Vec::<String>::new());
    assert_eq!(on.calls(), Vec::<String>::new());
}

#[test]
fn a_label_herdr_refuses_leaves_the_report_written_with_a_warning() {
    let world = World::new(ON);
    executable(
        &world.root.path().join("bin/herdr"),
        "#!/bin/sh\necho '{\"error\":{\"code\":\"pane_not_found\",\"message\":\"pane w1:p0 not found\"}}'\nexit 1\n",
    );

    let out = world
        .loopherd("w1:p0", &["report", "--kind", "question", "which DB?"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd report: warning: label pane w1:p0: \
         herdr pane report-metadata: pane w1:p0 not found (pane_not_found)\n"
    );
    assert!(world.mailbox("lead-cards").contains("which DB?"));
}

#[test]
fn a_lead_reports_only_as_itself() {
    let world = World::new(ON);

    world.run("w1:p0", &["report", "--as", "lead-cards", "PR #9"]);
    let out = world
        .loopherd("w1:p0", &["report", "--as", "lh-1", "PR #10"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd report: lead-cards reports only as itself; lh-1's reports are lh-1's own\n"
    );
    assert!(world.mailbox("lead-cards").contains("PR #9"));
    assert_eq!(world.mailbox("lh-1"), "");
}

#[test]
fn a_lead_whose_broker_is_down_writes_no_report_and_is_told_who_revives_it() {
    let mut world = World::new(ON);
    world.broker = None;

    let out = world
        .loopherd("w1:p0", &["report", "--kind", "question", "which DB?"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "no broker is running for lead-cards, so it can't report: \
             the shepherd's loopherd revive --name lead-cards starts it"
        ),
        "{stderr}"
    );
    assert_eq!(world.mailbox("lead-cards"), "");
    assert_eq!(world.calls(), Vec::<String>::new());
}

#[test]
fn a_lead_whose_broker_predates_reports_writes_its_own_and_is_told_its_pane_gets_no_label() {
    use std::os::fd::AsRawFd;
    let mut world = World::new(ON);
    world.broker = None;
    let config = world.root.path().join("config/broker");
    let lock = fs::File::create(config.join("lead-cards.lock")).unwrap();
    // SAFETY: flock on a descriptor `lock` keeps open while the old broker "runs".
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let requests = world
        .root
        .path()
        .join("state/loopherd/broker/lead-cards/requests");
    let replies = config.join("lead-cards/replies");
    let old = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let request = fs::read_dir(&requests).ok().and_then(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .find(|n| n.ends_with(".json"))
            });
            if let Some(request) = request {
                fs::create_dir_all(&replies).unwrap();
                let part = replies.join(".part");
                fs::write(
                    &part,
                    r#"{"refused": "malformed request: unknown variant `report`"}"#,
                )
                .unwrap();
                fs::rename(part, replies.join(request)).unwrap();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });

    let out = world
        .loopherd("w1:p0", &["report", "--kind", "question", "which DB?"])
        .output()
        .unwrap();

    old.join().unwrap();
    drop(lock);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd report: warning: lead-cards's broker predates this loopherd, so it can't \
         report; the shepherd must restart it with loopherd broker-restart lead-cards; writing \
         the report directly, and its pane gets no label\n"
    );
    let mailbox = world.mailbox("lead-cards");
    assert_eq!(mailbox.lines().count(), 1, "{mailbox}");
    assert!(mailbox.contains("which DB?"), "{mailbox}");
    assert_eq!(world.calls(), Vec::<String>::new());
}
