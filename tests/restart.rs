//! `loopherd restart --self` through the binary.

mod brokers;

use brokers::{Broker, held};
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/herdr/herdr.sock";

fn write_member(state: &Path) {
    fs::create_dir_all(state.join("loopherd/handoffs/lh-1")).unwrap();
    fs::create_dir_all(state.join("loopherd/handoffs/shepherd")).unwrap();
    fs::write(
        state.join("loopherd/handoffs/lh-1/handoff.md"),
        "## State\n",
    )
    .unwrap();
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
    command(state, args).output().unwrap()
}

fn command(state: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    command
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        // install-hook into this claude dir puts the skill under HOME.
        .env("HOME", state.join("home"))
        .env("PATH", "")
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_SOCKET_PATH", SOCKET);
    command
}

/// Lead lh-1's broker; killed on drop.
fn start_broker(state: &Path) -> Broker {
    Broker::start(command(state, &[]), &state.join("config"), "lh-1", "h")
}

/// Whether process `pid` has `path` open.
fn has_open(pid: u32, path: &Path) -> bool {
    fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|fd| fs::read_link(fd.ok()?.path()).ok())
        .any(|target| target == path)
}

fn waiter_holds_its_claim(state: &Path) -> bool {
    held(&File::open(state.join("loopherd/restarts/lh-1.waiter.lock")).unwrap())
}

#[test]
fn self_restart_returns_at_once_and_leaves_a_detached_waiter_holding_the_claim() {
    let state = tempfile::tempdir().unwrap();
    write_member(state.path());
    let broker = start_broker(state.path());
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
    // The broker lives on, so only the waiter may keep the claim open.
    let claim = state.path().join("loopherd/restarts/lh-1.waiter.lock");
    assert!(!has_open(broker.pid(), &claim.canonicalize().unwrap()));

    let again = run(state.path(), &["restart", "--self"]);
    assert_eq!(
        String::from_utf8_lossy(&again.stderr).trim(),
        "loopherd restart: lh-1 already has a restart waiting for its stop"
    );

    // As if another restart consumed the handoff: the waiter exits and lets go.
    fs::remove_file(state.path().join("loopherd/handoffs/lh-1/handoff.md")).unwrap();
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

/// The shepherd's handoff record naming pane w1:p1 and this test process as
/// its Claude, a handoff written after it, the hooks in, and a config.toml
/// with no `self_restart` key, so the switch is on by default.
fn write_shepherd(state: &Path) {
    let me = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{me}/stat")).unwrap();
    let start = stat
        .rsplit(") ")
        .next()
        .unwrap()
        .split(' ')
        .nth(19)
        .unwrap();
    fs::create_dir_all(state.join("loopherd/handoffs/lh-1")).unwrap();
    fs::create_dir_all(state.join("loopherd/handoffs/shepherd")).unwrap();
    fs::write(
        state.join("loopherd/shepherd.json"),
        format!(
            r#"{{"pane": "w1:p1", "socket": "{SOCKET}", "process": {{"pid": {me}, "start": {start}}},
                "session": "s-1", "at": "2026-11-19T10:00:00Z", "used": false}}"#
        ),
    )
    .unwrap();
    fs::write(
        state.join("loopherd/handoffs/shepherd/handoff.md"),
        "## Next\n",
    )
    .unwrap();
    fs::create_dir_all(state.join("config")).unwrap();
    fs::write(state.join("config/config.toml"), "").unwrap();
    let settings = state.join("claude/settings.json");
    let out = run(
        state,
        &["install-hook", "--settings", settings.to_str().unwrap()],
    );
    assert!(out.status.success(), "{out:?}");
}

fn as_shepherd(state: &Path, args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("CLAUDE_CONFIG_DIR", state.join("claude"))
        // install-hook into this claude dir puts the skill under HOME.
        .env("HOME", state.join("home"))
        .env("CLAUDE_PID", std::process::id().to_string())
        .env("PATH", "")
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_SOCKET_PATH", SOCKET)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn shepherd_waiter_holds_its_claim(state: &Path) -> bool {
    held(&File::open(state.join("loopherd/restarts/shepherd.waiter.lock")).unwrap())
}

