//! `loopherd status` through the binary, with a stub `herdr` on PATH.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run_status(state: &Path, bin: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("status")
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .unwrap()
}

/// A `herdr` that logs its args and answers `agent list` with `reply`, or
/// fails every call when `reply` is `None`.
fn stub_herdr(bin: &Path, reply: Option<&str>) -> std::path::PathBuf {
    let calls = bin.join("calls");
    let body = match reply {
        Some(json) => format!("echo '{json}'"),
        None => "echo 'herdr: server not running' >&2; exit 1".into(),
    };
    let shim = bin.join("herdr");
    fs::write(
        &shim,
        format!("#!/bin/sh\necho \"$@\" >> {}\n{body}\n", calls.display()),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    calls
}

fn write_ledger(state: &Path) -> String {
    let ledger = r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": "s1",
        "repo": "/nonexistent/repo", "worktree": null, "workspace": "w1",
        "prompt": null, "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
        "herd": "prs"}}"#;
    fs::create_dir_all(state.join("loopherd")).unwrap();
    fs::write(state.join("loopherd/ledger.json"), ledger).unwrap();
    ledger.into()
}

#[test]
fn status_reads_agent_list_once_and_leaves_the_ledger_alone() {
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let ledger = write_ledger(state.path());
    let calls = stub_herdr(
        bin.path(),
        Some(
            r#"{"result": {"agents": [{"name": "lh-1", "pane_id": "w1:p1", "tab_id": "w1:t1",
            "agent_status": "blocked", "agent_session": {"value": "s1"}}]}}"#,
        ),
    );
    let out = run_status(state.path(), bin.path(), &[]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "prs\n  lh-1  blocked  -  -  w1:t1/w1:p1\n\
         +0 strays (live in herdr, not in loopherd's ledger)\n"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(fs::read_to_string(calls).unwrap(), "agent list\n");
    assert_eq!(
        fs::read_to_string(state.path().join("loopherd/ledger.json")).unwrap(),
        ledger
    );
}

#[test]
fn status_with_herdr_unreachable_prints_unknowns_warns_and_exits_2() {
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    stub_herdr(bin.path(), None);
    let out = run_status(state.path(), bin.path(), &["--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["herdr_reachable"], false);
    assert_eq!(json["herds"][0]["members"][0]["status"], "unknown");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("loopherd status: warning: herdr unreachable"),
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn status_help_documents_its_flags() {
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["status", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in ["--all", "--strays", "--json", "unstable"] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn repo_on(dir: &Path, branch: &str) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--quiet", "--initial-branch", branch]);
}

#[test]
fn status_reads_each_worktrees_branch_despite_git_env_vars() {
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let wt = src.path().join("wt");
    let other = src.path().join("other");
    repo_on(&wt, "feat-x");
    repo_on(&other, "elsewhere");
    let ledger = format!(
        r#"{{"lh-1": {{"name": "lh-1", "pane_id": "w1:p1", "session_id": "s1",
        "repo": "/nonexistent/repo", "worktree": "{}", "workspace": "w1",
        "prompt": null, "prompt_sent": true, "created": "2026-11-01T00:00:00Z"}}}}"#,
        wt.display()
    );
    fs::create_dir_all(state.path().join("loopherd")).unwrap();
    fs::write(state.path().join("loopherd/ledger.json"), ledger).unwrap();
    stub_herdr(bin.path(), Some(r#"{"result": {"agents": []}}"#));
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("status")
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_CONFIG_DIR", state.path().join("config"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env("GIT_DIR", other.join(".git"))
        .env("GIT_WORK_TREE", &other)
        .env("GIT_INDEX_FILE", other.join(".git/index"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).lines().nth(1),
        Some("  lh-1  gone  feat-x  wt  w1:p1")
    );
}

#[test]
fn status_with_a_malformed_agent_list_names_herdrs_output_and_exits_3() {
    for reply in ["not json", r#"{"result": {"agents": "nope"}}"#] {
        let state = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let ledger = write_ledger(state.path());
        let calls = stub_herdr(bin.path(), Some(reply));
        let out = run_status(state.path(), bin.path(), &[]);
        assert_eq!(
            fs::read_to_string(&calls).unwrap(),
            "agent list\n",
            "{reply}"
        );
        assert_eq!(
            fs::read_to_string(state.path().join("loopherd/ledger.json")).unwrap(),
            ledger,
            "{reply}"
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "prs\n  lh-1  unknown  -  -  w1:p1\n\
             strays unknown (herdr's agent list unreadable)\n",
            "{reply}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.starts_with(
                "loopherd status: error: herdr's `agent list` output is malformed, \
                 every status is unknown: "
            ),
            "{reply}: {stderr}"
        );
        assert!(!stderr.contains("unreachable"), "{stderr}");
        assert_eq!(out.status.code(), Some(3), "{reply}");

        let json = run_status(state.path(), bin.path(), &["--json"]);
        let json: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(
            json["herdr_reachable"], true,
            "herdr answered, just not usably"
        );
    }
}

#[test]
fn status_needs_no_home_when_xdg_state_home_is_set() {
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    fs::create_dir_all(state.path().join("loopherd/paused")).unwrap();
    fs::write(
        state.path().join("loopherd/paused/prs"),
        r#"{"since": "2026-11-05T09:00:00Z", "reason": null}"#,
    )
    .unwrap();
    stub_herdr(bin.path(), Some(r#"{"result": {"agents": []}}"#));

    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("status")
        .env_clear()
        .env("XDG_STATE_HOME", state.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .output()
        .unwrap();

    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("prs  paused since 2026-11-05T09:00:00Z\n"),
        "{stdout}"
    );
    assert_eq!(out.status.code(), Some(0));
}
