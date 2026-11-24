//! `scripts/install` with a fake cargo, and `install-hook`'s hook command run
//! the way Claude runs it.

mod common;
mod fakes;

use common::feed_stdin;
use fakes::executable;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const INSTALL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install");
const DOCTOR_FAILS: &str = "doctor: 8 ok, 0 warnings, 1 failures";

/// A temp HOME and a PATH whose `cargo` "builds" a script that logs its
/// arguments, or fails when `build_fails`. Its `doctor` reports a failure
/// and exits 1; `exits` changes a subcommand's exit code.
struct Sandbox {
    dir: tempfile::TempDir,
    home: PathBuf,
    fake_bin: PathBuf,
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
            format!(
                "#!/bin/sh\necho \"new $*\" >> '{}'\n[ \"$1\" != doctor ] || echo '{DOCTOR_FAILS}'\n[ ! -f \"{exits}/$1\" ] || exit \"$(cat \"{exits}/$1\")\"\n",
                log.display(),
                exits = dir.path().join("exits").display(),
            ),
        );
        fs::create_dir_all(dir.path().join("exits")).unwrap();
        fs::write(dir.path().join("exits/doctor"), "1").unwrap();
        let cargo = match build_fails {
            true => "#!/bin/sh\necho 'error: could not compile' >&2\nexit 101\n".to_string(),
            false => format!(
                "#!/bin/sh\necho '{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\"loopherd\"}},\"executable\":\"{}\"}}'\necho '{{\"reason\":\"build-finished\",\"success\":true}}'\n",
                built.display()
            ),
        };
        executable(&fake_bin.join("cargo"), &cargo);
        // Reports the plugin installed, so a run never reaches a real claude or the network.
        executable(
            &fake_bin.join("claude"),
            "#!/bin/sh\n[ \"$*\" != 'plugin list --json' ] || echo '[{\"id\":\"mattpocock-skills@claude-plugins-official\",\"scope\":\"user\"}]'\n",
        );
        let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
        Sandbox {
            dir,
            home,
            fake_bin,
            path,
            log,
        }
    }

    fn installed(&self) -> PathBuf {
        self.home.join(".local/bin/loopherd")
    }

    fn exits(&self, subcommand: &str, code: i32) {
        fs::write(
            self.dir.path().join("exits").join(subcommand),
            code.to_string(),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(INSTALL);
        cmd.args(args)
            .env("HOME", &self.home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("XDG_STATE_HOME")
            .env("LOOPHERD_CONFIG_DIR", self.config())
            .env("PATH", &self.path);
        cmd
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config")
    }

    fn record(&self) -> Option<String> {
        fs::read_to_string(self.config().join("install.toml")).ok()
    }

    fn install(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs install on a `cols`x`rows` pty with stdin not the tty, or None
    /// where util-linux `script` isn't available.
    fn install_in_pty(&self, cols: u16, rows: u16) -> Option<String> {
        let mut cmd = pty(&format!(
            "stty cols {cols} rows {rows}; exec {INSTALL} --settings /tmp/s.json </dev/null"
        ))?;
        let out = cmd
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
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
        "new install-hook --settings /tmp/s.json\nnew setup\nnew doctor --settings /tmp/s.json\n"
    );
    assert_eq!(sandbox.bin_dir_entries(), ["loopherd"]);
}

#[test]
fn rerunning_install_on_an_unchanged_build_leaves_the_installed_file_alone() {
    use std::os::unix::fs::MetadataExt;
    let sandbox = Sandbox::new(false);
    let first = sandbox.install(&["--no-extras", "--settings", "/tmp/s.json"]);
    assert!(first.status.success(), "{first:?}");
    let before = fs::metadata(sandbox.installed()).unwrap();

    let again = sandbox.install(&["--no-extras", "--settings", "/tmp/s.json"]);

    assert!(again.status.success(), "{again:?}");
    let after = fs::metadata(sandbox.installed()).unwrap();
    assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
    assert_eq!(sandbox.bin_dir_entries(), ["loopherd"]);
}

#[test]
fn install_replaces_an_installed_binary_that_differs_from_the_build() {
    let sandbox = Sandbox::new(false);
    fs::create_dir_all(sandbox.installed().parent().unwrap()).unwrap();
    executable(&sandbox.installed(), "#!/bin/sh\necho old\n");

    let out = sandbox.install(&["--no-extras", "--settings", "/tmp/s.json"]);

    assert!(out.status.success(), "{out:?}");
    assert!(
        fs::read_to_string(sandbox.installed())
            .unwrap()
            .contains("echo \"new $*\"")
    );
}

#[test]
fn install_prints_a_failing_doctor_last_and_still_succeeds() {
    let sandbox = Sandbox::new(false);

    let out = sandbox.install(&["--no-extras", "--settings", "/tmp/s.json"]);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.trim_end().ends_with(DOCTOR_FAILS), "{stdout}");
}

#[test]
fn install_succeeds_when_doctor_cannot_run() {
    let sandbox = Sandbox::new(false);
    sandbox.exits("doctor", 2);

    let out = sandbox.install(&["--no-extras", "--settings", "/tmp/s.json"]);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn a_failed_install_hook_fails_the_install_before_doctor() {
    let sandbox = Sandbox::new(false);
    sandbox.exits("install-hook", 1);

    let out = sandbox.install(&["--no-extras", "--settings", "/tmp/s.json"]);

    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        fs::read_to_string(&sandbox.log).unwrap(),
        "new install-hook --settings /tmp/s.json\n"
    );
}

#[test]
fn install_remove_runs_no_doctor() {
    let sandbox = Sandbox::new(false);

    let out = sandbox.install(&["--remove", "--settings", "/tmp/s.json"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(&sandbox.log).unwrap(),
        "new install-hook --remove --settings /tmp/s.json\n"
    );
}

#[test]
fn install_records_a_source_install_of_its_checkout_over_a_release_record() {
    let sandbox = Sandbox::new(false);
    fs::create_dir_all(sandbox.config()).unwrap();
    fs::write(
        sandbox.config().join("install.toml"),
        "kind = \"release\"\ntag = \"v0.1.0\"\n",
    )
    .unwrap();

    let out = sandbox.install(&["--no-extras"]);

    assert!(out.status.success(), "{out:?}");
    let head = Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        sandbox.record().unwrap(),
        format!(
            "kind = \"source\"\ncheckout = \"{}\"\ncommit = \"{}\"\n",
            env!("CARGO_MANIFEST_DIR"),
            String::from_utf8_lossy(&head.stdout).trim()
        )
    );
}

#[test]
fn install_with_settings_writes_no_install_record() {
    let sandbox = Sandbox::new(false);

    let out = sandbox.install(&["--no-extras", "--settings", "/tmp/s.json"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(sandbox.record(), None);
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
fn help_prints_usage_without_building_or_installing() {
    for flag in ["-h", "--help"] {
        let sandbox = Sandbox::new(true);

        let out = sandbox.install(&[flag]);

        assert!(
            out.status.success(),
            "{flag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let usage = String::from_utf8_lossy(&out.stdout);
        assert!(usage.contains("--settings <file>"), "{flag}: {usage}");
        assert!(usage.contains("loopherd install-hook"), "{flag}: {usage}");
        assert!(!sandbox.home.join(".local").exists(), "{flag}");
        assert!(!sandbox.home.join(".claude").exists(), "{flag}");
        assert!(!sandbox.log.exists(), "{flag}");
    }
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

    let out = Command::new(INSTALL)
        .env("HOME", &sandbox.home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("XDG_STATE_HOME")
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

#[test]
fn feeding_a_hook_that_exited_without_reading_stdin_is_not_an_error() {
    let mut child = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    child.wait().unwrap();
    feed_stdin(stdin, b"{}");
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
    feed_stdin(child.stdin.take().unwrap(), input.as_bytes());
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
    let mailbox = fs::read_to_string(state.join("loopherd/reports/lh-1/mailbox.jsonl")).unwrap();
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
    feed_stdin(child.stdin.take().unwrap(), input.to_string().as_bytes());
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
fn the_installed_tool_hook_keeps_a_retired_leads_gates() {
    let dir = tempfile::tempdir().unwrap();
    let command = installed_tool_hook_command(dir.path());
    fs::create_dir_all(dir.path().join("state/loopherd")).unwrap();
    fs::write(
        dir.path().join("state/loopherd/ledger.json"),
        r#"{"lead-h": {"name": "lead-h", "pane_id": "w1:p1", "session_id": null, "repo": "/",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "socket": "/run/herdr/herdr.sock",
            "herd": "h", "role": "lead", "retired": true,
            "retired_at": "2026-11-02T00:00:00Z"}}"#,
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("config/herds")).unwrap();
    fs::write(
        dir.path().join("config/herds/h.toml"),
        "repo = \"/r\"\n[lead]\nprompt = \"go\"\nrebase = true\n",
    )
    .unwrap();

    for (bash, why) in [
        ("gh pr merge 3", "`merge` switch is off"),
        ("scripts/install", "`install` switch is off"),
        (
            "loopherd herd set h lead.merge true",
            "may not run `loopherd herd set`",
        ),
        ("loopherd pause h", "may not run `loopherd pause`"),
        ("loopherd retire lead-h", "may not retire a lead"),
    ] {
        let out = run_tool_hook(&command, dir.path(), Some("w1:p1"), bash);
        let reason = denial(&out).unwrap_or_else(|| panic!("{bash} was allowed"));
        assert!(
            reason.contains("lead-h") && reason.contains(why),
            "{reason}"
        );
    }
    for bash in ["git push --force", "loopherd retire lh-1", "cargo test"] {
        let out = run_tool_hook(&command, dir.path(), Some("w1:p1"), bash);
        assert_eq!(denial(&out), None, "{bash}");
    }
}

#[test]
fn the_installed_tool_hook_resolves_switches_through_lead_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let command = installed_tool_hook_command(dir.path());
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
    fs::write(
        dir.path().join("config/herds/h.toml"),
        "repo = \"/r\"\n[lead]\nprompt = \"go\"\nmerge = false\n",
    )
    .unwrap();
    let config = dir.path().join("config/config.toml");
    fs::write(&config, "[lead_defaults]\nmerge = true\nrebase = true\n").unwrap();

    let on_by_default = run_tool_hook(&command, dir.path(), Some("w1:p1"), "git push --force");
    assert_eq!(denial(&on_by_default), None);

    let overridden = run_tool_hook(&command, dir.path(), Some("w1:p1"), "gh pr merge 3");
    let reason = denial(&overridden).expect("the herd file's merge = false was ignored");
    assert!(reason.contains("`merge` switch is off"), "{reason}");

    fs::write(&config, "[lead_defaults]\nrebase = tru\n").unwrap();
    let broken = run_tool_hook(&command, dir.path(), Some("w1:p1"), "git push --force");
    let reason = denial(&broken).expect("a broken config.toml allowed a rebase");
    assert!(reason.contains("config.toml"), "{reason}");
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
    // install-hook writes current_exe(), canonical on Linux; the env! path may hold `..`.
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

#[test]
fn install_without_a_tty_writes_no_escape_codes() {
    let sandbox = Sandbox::new(false);

    let out = sandbox
        .command(&["--settings", "/tmp/s.json"])
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .env_remove("LOOPHERD_NO_SPLASH")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!out.stdout.contains(&0x1b), "{:?}", out.stdout);
    assert!(!out.stderr.contains(&0x1b), "{:?}", out.stderr);
}

#[test]
fn a_lead_defaults_list_is_consumed_and_not_passed_to_install_hook() {
    for args in [
        &[
            "--lead-defaults",
            "merge,install",
            "--settings",
            "/tmp/s.json",
        ][..],
        &["--lead-defaults=merge,install", "--settings", "/tmp/s.json"][..],
    ] {
        let sandbox = Sandbox::new(false);

        let out = sandbox.install(args);

        assert!(out.status.success(), "{args:?}: {out:?}");
        assert_eq!(
            fs::read_to_string(&sandbox.log).unwrap(),
            "new setup --lead-defaults=merge,install\nnew install-hook --settings /tmp/s.json\nnew doctor --settings /tmp/s.json\n",
            "{args:?}"
        );
    }
}

#[test]
fn no_splash_is_consumed_and_not_passed_to_install_hook() {
    let sandbox = Sandbox::new(false);

    let out = sandbox.install(&["--no-splash", "--settings", "/tmp/s.json"]);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(&sandbox.log).unwrap(),
        "new install-hook --settings /tmp/s.json\nnew setup\nnew doctor --settings /tmp/s.json\n"
    );
}

#[test]
fn help_documents_no_splash() {
    let sandbox = Sandbox::new(true);

    let out = sandbox.install(&["--help"]);

    assert!(String::from_utf8_lossy(&out.stdout).contains("--no-splash"));
}

/// `shell` run by `sh` on a pty with a colour TERM and no splash opt-outs, or
/// None where util-linux `script` isn't available.
fn pty(shell: &str) -> Option<Command> {
    let util_linux = Command::new("script")
        .arg("--version")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("util-linux"));
    if !util_linux {
        return None;
    }
    let mut cmd = Command::new("script");
    cmd.args(["-qec", shell, "/dev/null"])
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .env_remove("LOOPHERD_NO_SPLASH")
        .env_remove("COLUMNS")
        .env_remove("LINES")
        .stdin(Stdio::null());
    Some(cmd)
}

/// Runs `cmd` and, once `ready` holds for its output so far, types Ctrl+C
/// into its pty, so the whole foreground process group gets SIGINT, as from
/// a keyboard.
fn ctrl_c_when(cmd: &mut Command, ready: impl Fn(&[u8]) -> bool) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut seen = Vec::new();
    let mut chunk = [0; 4096];
    while !ready(&seen) {
        match stdout.read(&mut chunk).unwrap() {
            0 => break,
            read => seen.extend_from_slice(&chunk[..read]),
        }
    }
    child.stdin.take().unwrap().write_all(b"\x03").ok();
    stdout.read_to_end(&mut seen).unwrap();
    let mut out = child.wait_with_output().unwrap();
    out.stdout = seen;
    out
}

const ALT_SCREEN: &[u8] = b"\x1b[?1049h";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// True once `out` has entered the alternate screen and drawn `bytes` more.
fn drawn_past_alt_screen(out: &[u8], bytes: usize) -> bool {
    find(out, ALT_SCREEN).is_some_and(|at| out.len() >= at + ALT_SCREEN.len() + bytes)
}

fn without_escapes(text: &str) -> String {
    let mut plain = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            chars.by_ref().find(|c| c.is_ascii_alphabetic());
        } else {
            plain.push(c);
        }
    }
    plain
}

