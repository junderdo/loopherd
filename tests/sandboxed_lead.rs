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
        .env_remove("LOOPHERD_BROKER_OF")
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
                "refused: this loopherd {}, so it can't run loopherd {command} itself",
                ledger_dir_shown(state.path())
            )),
            "{err}"
        );
    }
}

/// The ledger a lead's sandbox sees, with `lead-h` in pane `w1:p-lead` on
/// `socket`, its Claude recorded as `process`.
fn lead_s_state(state: &Path, socket: &Path, process: Option<serde_json::Value>) {
    let dir = state.join("loopherd");
    fs::create_dir_all(&dir).unwrap();
    let ledger = serde_json::json!({"lead-h": {
        "name": "lead-h", "pane_id": "w1:p-lead", "session_id": null,
        "repo": "/", "worktree": null, "workspace": "w1", "prompt": "lead",
        "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
        "role": "lead", "herd": "h", "socket": socket, "process": process}});
    fs::write(dir.join("ledger.json"), ledger.to_string()).unwrap();
}

/// This test process, as the ledger records a live lead's Claude.
fn alive() -> Option<serde_json::Value> {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let after_name = stat.rsplit_once(')').unwrap().1;
    let start: u64 = after_name
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    Some(serde_json::json!({"pid": std::process::id(), "start": start}))
}

/// This test's pid with a start it never had: a lead's Claude that exited.
fn dead() -> Option<serde_json::Value> {
    Some(serde_json::json!({"pid": std::process::id(), "start": 1}))
}

fn ledger_dir_shown(state: &Path) -> String {
    format!(
        "can't write the ledger (a lead's sandbox, or permissions on {})",
        state.join("loopherd").display()
    )
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
    (&["update"], UPDATE_WHY),
    (&["update", "--version", "v0.1.0"], UPDATE_WHY),
];

const UPDATE_WHY: &str =
    "updating loopherd is the human's unless the lead's herd sets [lead] install = true";

fn herd_h(state: &Path, lead: &str) {
    fs::create_dir_all(state.join("config/herds")).unwrap();
    fs::write(
        state.join("config/herds/h.toml"),
        format!("repo = \"/r\"\n[lead]\nprompt = \"go\"\n{lead}"),
    )
    .unwrap();
}

#[test]
fn a_lead_s_pane_refuses_update_while_its_herd_has_install_off() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, alive());
    herd_h(state.path(), "install = false\n");

    let err = refused(&run_in_lead_pane(state.path(), &socket, &["update"]));

    assert!(
        err.contains(&format!(
            "refused: lead lead-h runs this loopherd, and {UPDATE_WHY}"
        )),
        "{err}"
    );
}

#[test]
fn a_lead_s_pane_runs_update_when_its_herd_has_install_on() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, alive());
    herd_h(state.path(), "install = true\n");

    let err = refused(&run_in_lead_pane(state.path(), &socket, &["update"]));

    assert!(!err.contains("lead-h"), "{err}");
    assert!(err.contains("no install record"), "{err}");
}

#[test]
fn a_lead_s_pane_runs_update_check_with_install_off() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, alive());
    herd_h(state.path(), "install = false\n");

    let out = run_in_lead_pane(state.path(), &socket, &["update", "--check"]);

    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("lead-h"), "{err}");
}

#[test]
fn a_live_lead_s_pane_refuses_the_shepherd_s_and_human_s_commands_naming_why() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, alive());

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
                "refused: this loopherd {}, and {why}",
                ledger_dir_shown(state.path())
            )),
            "{args:?}: {err}"
        );
    }
}

#[test]
fn a_dead_lead_s_pane_runs_revive_lead_and_pause_for_the_human() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, dead());

    for args in [
        &["revive", "--dry-run"][..],
        &["lead", "h"],
        &["pause", "h"],
    ] {
        let out = run_in_lead_pane(state.path(), &socket, args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("refused"), "{args:?}: {out:?}");
    }
}

