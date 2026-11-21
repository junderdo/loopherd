//! `loopherd status` through the binary, with a stub `herdr` on PATH.

mod fakes;

use fakes::executable;
use std::fs;
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
    executable(
        &shim,
        format!("#!/bin/sh\necho \"$@\" >> {}\n{body}\n", calls.display()),
    );
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
        "repo": "{wt}", "worktree": "{wt}", "workspace": "w1",
        "prompt": null, "prompt_sent": true, "created": "2026-11-01T00:00:00Z"}}}}"#,
        wt = wt.display()
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

    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd status: warning: no config dir (set HOME or XDG_CONFIG_HOME), \
         so a paused herd with no members isn't shown\n"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("prs  paused since 2026-11-05T09:00:00Z\n"),
        "{stdout}"
    );
    assert_eq!(out.status.code(), Some(0));
}

fn write_herd(state: &Path, herd: &str, repo: &Path, queue: &str) {
    let herds = state.join("config/herds");
    fs::create_dir_all(&herds).unwrap();
    fs::write(
        herds.join(format!("{herd}.toml")),
        format!(
            "repo = {:?}\n[template.pr]\nparams = [\"number\"]\nname = \"lh-{{number}}\"\n\
             prompt = \"go\"\n[queue]\ntemplate = \"pr\"\n{queue}",
            repo.display().to_string()
        ),
    )
    .unwrap();
}

const ONE_BLOCKED: &str = r#"{"result": {"agents": [{"name": "lh-1", "pane_id": "w1:p1",
    "tab_id": "w1:t1", "agent_status": "blocked", "agent_session": {"value": "s1"}}]}}"#;

#[test]
fn status_counts_each_backlog_herds_claimable_and_unready_tasks() {
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    stub_herdr(bin.path(), Some(ONE_BLOCKED));
    fs::write(repo.path().join("ready"), "1\n2\n3\n").unwrap();
    fs::write(
        repo.path().join("unready"),
        "LH-7 Spec me\n\nLH-8 Ask the human\n",
    )
    .unwrap();
    write_herd(
        state.path(),
        "prs",
        repo.path(),
        "source = \"cat ready\"\nbacklog = \"cat unready\"\nmax = 1\n",
    );
    write_herd(
        state.path(),
        "idle",
        repo.path(),
        "source = \"echo first >&2; printf '\\\\033[31mboom' >&2; exit 3\"\nbacklog = \"true\"\nmax = 2\n",
    );
    write_herd(
        state.path(),
        "plain",
        repo.path(),
        "source = \"cat ready\"\nmax = 1\n",
    );

    let out = run_status(state.path(), bin.path(), &[]);

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "idle  claimable unknown (source failed (exit status: 3): first … boom), 0 unready\n\
         prs  2 claimable, 2 unready\n  lh-1  blocked  -  -  w1:t1/w1:p1\n\
         +0 strays (live in herdr, not in loopherd's ledger)\n"
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn status_json_gives_each_herds_counts_or_null() {
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    stub_herdr(bin.path(), Some(ONE_BLOCKED));
    fs::write(repo.path().join("ready"), "1\n2\n").unwrap();
    write_herd(
        state.path(),
        "prs",
        repo.path(),
        "source = \"cat ready\"\nbacklog = \"exit 4\"\nmax = 3\n",
    );
    write_herd(
        state.path(),
        "plain",
        repo.path(),
        "source = \"cat ready\"\nmax = 1\n",
    );
    let mut ledger: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(state.path().join("loopherd/ledger.json")).unwrap(),
    )
    .unwrap();
    ledger["lh-9"] = ledger["lh-1"].clone();
    ledger["lh-9"]["name"] = "lh-9".into();
    ledger["lh-9"]["herd"] = "plain".into();
    fs::write(
        state.path().join("loopherd/ledger.json"),
        ledger.to_string(),
    )
    .unwrap();

    let out = run_status(state.path(), bin.path(), &["--json"]);

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let herd = |name: &str| {
        json["herds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(herd("prs")["claimable"], 1);
    assert_eq!(herd("prs")["unready"], serde_json::Value::Null);
    assert_eq!(
        herd("prs")["unready_error"],
        "backlog failed (exit status: 4)"
    );
    assert_eq!(herd("plain")["claimable"], serde_json::Value::Null);
    assert_eq!(herd("plain")["unready"], serde_json::Value::Null);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_bad_git_timeout_warns_and_status_still_counts() {
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    stub_herdr(bin.path(), Some(ONE_BLOCKED));
    write_herd(
        state.path(),
        "prs",
        repo.path(),
        "source = \"echo 2\"\nbacklog = \"echo LH-7 Spec me\"\nmax = 1\n",
    );

    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("status")
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_CONFIG_DIR", state.path().join("config"))
        .env("LOOPHERD_GIT_TIMEOUT", "abc")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .output()
        .unwrap();

    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("prs  1 claimable, 1 unready\n"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("counting tasks with the default 2m"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_task_a_parked_member_holds_is_neither_claimable_nor_unready_but_a_released_ones_is_claimable()
{
    let state = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    write_ledger(state.path());
    let path = state.path().join("loopherd/ledger.json");
    let mut ledger: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for (name, released_at) in [("lh-2", None), ("lh-3", Some("2026-11-02T00:00:00Z"))] {
        let mut entry = ledger["lh-1"].clone();
        entry["name"] = name.into();
        entry["pane_id"] = format!("w1:{name}").into();
        entry["retired"] = true.into();
        entry["retired_at"] = "2026-11-01T12:00:00Z".into();
        entry["released_at"] = released_at.into();
        ledger[name] = entry;
    }
    fs::write(&path, ledger.to_string()).unwrap();
    stub_herdr(bin.path(), Some(ONE_BLOCKED));
    write_herd(
        state.path(),
        "prs",
        repo.path(),
        "source = \"printf '1\\\\n2\\\\n3\\\\n4\\\\n'\"\nbacklog = \"echo LH-9 Spec me\"\nmax = 1\n",
    );

    let out = run_status(state.path(), bin.path(), &["--json"]);

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["herds"][0]["name"], "prs");
    assert_eq!(json["herds"][0]["claimable"], 2, "only 3 (released) and 4");
    assert_eq!(json["herds"][0]["unready"], 1);
    assert_eq!(out.status.code(), Some(0));
}