#[test]
fn the_splash_fits_the_terminal_on_stdout_even_without_tput() {
    let sandbox = Sandbox::new(false);
    executable(&sandbox.fake_bin.join("tput"), "#!/bin/sh\nexit 1\n");

    for (cols, rows, wordmark) in [
        (160, 45, true),
        (100, 30, true),
        (50, 30, false),
        (100, 8, false),
    ] {
        let Some(out) = sandbox.install_in_pty(cols, rows) else {
            eprintln!("skipped: needs util-linux script");
            return;
        };

        assert_eq!(out.contains('█'), wordmark, "{cols}x{rows}: {out}");
        let title = without_escapes(&out).contains("  loopherd\r\n");
        assert_eq!(title, !wordmark, "{cols}x{rows}: {out}");
    }
}

#[test]
fn install_without_the_splash_file_still_installs() {
    let dir = tempfile::tempdir().unwrap();
    let scripts = dir.path().join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    executable(
        &scripts.join("install"),
        fs::read_to_string(INSTALL).unwrap(),
    );
    fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-extras"),
        scripts.join("install-extras"),
    )
    .unwrap();
    fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-agents.bash"),
        scripts.join("install-agents.bash"),
    )
    .unwrap();
    fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-skills.bash"),
        scripts.join("install-skills.bash"),
    )
    .unwrap();
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/install-herdr-plugin.bash"
        ),
        scripts.join("install-herdr-plugin.bash"),
    )
    .unwrap();
    fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/install-herdr-keys.bash"
        ),
        scripts.join("install-herdr-keys.bash"),
    )
    .unwrap();
    let sandbox = Sandbox::new(false);

    let out = Command::new(scripts.join("install"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("XDG_STATE_HOME")
        .args(["--settings", "/tmp/s.json"])
        .env("HOME", &sandbox.home)
        .env("PATH", &sandbox.path)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(&sandbox.log).unwrap(),
        "new install-hook --settings /tmp/s.json\nnew setup\nnew doctor --settings /tmp/s.json\n"
    );
}

