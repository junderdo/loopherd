//! `loopherd broker-restart`: the shepherd's way to put a lead's broker on
//! the current binary, refused from the lead's own pane.

mod brokers;

use brokers::{Broker, held};
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/herdr-test.sock";

/// lead-k leads herd k from pane w1:p1. Dropping it retires lead-k, which
/// stops any broker a restart left running, and waits until it has.
struct World {
    root: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state/loopherd");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("ledger.json"), ledger(false)).unwrap();
        World { root }
    }

    fn broker(&self) -> Broker {
        Broker::start(self.loopherd(None, &[]), &self.config(), "lead-k", "k")
    }

    fn run(&self, pane: Option<&str>, args: &[&str]) -> Output {
        self.loopherd(pane, args).output().unwrap()
    }

    fn loopherd(&self, pane: Option<&str>, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        cmd.env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", self.config())
            .env("CLAUDE_CONFIG_DIR", self.root.path().join("claude"))
            .env("PATH", "/usr/bin:/bin")
            .env("HERDR_SOCKET_PATH", SOCKET)
            .env_remove("CLAUDE_PID");
        match pane {
            Some(pane) => cmd.env("HERDR_PANE_ID", pane),
            None => cmd.env_remove("HERDR_PANE_ID"),
        };
        cmd.args(args);
        cmd
    }

    fn config(&self) -> std::path::PathBuf {
        self.root.path().join("config")
    }

    fn serving(&self) -> bool {
        File::open(self.config().join("broker/lead-k.lock")).is_ok_and(|file| held(&file))
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let ledger = self.root.path().join("state/loopherd/ledger.json");
        let _ = fs::write(ledger, self::ledger(true));
        let given_up = Instant::now() + Duration::from_secs(5);
        while self.serving() && Instant::now() < given_up {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn ledger(retired: bool) -> String {
    format!(
        r#"{{"lead-k": {{"name": "lead-k", "pane_id": "w1:p1", "session_id": null,
        "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
        "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
        "socket": "{SOCKET}", "herd": "k", "role": "lead", "retired": {retired}}}}}"#
    )
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into()
}

/// The pid after `pid ` in `text`, ahead of `before`.
fn pid_in(text: &str, before: &str) -> u32 {
    let head = &text[..text.find(before).unwrap()];
    head.rsplit("pid ").next().unwrap().trim().parse().unwrap()
}

fn exited(pid: u32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn broker_restart_replaces_a_running_broker_with_one_from_this_binary() {
    let world = World::new();
    let mut old = world.broker();
    let old_pid = old.pid();

    let restarted = stdout(&world.run(None, &["broker-restart", "lead-k"]));

    let new_pid = pid_in(&restarted, " serves");
    assert_eq!(
        restarted,
        format!(
            "asked lead-k's broker (pid {old_pid}) to stop once its request in flight is answered\n\
             restarted lead-k's broker: pid {old_pid} stopped, pid {new_pid} serves\n"
        )
    );
    assert!(old.wait_exited(Duration::from_secs(5)));
    assert_ne!(new_pid, old_pid);
    assert!(!exited(new_pid));
    assert!(world.serving());
}

#[test]
fn broker_restart_with_no_broker_running_starts_one() {
    let world = World::new();
    drop(world.broker());

    let started = stdout(&world.run(None, &["broker-restart", "lead-k"]));

    let pid = pid_in(&started, "\n");
    assert_eq!(started, format!("started lead-k's broker: pid {pid}\n"));
    assert!(world.serving());
}

#[test]
fn broker_restart_from_a_leads_pane_is_refused_and_leaves_its_broker_alone() {
    let world = World::new();
    let mut broker = world.broker();

    let refused = world.run(Some("w1:p1"), &["broker-restart", "lead-k"]);

    assert!(!refused.status.success());
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "loopherd broker-restart: refused: broker-restart is the shepherd's or the human's, \
         and a lead runs this loopherd\n"
    );
    assert!(!broker.wait_exited(Duration::from_millis(300)));
    assert!(world.serving());
}

#[test]
fn broker_restart_run_by_a_brokers_own_request_is_refused() {
    let world = World::new();
    let mut broker = world.broker();

    let refused = world
        .loopherd(None, &["broker-restart", "lead-k"])
        .env("LOOPHERD_BROKER_OF", "lead-k")
        .output()
        .unwrap();

    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("refused: broker-restart is the shepherd's"),
        "{refused:?}"
    );
    assert!(!broker.wait_exited(Duration::from_millis(300)));
}
