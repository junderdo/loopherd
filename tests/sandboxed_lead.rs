//! Commands a lead's sandbox runs, where the state dir is read-only and the
//! lead's Claude is out of sight of the parent-pid walk.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// A state dir whose ledger this process can't write, as inside a lead's sandbox.
fn read_only_state(state: &Path) {
    let dir = state.join("loopherd");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("ledger.json"),
        r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": null,
            "repo": "/", "worktree": null, "workspace": "w1", "prompt": "go",
            "prompt_sent": false, "created": "2026-11-01T00:00:00Z",
            "role": "worker", "herd": "other"}}"#,
    )
    .unwrap();
    fs::write(dir.join("ledger.lock"), "").unwrap();
    fs::set_permissions(dir.join("ledger.lock"), fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
}

/// No herdr on PATH, so a command that got past its refusal fails on herdr instead.
fn run(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("PATH", "")
        .env_remove("HERDR_PANE_ID")
        .output()
        .unwrap()
}

fn refused(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn commands_that_reach_herdr_refuse_a_run_that_cannot_write_the_ledger() {
    // Root writes past file modes, so the ledger can't be made read-only to it.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    read_only_state(state.path());

    let revive = run(state.path(), &["revive"]);
    let others =
        ["prompt", "restart"].map(|command| (command, run(state.path(), &[command, "lh-1"])));
    // Writable again, so the temp dir can be removed.
    fs::set_permissions(
        state.path().join("loopherd"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let err = refused(&revive);
    assert!(
        err.contains("refused: revive is the shepherd's or the human's"),
        "{err}"
    );
    for (command, out) in others {
        let err = refused(&out);
        assert!(
            err.contains(&format!(
                "refused: this loopherd runs in a sandbox, where it can't run loopherd {command} itself"
            )),
            "{err}"
        );
    }
}