#[test]
fn an_interrupted_splash_returns_130_and_keeps_the_callers_traps() {
    let splash = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-splash.bash");
    let shell = format!(
        "stty cols 40 rows 10; exec bash -c 'trap \"echo bye\" EXIT; trap \"echo int\" INT; \
         source {splash}; splash_play; echo \"returned $?\"; trap -p'"
    );
    let Some(mut cmd) = pty(&shell) else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    let out = ctrl_c_when(&mut cmd, |out| find(out, HIDE_CURSOR).is_some());

    let out = without_escapes(&String::from_utf8_lossy(&out.stdout));
    assert!(out.contains("returned 130"), "{out}");
    assert!(out.contains("trap -- 'echo bye' EXIT"), "{out}");
    assert!(out.contains("trap -- 'echo int' SIGINT"), "{out}");
    assert!(out.trim_end().ends_with("bye"), "{out}");
}

#[test]
fn ctrl_c_during_the_splash_returns_130_to_a_set_e_caller_whose_exit_trap_runs() {
    let splash = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-splash.bash");
    let shell = format!(
        "stty cols 100 rows 30; exec bash -c 'set -e; trap \"echo exit \\$?\" EXIT; \
         source {splash}; splash_play; echo after'"
    );
    let Some(mut cmd) = pty(&shell) else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    let out = ctrl_c_when(&mut cmd, |out| drawn_past_alt_screen(out, 0));

    let raw = String::from_utf8_lossy(&out.stdout);
    let entered = raw
        .find("\x1b[?1049h")
        .expect("never entered the alternate screen");
    let restored = &raw[entered..];
    assert!(restored.contains("\x1b[?1049l"), "{raw:?}");
    assert!(restored.contains("\x1b[?25h"), "{raw:?}");
    let out = without_escapes(&raw);
    assert!(out.contains("exit 130"), "{out}");
    assert!(!out.contains("after"), "{out}");
}

