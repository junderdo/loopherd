//! `loopherd restart --self` through the binary.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/herdr/herdr.sock";

fn write_member(state: &Path) {
    fs::create_dir_all(state.join("loopherd/handoffs")).unwrap();
    fs::write(state.join("loopherd/handoffs/lh-1.md"), "## State\n").unwrap();
    fs::write(
        state.join("loopherd/ledger.json"),
        format!(
            r#"{{"lh-1": {{"name": "lh-1", "pane_id": "w1:p1", "session_id": null,
                "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
                "prompt_sent": true, "created": "2026-11-01T00:00:00Z", "socket": "{SOCKET}",
                "role": "lead", "herd": "h"}}}}"#
        ),
    )
    .unwrap();
}

/// Run from lh-1's pane, with no herdr on PATH.
fn run(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        .env("PATH", "")
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_SOCKET_PATH", SOCKET)
        .output()
        .unwrap()
}

fn waiter_holds_its_claim(state: &Path) -> bool {
    let file = File::open(state.join("loopherd/restarts/lh-1.waiter.lock")).unwrap();
    // SAFETY: flock on a descriptor `file` keeps open for the call.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    !taken
}

#[test]
fn self_restart_returns_at_once_and_leaves_a_detached_waiter_holding_the_claim() {
    let state = tempfile::tempdir().unwrap();
    write_member(state.path());
    let out = run(state.path(), &["report", "--kind", "handoff", "at step 3"]);
    assert!(out.status.success(), "{out:?}");

    // `output` waits for stdout to close, so a waiter holding it would hang here.
    let out = run(state.path(), &["restart", "--self"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("lh-1 restarts in this pane once"),
        "{out:?}"
    );
    assert!(waiter_holds_its_claim(state.path()));

    let again = run(state.path(), &["restart", "--self"]);
    assert_eq!(
        String::from_utf8_lossy(&again.stderr).trim(),
        "loopherd restart: lh-1 already has a restart waiting for its stop"
    );

    // As if another restart consumed the handoff: the waiter exits and lets go.
    fs::remove_file(state.path().join("loopherd/handoffs/lh-1.md")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while waiter_holds_its_claim(state.path()) {
        assert!(Instant::now() < deadline, "the waiter never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn restart_wait_refuses_without_the_claim_self_restart_hands_it() {
    let state = tempfile::tempdir().unwrap();
    write_member(state.path());
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["restart-wait", "lh-1"])
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_RESTART_WAITER_FD", "0")
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("doesn't hold lh-1's self-restart claim"),
        "{out:?}"
    );
}