#[test]
fn the_shepherd_arms_its_own_restart_and_a_prompt_in_its_pane_cancels_it() {
    let state = tempfile::tempdir().unwrap();
    write_shepherd(state.path());

    let out = as_shepherd(state.path(), &["restart", "--self"], "");
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.starts_with("Armed: you restart yourself in pane w1:p1"),
        "{said}"
    );
    assert!(shepherd_waiter_holds_its_claim(state.path()));

    let prompt = r#"{"hook_event_name":"UserPromptSubmit","prompt":"wait"}"#;
    let out = as_shepherd(state.path(), &["prompt-hook"], prompt);
    assert!(out.status.success(), "{out:?}");
    let told: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let context = told["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(context.contains("self-restart is cancelled"), "{context}");

    let deadline = Instant::now() + Duration::from_secs(10);
    while shepherd_waiter_holds_its_claim(state.path()) {
        assert!(Instant::now() < deadline, "the waiter never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
    let saved = fs::read_to_string(state.path().join("loopherd/shepherd.json")).unwrap();
    assert!(saved.contains(r#""state": "cancelled""#), "{saved}");
}

#[test]
fn a_shepherd_wait_without_the_claim_leaves_the_running_waiters_restart_pending() {
    let state = tempfile::tempdir().unwrap();
    write_shepherd(state.path());
    let out = as_shepherd(state.path(), &["restart", "--self"], "");
    assert!(out.status.success(), "{out:?}");

    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["restart-wait", "shepherd"])
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_CONFIG_DIR", state.path().join("config"))
        .env("LOOPHERD_RESTART_WAITER_FD", "0")
        .env("PATH", "")
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("doesn't hold shepherd's self-restart claim"),
        "{out:?}"
    );
    let saved = fs::read_to_string(state.path().join("loopherd/shepherd.json")).unwrap();
    assert!(saved.contains(r#""state": "pending""#), "{saved}");
    assert!(shepherd_waiter_holds_its_claim(state.path()));

    let prompt = r#"{"hook_event_name":"UserPromptSubmit","prompt":"stop"}"#;
    as_shepherd(state.path(), &["prompt-hook"], prompt);
    let deadline = Instant::now() + Duration::from_secs(10);
    while shepherd_waiter_holds_its_claim(state.path()) {
        assert!(Instant::now() < deadline, "the waiter never exited");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_shepherds_restart_is_refused_with_the_switch_set_off() {
    let state = tempfile::tempdir().unwrap();
    write_shepherd(state.path());
    fs::write(
        state.path().join("config/config.toml"),
        "[shepherd]\nself_restart = false\n",
    )
    .unwrap();

    let out = as_shepherd(state.path(), &["restart", "--self"], "");

    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("self_restart is off"), "{said}");
}

#[test]
fn self_restart_under_a_planted_path_leaves_the_kept_path_as_it_was() {
    let state = tempfile::tempdir().unwrap();
    write_member(state.path());
    let _broker = start_broker(state.path());
    let record = state.path().join("config/sandbox/lh-1.lead.json");
    let kept = r#"{"herd": "h", "args": [], "path": "/usr/bin:/bin"}"#;
    fs::write(&record, kept).unwrap();
    let out = run(state.path(), &["report", "--kind", "handoff", "at step 3"]);
    assert!(out.status.success(), "{out:?}");

    let planted = state.path().join("planted");
    fs::create_dir_all(&planted).unwrap();
    let out = command(state.path(), &["restart", "--self"])
        .env("PATH", format!("{}:/usr/bin:/bin", planted.display()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    fs::remove_file(state.path().join("loopherd/handoffs/lh-1/handoff.md")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while waiter_holds_its_claim(state.path()) {
        assert!(Instant::now() < deadline, "the waiter never exited");
        std::thread::sleep(Duration::from_millis(100));
    }

    assert_eq!(fs::read_to_string(&record).unwrap(), kept);
}