#[test]
fn ctrl_c_during_the_splash_stops_install_with_130_before_building() {
    let sandbox = Sandbox::new(false);
    let Some(mut cmd) = pty(&format!(
        "stty cols 100 rows 30; {INSTALL} --settings /tmp/s.json; echo \"install exited $?\""
    )) else {
        eprintln!("skipped: needs util-linux script");
        return;
    };
    cmd.env("HOME", &sandbox.home).env("PATH", &sandbox.path);

    let out = ctrl_c_when(&mut cmd, |out| drawn_past_alt_screen(out, 0));

    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("install exited 130"), "{out}");
    assert!(!sandbox.installed().exists());
    assert!(!sandbox.log.exists());
}

const SPLASH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-splash.bash");

/// Stdout of `script` run by bash with the splash sourced, in colour mode
/// `colour`: "truecolor", "256" or "none".
fn splash_bash(colour: &str, script: &str) -> String {
    let mut cmd = Command::new("bash");
    cmd.args(["-euo", "pipefail", "-c"])
        .arg(format!("source {SPLASH}; {script}"))
        .env("LANG", "C.UTF-8")
        .env_remove("LC_ALL")
        .env_remove("LC_CTYPE")
        .env_remove("NO_COLOR")
        .env_remove("COLORTERM");
    match colour {
        "truecolor" => cmd.env("COLORTERM", "truecolor"),
        "none" => cmd.env("NO_COLOR", "1"),
        _ => &mut cmd,
    };
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// `buf` split at each cursor move into (row, column, what is drawn there).
fn segments(buf: &str) -> Vec<(usize, usize, String)> {
    let mut segments: Vec<(usize, usize, String)> = Vec::new();
    for piece in buf.split("\x1b[").skip(1) {
        let at = piece
            .split_once('H')
            .and_then(|(at, text)| Some((at.split_once(';')?, text)))
            .and_then(|((row, col), text)| Some((row.parse().ok()?, col.parse().ok()?, text)));
        match at {
            Some((row, col, text)) => segments.push((row, col, text.to_string())),
            None => segments.last_mut().unwrap().2 += &format!("\x1b[{piece}"),
        }
    }
    segments
}

#[test]
fn a_cloud_over_the_sun_keeps_the_suns_colour_behind_it() {
    // Scene row 7 crosses the sun's rim at columns 22..26 and 36..40 and its core
    // at 27..35; this cloud spans 20..38, so it starts off the sun.
    let sky = "splash_colours; splash_layout 30 100; \
               splash_clouds=('7|20|1|1000|&&&&&&&&&&&&&&&&&&&'); \
               splash_prepare; splash_sky 0 8 0; printf %s \"$splash_buf\"";
    let row_7 = |out: &str| {
        segments(out)
            .into_iter()
            .find(|&(row, ..)| row == 12)
            .unwrap()
            .2
    };
    for (colour, cloud, core, rim) in [
        (
            "truecolor",
            "38;2;235;235;242",
            "48;2;255;192;77",
            "48;2;255;154;31",
        ),
        ("256", "38;5;254", "48;5;214", "48;5;208"),
    ] {
        let row = row_7(&splash_bash(colour, sky));

        let expected = format!(
            "\x1b[{cloud}m▒▒\x1b[0m\x1b[{cloud};{rim}m▒▒▒▒▒\x1b[0m\
             \x1b[{cloud};{core}m▒▒▒▒▒▒▒▒▒\x1b[0m\x1b[{cloud};{rim}m▒▒▒\x1b[0m"
        );
        assert!(row.contains(&expected), "{colour}: {row:?}");
    }
    let row = row_7(&splash_bash("none", sky));
    assert!(!row.contains('\x1b'), "{row:?}");
    assert!(row.contains(&"▒".repeat(19)), "{row:?}");
}
#[test]
fn the_layout_is_chosen_by_terminal_size() {
    for (cols, rows, layout) in [
        (65, 30, "title"),
        (100, 22, "title"),
        (66, 23, "medium"),
        (139, 60, "medium"),
        (200, 39, "medium"),
        (140, 40, "large"),
    ] {
        let out = splash_bash(
            "none",
            &format!("splash_layout {rows} {cols}; printf %s \"$splash_layout\""),
        );

        assert_eq!(out, layout, "{cols}x{rows}");
    }
}

/// The first and last column each screen row of every frame is painted at,
/// 1-based, for a `cols`x`rows` terminal without colour.
fn painted_rows(cols: usize, rows: usize) -> std::collections::BTreeMap<usize, (usize, usize)> {
    let out = splash_bash(
        "none",
        &format!(
            "splash_colours; splash_layout {rows} {cols}; splash_prepare; \
             for ((f = 0; f < splash_frames; f++)); do splash_frame $f; printf %s \"$splash_buf\"; done"
        ),
    );
    let mut painted = std::collections::BTreeMap::new();
    for (row, col, text) in segments(&out) {
        let last = col + without_escapes(&text).chars().count() - 1;
        let span = painted.entry(row).or_insert((col, last));
        *span = (span.0.min(col), span.1.max(last));
    }
    painted
}

#[test]
fn the_medium_scene_sits_centred_in_its_box() {
    let painted = painted_rows(100, 30);

    assert_eq!(painted.keys().next(), Some(&5));
    assert_eq!(painted.keys().last(), Some(&25));
    assert!(
        painted.values().all(|&span| span == (20, 81)),
        "{painted:?}"
    );
}

#[test]
fn the_large_scene_fills_a_centred_area_with_nothing_drawn_outside_it() {
    // 80% of the terminal, capped at 180x50, centred: (cols, rows, left, right, top, bottom).
    for (cols, rows, left, right, top, bottom) in [
        (160, 45, 17, 144, 5, 40),
        (141, 41, 15, 126, 5, 36),
        (300, 80, 61, 240, 16, 65),
    ] {
        let painted = painted_rows(cols, rows);

        assert_eq!(
            painted.keys().copied().collect::<Vec<_>>(),
            (top..=bottom).collect::<Vec<_>>(),
            "{cols}x{rows}"
        );
        assert!(
            painted.values().all(|&span| span == (left, right)),
            "{cols}x{rows}: {painted:?}"
        );
    }
}

#[test]
fn the_large_scene_has_no_border_under_utf8_or_ascii() {
    for lang in ["C.UTF-8", "C"] {
        let out = splash_bash(
            "none",
            &format!(
                "LANG={lang}; splash_colours; splash_layout 45 160; splash_prepare; \
                 for ((f = 0; f < splash_frames; f++)); do splash_frame $f; printf %s \"$splash_buf\"; done"
            ),
        );
        let drawn = without_escapes(&out);

        for border in ['┌', '┐', '└', '┘', '─', '│', '+'] {
            assert!(!drawn.contains(border), "{lang}: {border}");
        }
    }
}

/// Rows and columns a sprite covers over the whole splash: (top, bottom, left, right).
type Swept = (i64, i64, i64, i64);

/// Every sheep's cells over the whole splash for a `cols`x`rows` terminal, as wide
/// as its widest sprite, each checked to stay on the scene's ground rows and columns.
fn swept_cells(cols: usize, rows: usize) -> Vec<Swept> {
    let out = splash_bash(
        "none",
        &format!(
            "splash_layout {rows} {cols}; splash_prepare; \
             echo \"$splash_frames $splash_width $splash_ground_top $splash_ground_rows\"; \
             for entry in \"${{splash_herd[@]}}\"; do \
               set -- $entry; width=0; \
               for pose in ra rb rg la lb lg; do \
                 splash_sheep_sprite $3 ${{pose:0:1}} ${{pose:1}}; \
                 for row in \"${{splash_sprite[@]}}\"; do ((${{#row}} > width)) && width=${{#row}}; done; \
               done; \
               echo \"$entry ${{#splash_sprite[@]}} $width\"; \
             done"
        ),
    );
    let mut lines = out.lines();
    let head: Vec<i64> = lines
        .next()
        .unwrap()
        .split(' ')
        .map(|n| n.parse().unwrap())
        .collect();
    let (frames, width, ground_top, ground_rows) = (head[0], head[1], head[2], head[3]);
    lines
        .map(|entry| {
            let f: Vec<&str> = entry.split(' ').collect();
            let num = |i: usize| f[i].parse::<i64>().unwrap();
            let (row, x, height, size) = (num(0), num(3), num(6), num(7));
            let travel = (frames - 1) / num(5);
            let (left, right) = match f[4] {
                "r" => (x, x + size - 1 + travel),
                "l" => (x - travel, x + size - 1),
                _ => (x, x + size - 1),
            };
            let bottom = row + height - 1;
            assert!(left >= 0 && right < width, "{cols}x{rows}: {entry}");
            assert!(
                row >= ground_top && bottom < ground_top + ground_rows,
                "{cols}x{rows}: {entry}"
            );
            (row, bottom, left, right)
        })
        .collect()
}

/// Whether `a` and `b` overlap or share an edge or corner.
fn touch(a: Swept, b: Swept) -> bool {
    a.0 <= b.1 + 1 && b.0 <= a.1 + 1 && a.2 <= b.3 + 1 && b.2 <= a.3 + 1
}

#[test]
fn sheep_never_touch_each_other_or_leave_the_pasture() {
    for (cols, rows) in [
        (100, 30),
        (140, 40),
        (140, 60),
        (141, 41),
        (147, 40),
        (151, 47),
        (151, 53),
        (160, 45),
        (203, 61),
        (226, 63),
        (400, 100),
    ] {
        let sheep = swept_cells(cols, rows);

        for (i, &a) in sheep.iter().enumerate() {
            for &b in &sheep[i + 1..] {
                assert!(!touch(a, b), "{cols}x{rows}: {a:?} touches {b:?}");
            }
        }
    }
}

#[test]
fn the_large_herd_grows_with_the_scenes_area_not_the_terminals() {
    let herd = |cols: usize, rows: usize| -> usize {
        splash_bash(
            "none",
            &format!("splash_layout {rows} {cols}; printf %s ${{#splash_herd[@]}}"),
        )
        .parse()
        .unwrap()
    };

    let mut last = 0;
    // The scene is 80% of the terminal, up to 180x50.
    for (cols, rows, area) in [
        (140, 40, 112 * 32),
        (160, 45, 128 * 36),
        (200, 60, 160 * 48),
        (226, 63, 180 * 50),
    ] {
        let sheep = herd(cols, rows);
        assert!(sheep > last, "{cols}x{rows}: {sheep} after {last}");
        let cells_per_sheep = area / sheep;
        assert!(
            (120..=300).contains(&cells_per_sheep),
            "{cols}x{rows}: {sheep}"
        );
        last = sheep;
    }
    assert_eq!(herd(400, 100), last);
    assert_eq!(herd(600, 150), last);
}

/// The ground rows of the 100x30 medium scene after `frame` with only `sheep`
/// in the herd, blank except for the sheep's own cells.
fn lone_sheep(sheep: &str, frame: usize) -> Vec<String> {
    splash_bash(
        "none",
        &format!(
            "splash_colours; splash_layout 30 100; splash_herd=('{sheep}'); splash_prepare; splash_pasture {frame}; \
             for ((d = 0; d < splash_ground_rows; d++)); do \
               text=${{splash_rows_text[d]}} kinds=${{splash_rows_kinds[d]}} line=; \
               for ((i = 0; i < ${{#text}}; i++)); do \
                 if [[ ${{kinds:i:1}} == w ]]; then line+=${{text:i:1}}; else line+=' '; fi; \
               done; \
               echo \"$line\"; \
             done"
        ),
    )
    .lines()
    .map(str::to_string)
    .collect()
}

#[test]
fn a_front_sheep_is_four_rows_tall_and_moves_its_legs_as_it_walks() {
    let before = lone_sheep("15 w 14 10 r 4", 0);
    let after = lone_sheep("15 w 14 10 r 4", 4);

    let drawn: Vec<usize> = (0..before.len())
        .filter(|&d| !before[d].trim().is_empty())
        .collect();
    assert_eq!(drawn, vec![6, 7, 8, 9], "{before:#?}");
    for d in 6..=8 {
        assert_eq!(format!(" {}", &before[d][..61]), after[d], "row {d}");
    }
    assert_ne!(format!(" {}", &before[9][..61]), after[9], "{after:#?}");
}

#[test]
fn a_grazing_sheep_lowers_its_head_to_the_grass() {
    let eye_row = |frame: usize| {
        lone_sheep("15 w 14 10 gr 4", frame)
            .iter()
            .position(|row| row.contains('o'))
    };

    assert_eq!(eye_row(0), Some(7));
    assert_eq!(eye_row(4), Some(9));
}

#[test]
fn a_sheep_facing_left_has_its_head_on_the_left() {
    let face = |motion: &str| {
        let rows = lone_sheep(&format!("15 w 14 30 {motion} 1000"), 0);
        let eye_row = &rows[7];
        (eye_row.find('o').unwrap(), eye_row.find('#').unwrap())
    };

    let (eye, body) = face("gl");
    assert!(eye < body, "eye {eye}, body {body}");
    let (eye, body) = face("gr");
    assert!(eye > body, "eye {eye}, body {body}");
}

#[test]
fn sheep_are_drawn_in_block_characters_under_utf8_and_ascii_otherwise() {
    let pasture = |lang: &str| -> String {
        let out = splash_bash(
            "none",
            &format!(
                "LANG={lang}; splash_colours; splash_layout 30 100; splash_prepare; \
                 splash_frame 0; printf %s \"$splash_buf\""
            ),
        );
        // The 100x30 scene's ground runs from screen row 14 to 23.
        segments(&out)
            .into_iter()
            .filter(|&(row, ..)| (14..=23).contains(&row))
            .map(|(.., text)| text)
            .collect()
    };

    assert!(pasture("C.UTF-8").contains('█'));
    assert!(pasture("C").is_ascii(), "{}", pasture("C"));
}

#[test]
fn ctrl_c_mid_frame_writes_nothing_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let err = dir.path().join("err");
    for drawn in [0, 20_000, 60_000] {
        let Some(mut cmd) = pty(&format!(
            "stty cols 160 rows 45; bash -c 'source {SPLASH}; splash_play' 2>{}",
            err.display()
        )) else {
            eprintln!("skipped: needs util-linux script");
            return;
        };

        ctrl_c_when(&mut cmd, |out| drawn_past_alt_screen(out, drawn));

        assert_eq!(fs::read_to_string(&err).unwrap(), "", "{drawn} bytes in");
    }
}

