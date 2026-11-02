//! `loopherd watch` through the binary, with herdr off PATH.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// lh-1 in the ledger, with a paused stop in its mailbox.
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state/loopherd");
        fs::create_dir_all(state.join("reports")).unwrap();
        fs::write(
            state.join("ledger.json"),
            r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": null,
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

    let deadline = Instant::now() + Duration::from_secs(10);
    let refused = loop {
        let out = sandbox.watch(&["--timeout", "0"]);
        if out.status.code() == Some(1) || Instant::now() > deadline {
            break out;
        }
        // Started before the first took the lock; try again.
        std::thread::sleep(Duration::from_millis(50));
    };
    first.kill().unwrap();
    first.wait().unwrap();

    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    let lock = reports.join("watch.lock");
    assert!(
        stderr(&refused).contains(&lock.display().to_string()),
        "{}",
        stderr(&refused)
    );
}
