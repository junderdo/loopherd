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

    let others =
        ["prompt", "restart"].map(|command| (command, run(state.path(), &[command, "lh-1"])));
    // Writable again, so the temp dir can be removed.
    fs::set_permissions(
        state.path().join("loopherd"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

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

/// The ledger a lead's sandbox sees, with `lead-h` in pane `w1:p-lead` on `socket`.
fn lead_s_state(state: &Path, socket: &Path) {
    let dir = state.join("loopherd");
    fs::create_dir_all(&dir).unwrap();
    let ledger = serde_json::json!({"lead-h": {
        "name": "lead-h", "pane_id": "w1:p-lead", "session_id": null,
        "repo": "/", "worktree": null, "workspace": "w1", "prompt": "lead",
        "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
        "role": "lead", "herd": "h", "socket": socket}});
    fs::write(dir.join("ledger.json"), ledger.to_string()).unwrap();
}

/// Runs `args` in lead-h's pane, found by its pane and socket alone, since
/// the sandbox hides the lead's Claude from the parent-pid walk.
fn run_in_lead_pane(state: &Path, socket: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env("LOOPHERD_CONFIG_DIR", state.join("config"))
        .env("PATH", "")
        .env("HERDR_PANE_ID", "w1:p-lead")
        .env("HERDR_SOCKET_PATH", socket)
        .output()
        .unwrap()
}

const REFUSED_TO_LEADS: &[(&[&str], &str)] = &[
    (&["lead", "h"], "starting a lead is the shepherd's"),
    (
        &["revive", "--dry-run"],
        "reviving members is the shepherd's",
    ),
    (&["pause", "h"], "pausing a herd is the human's"),
    (&["resume", "h"], "pausing a herd is the human's"),
    (
        &["herd", "set", "h", "lead.merge", "true"],
        "a herd file is the human's to change",
    ),
    (
        &["herd", "unset", "h", "lead.merge"],
        "a herd file is the human's to change",
    ),
    (
        &["herd", "remove", "h"],
        "a herd file is the human's to change",
    ),
    (
        &["init", "--herd", "h"],
        "a herd file is the human's to change",
    ),
    (
        &["lead-defaults", "--set", "merge"],
        "config.toml is the human's to change",
    ),
    (
        &["config", "set", "context_window", "200000"],
        "config.toml is the human's to change",
    ),
    (
        &["config", "unset", "context_window"],
        "config.toml is the human's to change",
    ),
    (
        &["setup", "--context", "window=200000"],
        "config.toml is the human's to change",
    ),
    (
        &["merge-guard", "h"],
        "a herd's credentials are the human's to set up",
    ),
    (
        &["broker", "lead-h"],
        "a lead's broker starts outside its sandbox",
    ),
    (
        &["restart-wait", "lead-h"],
        "a lead's broker starts its restart waiter",
    ),
];

#[test]
fn a_lead_s_pane_refuses_the_shepherd_s_and_human_s_commands_naming_why() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket);

    for (args, why) in REFUSED_TO_LEADS {
        let err = refused(&run_in_lead_pane(state.path(), &socket, args));
        assert!(
            err.contains(&format!(
                "refused: lead lead-h runs this loopherd, and {why}"
            )),
            "{args:?}: {err}"
        );
    }
    assert!(!state.path().join("config").exists());
}

#[test]
fn a_sandbox_whose_ledger_places_no_lead_in_its_pane_refuses_them_too() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    read_only_state(state.path());

    let outs: Vec<_> = REFUSED_TO_LEADS
        .iter()
        .map(|(args, why)| (args, why, run(state.path(), args)))
        .collect();
    fs::set_permissions(
        state.path().join("loopherd"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    for (args, why, out) in outs {
        let err = refused(&out);
        assert!(
            err.contains(&format!(
                "refused: this loopherd runs in a sandbox, and {why}"
            )),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn a_lead_s_pane_still_runs_the_commands_that_only_read() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket);
    let config = state.path().join("config");
    fs::create_dir_all(config.join("herds")).unwrap();
    fs::write(config.join("config.toml"), "context_window = 200000\n").unwrap();

    let get = run_in_lead_pane(state.path(), &socket, &["config", "get", "context_window"]);
    let plain_setup = run_in_lead_pane(state.path(), &socket, &["setup"]);

    assert_eq!(String::from_utf8_lossy(&get.stdout), "200000\n", "{get:?}");
    assert!(
        !String::from_utf8_lossy(&plain_setup.stderr).contains("refused"),
        "{plain_setup:?}"
    );
}

/// herdr runs the startup hook with its focused pane's id, which may be a lead's.
#[test]
fn revive_on_startup_runs_with_a_lead_s_pane_focused() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket);

    let out = run_in_lead_pane(state.path(), &socket, &["revive", "--startup"]);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("revive on startup is off"),
        "{out:?}"
    );
}