#[test]
fn the_splash_waits_without_starting_a_sleep_process() {
    let dir = tempfile::tempdir().unwrap();
    let sleeps = dir.path().join("sleeps");
    executable(
        &dir.path().join("sleep"),
        format!("#!/bin/sh\necho \"$@\" >> {}\n", sleeps.display()),
    );
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap()
    );

    // 100 frames of 50ms, or 8 letters of 0.1s: a wait that returns at once plays faster.
    for (size, at_least) in [
        ("cols 160 rows 45", Duration::from_millis(4500)),
        ("cols 40 rows 10", Duration::from_millis(700)),
    ] {
        let Some(mut cmd) = pty(&format!(
            "stty {size}; bash -c 'source {SPLASH}; splash_play'"
        )) else {
            eprintln!("skipped: needs util-linux script");
            return;
        };
        let started = Instant::now();
        let raw =
            String::from_utf8_lossy(&cmd.env("PATH", &path).output().unwrap().stdout).into_owned();
        let took = started.elapsed();

        assert!(
            raw.contains("\x1b[?25h"),
            "{size}: the splash never finished: {raw:?}"
        );
        assert!(took >= at_least, "{size}: the splash took only {took:?}");
    }

    assert_eq!(fs::read_to_string(&sleeps).unwrap_or_default(), "");
}

