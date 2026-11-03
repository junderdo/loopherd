//! `scripts/install` with a fake cargo, and `install-hook`'s hook command run
//! the way Claude runs it.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A temp HOME and a PATH whose `cargo` "builds" a script that logs its
/// arguments, or fails when `build_fails`.
struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    path: String,
    log: PathBuf,
}

impl Sandbox {
    fn new(build_fails: bool) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        let log = dir.path().join("log");
        let built = dir.path().join("target/release/loopherd");
        fs::create_dir_all(built.parent().unwrap()).unwrap();
        executable(
            &built,
            &format!("#!/bin/sh\necho \"new $*\" >> '{}'\n", log.display()),
        );
        let cargo = match build_fails {
            true => "#!/bin/sh\necho 'error: could not compile' >&2\nexit 101\n".to_string(),
            false => format!(
                "#!/bin/sh\necho '{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\"loopherd\"}},\"executable\":\"{}\"}}'\necho '{{\"reason\":\"build-finished\",\"success\":true}}'\n",
                built.display()
            ),
        };
        executable(&fake_bin.join("cargo"), &cargo);
        let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
        Sandbox {
            _dir: dir,
            home,
            path,
            log,
        }
    }

    fn installed(&self) -> PathBuf {
        self.home.join(".local/bin/loopherd")
    }

    fn install(&self, args: &[&str]) -> Output {
        Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install"))
            .args(args)
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .output()
            .unwrap()
    }

    fn bin_dir_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.home.join(".local/bin"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

#[test]
fn install_copies_the_build_into_place_then_runs_install_hook() {
    let sandbox = Sandbox::new(false);

    let out = sandbox.install(&["--settings", "/tmp/s.json"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        fs::read_to_string(sandbox.installed())
            .unwrap()
            .contains("echo \"new $*\"")
    );
    assert_eq!(
        fs::read_to_string(&sandbox.log).unwrap(),
        "new install-hook --settings /tmp/s.json\n"
    );
    assert_eq!(sandbox.bin_dir_entries(), ["loopherd"]);
}

#[test]
fn a_failed_build_leaves_the_old_binary() {
    let sandbox = Sandbox::new(true);
    fs::create_dir_all(sandbox.installed().parent().unwrap()).unwrap();
    executable(&sandbox.installed(), "#!/bin/sh\necho old\n");

    let out = sandbox.install(&[]);

    assert!(!out.status.success());
    assert_eq!(
        fs::read_to_string(sandbox.installed()).unwrap(),
        "#!/bin/sh\necho old\n"
    );
    assert_eq!(sandbox.bin_dir_entries(), ["loopherd"]);
    assert!(!sandbox.log.exists());
}

#[test]
fn install_without_jq_stops_before_building() {
    let sandbox = Sandbox::new(false);
    let no_jq = tempfile::tempdir().unwrap();
    for tool in ["bash", "dirname", "cargo"] {
        let found = String::from_utf8(
            Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .env("PATH", &sandbox.path)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        std::os::unix::fs::symlink(found.trim(), no_jq.path().join(tool)).unwrap();
    }

    let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install"))
        .env("HOME", &sandbox.home)
        .env("PATH", no_jq.path())
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs jq"));
    assert!(!sandbox.installed().exists());
}

/// The command `install-hook` wrote for this test's binary, run by `sh` as
/// Claude runs hooks.
fn installed_hook_command(dir: &Path) -> String {
    let settings = dir.join("settings.json");
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["install-hook", "--settings"])
        .arg(&settings)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    for event in ["Stop", "StopFailure"] {
        assert_eq!(settings["hooks"][event][0]["hooks"][0]["timeout"], 10);
    }
    settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string()
}

fn run_hook(command: &str, state: &Path, pane: Option<&str>, input: &str) -> Output {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command])
        .env("XDG_STATE_HOME", state)
        .env("HERDR_SOCKET_PATH", "/run/herdr/herdr.sock")
        .env_remove("HERDR_PANE_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pane) = pane {
        cmd.env("HERDR_PANE_ID", pane);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn the_installed_hook_records_a_members_stop_and_exits_0_even_when_loopherd_fails() {
    let dir = tempfile::tempdir().unwrap();
    let command = installed_hook_command(dir.path());
    let state = dir.path().join("state");
    fs::create_dir_all(state.join("loopherd")).unwrap();
    fs::write(
        state.join("loopherd/ledger.json"),
        r#"{"lh-1": {"name": "lh-1", "pane_id": "w1:p1", "session_id": null, "repo": "/",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "socket": "/run/herdr/herdr.sock"}}"#,
    )
    .unwrap();

    let out = run_hook(
        &command,
        &state,
        Some("w1:p1"),
        r#"{"background_tasks":[],"session_crons":[]}"#,
    );
    assert_eq!(out.status.code(), Some(0));
    let mailbox = fs::read_to_string(state.join("loopherd/reports/lh-1.jsonl")).unwrap();
    assert!(mailbox.contains(r#""kind":"stop""#), "{mailbox}");

    fs::write(state.join("loopherd/ledger.json"), "not json").unwrap();
    let out = run_hook(&command, &state, Some("w1:p1"), "{}");
    assert_eq!(out.status.code(), Some(0), "a broken ledger");
    assert!(!out.stderr.is_empty());
}

/// The PreToolUse command `install-hook` wrote for this test's binary.
fn installed_tool_hook_command(dir: &Path) -> String {
    let settings = dir.join("settings.json");
    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["install-hook", "--settings"])
        .arg(&settings)
        .output()
        .unwrap();
    assert!(out.status.success());
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    let group = &settings["hooks"]["PreToolUse"][0];
    assert_eq!(group["matcher"], "Bash");
    group["hooks"][0]["command"].as_str().unwrap().to_string()
}

fn run_tool_hook(command: &str, dir: &Path, pane: Option<&str>, bash: &str) -> Output {
    let input = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": bash}});
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command])
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("LOOPHERD_CONFIG_DIR", dir.join("config"))
        .env("HERDR_SOCKET_PATH", "/run/herdr/herdr.sock")
        .env_remove("HERDR_PANE_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pane) = pane {
        cmd.env("HERDR_PANE_ID", pane);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.to_string().as_bytes()).unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn denial(out: &Output) -> Option<String> {
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    if out.stdout.is_empty() {
        return None;
    }
    let decision: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let output = &decision["hookSpecificOutput"];
    assert_eq!(output["hookEventName"], "PreToolUse");
    assert_eq!(output["permissionDecision"], "deny");
    Some(
        output["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn the_installed_tool_hook_denies_a_leads_merge_and_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let command = installed_tool_hook_command(dir.path());
    assert!(command.ends_with(" tool-hook || true"), "{command}");
    fs::create_dir_all(dir.path().join("state/loopherd")).unwrap();
    fs::write(
        dir.path().join("state/loopherd/ledger.json"),
        r#"{"lead-h": {"name": "lead-h", "pane_id": "w1:p1", "session_id": null, "repo": "/",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "socket": "/run/herdr/herdr.sock",
            "herd": "h", "role": "lead"}}"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("config/herds")).unwrap();
    let herd = dir.path().join("config/herds/h.toml");
    fs::write(
        &herd,
        "repo = \"/r\"\n[lead]\nprompt = \"go\"\nrebase = true\n",
    )
    .unwrap();

    let out = run_tool_hook(&command, dir.path(), Some("w1:p1"), "gh pr merge 3");
    let reason = denial(&out).expect("merge was allowed");
    assert!(reason.contains("`merge` switch is off"), "{reason}");

    let allowed = [
        (Some("w1:p1"), "git push --force"),
        (Some("w1:p1"), "cargo test"),
        (Some("w1:p2"), "gh pr merge 3"),
        (None, "gh pr merge 3"),
    ];
    for (pane, bash) in allowed {
        let out = run_tool_hook(&command, dir.path(), pane, bash);
        assert_eq!(denial(&out), None, "{pane:?} {bash}");
    }

    fs::remove_file(&herd).unwrap();
    let out = run_tool_hook(&command, dir.path(), Some("w1:p1"), "git push --force");
    let reason = denial(&out).expect("fails closed");
    assert!(reason.contains("couldn't be read"), "{reason}");
}

#[test]
fn an_installed_tool_hook_whose_binary_fails_with_exit_2_does_not_block() {
    let dir = tempfile::tempdir().unwrap();
    let real = installed_tool_hook_command(dir.path());
    let stale = dir.path().join("old bin/loopherd");
    fs::create_dir_all(stale.parent().unwrap()).unwrap();
    executable(
        &stale,
        "#!/bin/sh\necho \"error: unrecognized subcommand '$1'\" >&2\nexit 2\n",
    );
    // install-hook writes current_exe(), which is canonical; the env! path may hold `..`.
    let exe = fs::canonicalize(env!("CARGO_BIN_EXE_loopherd")).unwrap();
    let stale = stale.to_str().unwrap();
    let command = real.replacen(exe.to_str().unwrap(), stale, 1);
    assert!(
        command.contains(stale),
        "stale binary not substituted: {command}"
    );

    let out = run_tool_hook(&command, dir.path(), Some("w1:p1"), "cargo test");

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty());
}