#[test]
fn a_lead_s_pane_with_no_recorded_process_says_to_run_it_from_another_pane() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, None);

    let err = refused(&run_in_lead_pane(state.path(), &socket, &["pause", "h"]));

    assert!(
        err.contains(
            "refused: this is lead lead-h's pane, and pausing a herd is the human's; the ledger \
             records no process to tell whether lead-h still runs here, so run it from another pane"
        ),
        "{err}"
    );
}

/// A state dir whose ledger can't be checked for writing: its lock is a directory.
fn unknowable_state(state: &Path) {
    fs::create_dir_all(state.join("loopherd/ledger.lock")).unwrap();
}

#[test]
fn commands_that_never_touch_the_ledger_run_when_its_writability_is_unknown() {
    let state = tempfile::tempdir().unwrap();
    unknowable_state(state.path());
    let herds = state.path().join("config/herds");
    fs::create_dir_all(&herds).unwrap();
    fs::write(
        herds.join("h.toml"),
        "repo = \"/\"\n[lead]\nprompt = \"go\"\n",
    )
    .unwrap();

    let set = run(state.path(), &["config", "set", "context_window", "200000"]);
    assert_eq!(set.status.code(), Some(0), "{set:?}");
    let herd_set = run(state.path(), &["herd", "set", "h", "lead.merge", "true"]);
    assert_eq!(herd_set.status.code(), Some(0), "{herd_set:?}");
    for args in [
        &["init", "--herd", "h"][..],
        &["setup", "--again"],
        &["merge-guard", "h"],
    ] {
        let out = run(state.path(), args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("ledger.lock"), "{args:?}: {out:?}");
    }
}

#[test]
fn a_lead_s_pane_still_runs_the_commands_that_only_read() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, alive());
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
    lead_s_state(state.path(), &socket, alive());

    let out = run_in_lead_pane(state.path(), &socket, &["revive", "--startup"]);

    // Past the refusal it reaches herdr, which isn't on this test's PATH.
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("loopherd revive: settle:"),
        "{out:?}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("revive on startup is off"),
        "{out:?}"
    );
}

#[test]
fn revive_on_startup_turned_on_gets_past_the_refusal_in_a_lead_s_focused_pane() {
    let state = tempfile::tempdir().unwrap();
    let socket = state.path().join("herdr.sock");
    lead_s_state(state.path(), &socket, alive());
    let config = state.path().join("config");
    fs::create_dir_all(config.join("herds")).unwrap();
    fs::write(
        config.join("config.toml"),
        "[herdr_plugin]\nrevive_on_startup = true\n",
    )
    .unwrap();

    let out = run_in_lead_pane(state.path(), &socket, &["revive", "--startup"]);

    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("refused"), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("revive on startup is off"));
}

#[test]
fn a_lead_s_own_revive_on_startup_is_refused_where_it_cannot_write_the_ledger() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    read_only_state(state.path());

    let out = run(state.path(), &["revive", "--startup"]);
    fs::set_permissions(
        state.path().join("loopherd"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let err = refused(&out);
    assert!(
        err.contains(&format!(
            "refused: this loopherd {}, and reviving members is the shepherd's",
            ledger_dir_shown(state.path())
        )),
        "{err}"
    );
}

/// Claude's Bash runs without a controlling terminal, so `/dev/tty` won't
/// open there; plain setup must then name its flags rather than prompt.
#[test]
fn setup_without_a_controlling_terminal_names_its_flags_instead_of_prompting() {
    use std::os::unix::process::CommandExt;
    let state = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    command
        .arg("setup")
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_CONFIG_DIR", state.path().join("config"))
        .env("PATH", "")
        .env_remove("HERDR_PANE_ID")
        .env_remove("LOOPHERD_BROKER_OF")
        .stdin(std::process::Stdio::piped());
    // SAFETY: setsid is async-signal-safe; a new session has no controlling terminal.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }

    let out = command.output().unwrap();

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("setup asked nothing (no terminal)"),
        "{out:?}"
    );
}
