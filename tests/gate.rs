//! `loopherd gate` through a real broker process, whose own environment its
//! gate commands must not inherit.

mod brokers;
mod fakes;

use brokers::Broker;
use fakes::executable;
use std::fs;
use std::path::Path;
use std::process::Command;

const SOCKET: &str = "/run/herdr-test.sock";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().into()
}

/// A clone of a bare origin whose `feat/x` holds a commit on top of `main`; that commit's sha.
fn repo_with_pushed_branch(root: &Path) -> (std::path::PathBuf, String) {
    let origin = root.join("origin.git");
    let repo = root.join("repo");
    git(
        root,
        &[
            "init",
            "--quiet",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    git(
        root,
        &[
            "clone",
            "--quiet",
            origin.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    git(&repo, &["checkout", "--quiet", "-b", "main"]);
    git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "base"]);
    git(&repo, &["push", "--quiet", "origin", "main"]);
    git(&repo, &["remote", "set-head", "origin", "main"]);
    git(&repo, &["checkout", "--quiet", "-b", "feat/x"]);
    git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "pr"]);
    git(&repo, &["push", "--quiet", "origin", "feat/x"]);
    let sha = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "--quiet", "main"]);
    (repo, sha)
}

#[test]
fn a_gate_s_commands_run_without_the_broker_s_identity() {
    let root = tempfile::tempdir().unwrap();
    let (repo, sha) = repo_with_pushed_branch(root.path());
    let config = root.path().join("config");
    fs::create_dir_all(config.join("herds")).unwrap();
    fs::write(
        config.join("herds/k.toml"),
        format!(
            "repo = {repo:?}\n[lead]\nprompt = \"go\"\n\
             gate = [\"test -z \\\"$LOOPHERD_BROKER_OF\\\" || {{ echo inherited $LOOPHERD_BROKER_OF; exit 1; }}\"]\n"
        ),
    )
    .unwrap();
    let state = root.path().join("state/loopherd");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("ledger.json"),
        format!(
            r#"{{"lead-k": {{"name": "lead-k", "pane_id": "w1:p1", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
            "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
            "socket": "{SOCKET}", "herd": "k", "role": "lead"}}}}"#
        ),
    )
    .unwrap();
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    executable(
        &bin.join("herdr"),
        "#!/bin/sh\n[ \"$1 $2\" = 'agent list' ] && echo '{\"result\": {\"agents\": \
         [{\"name\": \"lead-k\", \"pane_id\": \"w1:p1\", \"tab_id\": \"w1:t1\", \"agent_status\": \"idle\"}]}}'\n",
    );
    let loopherd = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        cmd.env("XDG_STATE_HOME", root.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", &config)
            .env("CLAUDE_CONFIG_DIR", root.path().join("claude"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HERDR_SOCKET_PATH", SOCKET)
            .env_remove("CLAUDE_PID")
            .env_remove("LOOPHERD_BROKER_OF");
        cmd
    };
    let _broker = Broker::start(loopherd(), &config, "lead-k", "k");

    let gated = loopherd()
        .env("HERDR_PANE_ID", "w1:p1")
        .args(["gate", &sha])
        .output()
        .unwrap();

    let out = String::from_utf8_lossy(&gated.stdout);
    assert!(gated.status.success(), "{gated:?}");
    assert!(
        out.starts_with(&format!("gate {sha} merged with origin/main ")),
        "{out}"
    );
    assert!(out.trim_end().ends_with(": pass"), "{out}");
}
