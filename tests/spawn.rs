mod fakes;

use fakes::executable;
use std::fs;
use std::path::Path;
use std::process::Output;

const SHIM: &str = r#"#!/bin/sh
case "$1 $2" in
  "status server") echo "$STATUS" ;;
  "agent list") echo '{"result": {"agents": []}}' ;;
  "workspace list") echo '{"result": {"workspaces": [{"workspace_id": "w1", "label": "ws"}]}}' ;;
  "tab create") echo '{"result": {"tab": {"tab_id": "w1:t1"}, "root_pane": {"pane_id": "w1:p1"}}}' ;;
  "pane process-info") echo '{"result": {"process_info": {"shell_pid": 7, "foreground_process_group_id": 7, "foreground_processes": [{"pid": 7}]}}}' ;;
  "pane read") echo '$' ;;
  "agent start") echo '{"result": {"agent": {"name": "a", "pane_id": "w1:p1", "tab_id": "w1:t1", "agent_status": "idle", "agent_session": null}}}' ;;
  "agent get") echo '{"result": {"agent": {"name": "a", "pane_id": "w1:p1", "tab_id": "w1:t1", "agent_status": "idle", "agent_session": {"value": "s1"}}}}' ;;
  *) echo "unexpected herdr $*" >&2; exit 1 ;;
esac
"#;

/// `loopherd spawn` with `HERDR_SOCKET_PATH` unset, against a fake `herdr`
/// whose `status server --json` prints `status`.
fn spawn_outside_herdr(state: &Path, status: &str) -> Output {
    let bin = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let shim = bin.path().join("herdr");
    executable(&shim, SHIM);
    std::process::Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["spawn", "--name", "a", "--workspace", "ws", "--repo"])
        .arg(repo.path())
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env("XDG_STATE_HOME", state)
        .env("STATUS", status)
        .output()
        .unwrap()
}

#[test]
fn a_spawn_outside_herdr_records_the_socket_herdr_reports() {
    let state = tempfile::tempdir().unwrap();
    let out = spawn_outside_herdr(
        state.path(),
        r#"{"status": "running", "socket": "/run/shim/herdr.sock"}"#,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&fs::read(state.path().join("loopherd/ledger.json")).unwrap())
            .unwrap();
    assert_eq!(ledger["a"]["socket"], "/run/shim/herdr.sock", "{ledger}");
}

#[test]
fn a_spawn_into_a_herdr_that_names_no_socket_is_refused_before_any_tab() {
    let state = tempfile::tempdir().unwrap();
    let out = spawn_outside_herdr(state.path(), r#"{"status": "running"}"#);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("HERDR_SOCKET_PATH"), "{stderr}");
    assert!(!stderr.contains("unexpected herdr tab"), "{stderr}");
    assert!(!state.path().join("loopherd/ledger.json").exists());
}

#[test]
fn a_spawn_goes_ahead_whatever_herds_are_paused() {
    let state = tempfile::tempdir().unwrap();
    let paused = state.path().join("loopherd/paused");
    fs::create_dir_all(&paused).unwrap();
    for herd in ["a", "prs"] {
        let pause = r#"{"since": "2026-11-05T09:00:00Z", "reason": null}"#;
        fs::write(paused.join(herd), pause).unwrap();
    }

    let out = spawn_outside_herdr(
        state.path(),
        r#"{"status": "running", "socket": "/run/shim/herdr.sock"}"#,
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let ledger = fs::read(state.path().join("loopherd/ledger.json")).unwrap();
    let ledger: serde_json::Value = serde_json::from_slice(&ledger).unwrap();
    assert!(ledger["a"].is_object(), "{ledger}");
}

#[test]
fn a_repo_git_names_is_ignored_without_a_dot_git_above_the_dir() {
    let bin = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let git = bin.path().join("git");
    executable(&git, "#!/bin/sh\necho /nonexistent/checkout/.git\n");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["spawn", "--name", "a", "--workspace", "ws"])
        .current_dir(dir.path())
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env("XDG_STATE_HOME", dir.path())
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("no .git in {} or above it", dir.path().display())),
        "{stderr}"
    );
    assert!(!stderr.contains("/nonexistent/checkout"), "{stderr}");
}