#[test]
fn the_splash_leaves_the_callers_last_background_pid_alone() {
    let Some(mut cmd) = pty(&format!(
        "stty cols 40 rows 10; bash -c 'source {SPLASH}; sleep 30 & job=$!; splash_play; \
         [[ $! == \"$job\" ]] && echo kept || echo \"lost $job to $!\"; kill \"$job\"'"
    )) else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    let out = String::from_utf8_lossy(&cmd.output().unwrap().stdout).into_owned();

    assert!(out.contains("kept"), "{out:?}");
}

#[test]
fn a_caller_with_a_strict_ifs_and_set_u_gets_the_whole_splash() {
    let dir = tempfile::tempdir().unwrap();
    let caller = dir.path().join("caller.bash");
    fs::write(
        &caller,
        format!(
            "set -euo pipefail\nIFS=$'\\n\\t'\ntrap 'echo exit $?' EXIT\n\
             source {SPLASH}\nsplash_play\necho after\n"
        ),
    )
    .unwrap();
    let Some(mut cmd) = pty(&format!("stty cols 160 rows 45; bash {}", caller.display())) else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    let raw = String::from_utf8_lossy(&cmd.output().unwrap().stdout).into_owned();

    assert!(raw.contains('█'), "the splash never played: {raw:?}");
    let out = without_escapes(&raw);
    assert!(out.contains("after"), "{out}");
    assert!(out.contains("exit 0"), "{out}");
}

