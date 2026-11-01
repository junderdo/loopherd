//! End-to-end runs of `loopherd spawn` against the real `loopherd-dev` herdr
//! session. Skipped unless `LOOPHERD_LIVE=1`; start the session first with
//! `scripts/herdr-dev --start`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const HERDR_IDS: [&str; 4] = [
    "HERDR_ENV",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
];

fn live_enabled() -> bool {
    let on = std::env::var("LOOPHERD_LIVE").as_deref() == Ok("1");
    if !on {
        eprintln!("skipped: set LOOPHERD_LIVE=1 to run against loopherd-dev");
    }
    on
}

fn dev_socket() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap())
        .join(".config/herdr/sessions/loopherd-dev/herdr.sock")
}

/// A command whose herdr calls reach only the dev session.
fn dev(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.env("HERDR_SOCKET_PATH", dev_socket());
    for var in HERDR_IDS {
        cmd.env_remove(var);
    }
    cmd
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

fn labels(out: Output) -> Vec<String> {
    json(&out)["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["label"].as_str().unwrap().to_string())
        .collect()
}

/// Fails unless herdr resolves to the running dev session and that session
/// shares no workspace label with the inherited (live) one. Returns a dev
/// workspace label.
fn assert_dev_session() -> String {
    let inherited = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
    assert_ne!(inherited.as_deref(), Some(dev_socket().as_path()));

    let status = dev("herdr").arg("status").output().unwrap();
    let status = String::from_utf8_lossy(&status.stdout);
    let socket_line = format!("  socket: {}", dev_socket().display());
    assert!(
        status.lines().any(|l| l == socket_line),
        "herdr is not targeting the dev socket:\n{status}"
    );
    assert!(
        status.contains("status: running"),
        "loopherd-dev is not running; start it with scripts/herdr-dev --start"
    );

    let dev_labels = labels(dev("herdr").args(["workspace", "list"]).output().unwrap());
    let live = Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .unwrap();
    if live.status.success() {
        let live_labels = labels(live);
        for label in &dev_labels {
            assert!(
                !live_labels.contains(label),
                "dev session has live workspace label {label:?}"
            );
        }
    }
    dev_labels
        .into_iter()
        .next()
        .expect("loopherd-dev has no workspace")
}

/// Closes the agent's tab when dropped, so a failed assertion leaves no tab.
struct CloseTab(String);

impl Drop for CloseTab {
    fn drop(&mut self) {
        let out = dev("herdr")
            .args(["agent", "get", &self.0])
            .output()
            .unwrap();
        if out.status.success() {
            let tab = json(&out)["result"]["agent"]["tab_id"]
                .as_str()
                .unwrap()
                .to_string();
            dev("herdr").args(["tab", "close", &tab]).output().unwrap();
        }
    }
}

fn spawn(state: &Path, args: &[&str]) -> Output {
    dev(env!("CARGO_BIN_EXE_loopherd"))
        .env("XDG_STATE_HOME", state)
        .arg("spawn")
        .args(args)
        .output()
        .unwrap()
}

fn ledger_entry(state: &Path, name: &str) -> Value {
    let text = std::fs::read_to_string(state.join("loopherd/ledger.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()[name].clone()
}

#[test]
fn spawns_a_ready_agent_and_submits_its_prompt() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    let name = format!("lh-live-ready-{}", std::process::id());
    let _cleanup = CloseTab(name.clone());

    let out = spawn(
        state.path(),
        &[
            "--repo",
            env!("CARGO_MANIFEST_DIR"),
            "--name",
            &name,
            "--workspace",
            &workspace,
            "--prompt",
            "Reply with just the word ok.",
        ],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.starts_with("pane "), "{stdout}");
    let entry = ledger_entry(state.path(), &name);
    assert_eq!(entry["prompt_sent"], true);
    assert!(entry["session_id"].is_string(), "{entry}");
    assert!(stdout.contains(entry["session_id"].as_str().unwrap()));
}

#[test]
fn leaves_an_agent_blocked_at_a_dialog_and_records_it_unprompted() {
    if !live_enabled() {
        return;
    }
    let workspace = assert_dev_session();
    let state = tempfile::tempdir().unwrap();
    // A fresh directory makes Claude ask whether to trust it.
    let untrusted = tempfile::tempdir().unwrap();
    let name = format!("lh-live-blocked-{}", std::process::id());
    let _cleanup = CloseTab(name.clone());

    let out = spawn(
        state.path(),
        &[
            "--repo",
            untrusted.path().to_str().unwrap(),
            "--name",
            &name,
            "--workspace",
            &workspace,
            "--prompt",
            "never sent",
            "--ready-timeout",
            "5",
        ],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.starts_with("blocked: answer it in pane "),
        "{stdout}"
    );
    assert_eq!(ledger_entry(state.path(), &name)["prompt_sent"], false);
}