fn agents_ledger(sandbox: &Sandbox) -> PathBuf {
    sandbox.home.join(".local/state/loopherd/agents.sha256")
}

fn repo_agent(name: &str) -> String {
    fs::read_to_string(format!("{}/agents/{name}.md", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn sha256(text: &str) -> String {
    use std::io::Write as _;
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap()[..64].to_string()
}

#[test]
fn update_runs_plain_setup_then_doctor_and_installs_no_agent_types_without_a_ledger() {
    let sandbox = Sandbox::new(false);

    let out = sandbox.install(&["--update"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(&sandbox.log).unwrap(),
        "new install-hook\nnew setup\nnew doctor\n"
    );
    assert!(!sandbox.home.join(".claude/agents").exists());
    assert!(sandbox.record().is_some());
}

#[test]
fn update_prints_the_skills_plugin_update_command_when_the_plugin_is_installed() {
    let sandbox = Sandbox::new(false);

    let out = sandbox.install(&["--update"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("claude plugin update mattpocock-skills@claude-plugins-official"),
        "{stdout}"
    );
}

#[test]
fn update_says_nothing_about_the_skills_plugin_when_it_isnt_installed() {
    let sandbox = Sandbox::new(false);
    executable(
        &sandbox.fake_bin.join("claude"),
        "#!/bin/sh\n[ \"$*\" != 'plugin list --json' ] || echo '[{\"id\":\"mattpocock-skills@claude-plugins-official\",\"scope\":\"project\"}]'\n",
    );

    let out = sandbox.install(&["--update"]);

    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("mattpocock-skills"), "{stdout}");
}

#[test]
fn update_refreshes_agent_types_loopherd_installed_when_the_ledger_exists() {
    let sandbox = Sandbox::new(false);
    let agents = sandbox.home.join(".claude/agents");
    fs::create_dir_all(&agents).unwrap();
    let old = "---\nname: builder\n---\nan older builder\n";
    fs::write(agents.join("builder.md"), old).unwrap();
    fs::create_dir_all(agents_ledger(&sandbox).parent().unwrap()).unwrap();
    fs::write(
        agents_ledger(&sandbox),
        format!("{} {}\n", sha256(old), agents.join("builder.md").display()),
    )
    .unwrap();

    let out = sandbox.install(&["--update"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(agents.join("builder.md")).unwrap(),
        repo_agent("builder")
    );
}

#[test]
fn update_off_a_terminal_keeps_an_edited_agent_type_with_a_warning() {
    let sandbox = Sandbox::new(false);
    let agents = sandbox.home.join(".claude/agents");
    fs::create_dir_all(&agents).unwrap();
    let installed = "---\nname: builder\n---\nan older builder\n";
    let edited = "---\nname: builder\n---\nmy own builder\n";
    fs::write(agents.join("builder.md"), edited).unwrap();
    fs::create_dir_all(agents_ledger(&sandbox).parent().unwrap()).unwrap();
    fs::write(
        agents_ledger(&sandbox),
        format!(
            "{} {}\n",
            sha256(installed),
            agents.join("builder.md").display()
        ),
    )
    .unwrap();

    let out = sandbox.install(&["--update"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(agents.join("builder.md")).unwrap(),
        edited
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("kept"), "{stdout}");
    assert!(
        stdout.contains("edited since loopherd installed it"),
        "{stdout}"
    );
}

#[test]
fn update_with_an_extra_or_a_setup_flag_is_an_error_that_builds_nothing() {
    for flag in [
        "--no-extras",
        "--with-agents",
        "--with-skills",
        "--with-herdr-plugin",
        "--with-herdr-keys",
        "--lead-defaults",
        "--context",
        "--revive-on-startup",
    ] {
        let sandbox = Sandbox::new(false);

        let out = sandbox.install(&["--update", flag]);

        assert_eq!(out.status.code(), Some(2), "{flag}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--update"),
            "{flag}: {out:?}"
        );
        assert!(!sandbox.log.exists(), "{flag}");
        assert!(!sandbox.installed().exists(), "{flag}");
    }
}

#[test]
fn update_on_a_terminal_shows_no_splash() {
    let sandbox = Sandbox::new(false);
    let Some(mut cmd) = pty(&format!(
        "stty cols 100 rows 30; exec {INSTALL} --update --settings /tmp/s.json </dev/null"
    )) else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    let out = cmd
        .env("HOME", &sandbox.home)
        .env("PATH", &sandbox.path)
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    assert!(find(&out.stdout, ALT_SCREEN).is_none(), "{out:?}");
}

#[test]
fn update_leaves_out_an_agent_type_the_human_removed() {
    let sandbox = Sandbox::new(false);
    let agents = sandbox.home.join(".claude/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("builder.md"), repo_agent("builder")).unwrap();
    fs::create_dir_all(agents_ledger(&sandbox).parent().unwrap()).unwrap();
    fs::write(
        agents_ledger(&sandbox),
        format!(
            "{} {}\n{} {}\n",
            sha256(&repo_agent("builder")),
            agents.join("builder.md").display(),
            sha256(&repo_agent("scout")),
            agents.join("scout.md").display()
        ),
    )
    .unwrap();

    let out = sandbox.install(&["--update"]);

    assert!(out.status.success(), "{out:?}");
    assert!(agents.join("builder.md").exists());
    assert!(!agents.join("scout.md").exists());
    assert!(!agents.join("reviewer.md").exists());
}

#[test]
fn update_installs_no_agent_types_where_the_ledger_names_only_another_dir() {
    let sandbox = Sandbox::new(false);
    fs::create_dir_all(agents_ledger(&sandbox).parent().unwrap()).unwrap();
    fs::write(
        agents_ledger(&sandbox),
        format!(
            "{} /elsewhere/agents/builder.md\n",
            sha256(&repo_agent("builder"))
        ),
    )
    .unwrap();

    let out = sandbox.install(&["--update"]);

    assert!(out.status.success(), "{out:?}");
    assert!(!sandbox.home.join(".claude/agents").exists());
}

#[test]
fn update_with_settings_names_the_config_dir_in_the_skills_plugin_update_command() {
    let sandbox = Sandbox::new(false);
    let claude = sandbox.dir.path().join("claude");
    fs::create_dir_all(&claude).unwrap();
    let settings = claude.join("settings.json");

    let out = sandbox.install(&["--update", "--settings", settings.to_str().unwrap()]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "CLAUDE_CONFIG_DIR={} claude plugin update mattpocock-skills@claude-plugins-official",
            claude.display()
        )),
        "{stdout}"
    );
}

#[test]
fn a_failed_install_hook_after_the_rename_leaves_the_install_record_as_it_was() {
    let sandbox = Sandbox::new(false);
    let before = format!(
        "kind = \"source\"\ncheckout = \"{}\"\ncommit = \"0000000000000000000000000000000000000000\"\n",
        env!("CARGO_MANIFEST_DIR")
    );
    fs::create_dir_all(sandbox.config()).unwrap();
    fs::write(sandbox.config().join("install.toml"), &before).unwrap();
    sandbox.exits("install-hook", 1);

    let out = sandbox.install(&["--no-extras"]);

    assert!(!out.status.success(), "{out:?}");
    assert!(sandbox.installed().exists());
    assert_eq!(sandbox.record().unwrap(), before);
}
