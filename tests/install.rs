//! `scripts/install` with a fake cargo, and `install-hook`'s hook command run
//! the way Claude runs it.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const INSTALL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install");

fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A temp HOME and a PATH whose `cargo` "builds" a script that logs its
/// arguments, or fails when `build_fails`.
struct Sandbox {
    _dir: tempfile::TempDir,
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
            fake_bin,
            path,
            log,
        }
    }

    fn installed(&self) -> PathBuf {
        self.home.join(".local/bin/loopherd")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(INSTALL);
        cmd.args(args)
            .env("HOME", &self.home)
            .env("PATH", &self.path);
        cmd
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
        .output()
        .unwrap();

    assert!(out.status.success());
    assert!(!out.stdout.contains(&0x1b), "{:?}", out.stdout);
    assert!(!out.stderr.contains(&0x1b), "{:?}", out.stderr);
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
        "new install-hook --settings /tmp/s.json\n"
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

/// Runs `cmd` and types Ctrl+C into its pty after `delay`, so the whole
/// foreground process group gets SIGINT, as from a keyboard.
fn ctrl_c_after(cmd: &mut Command, delay: std::time::Duration) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(delay);
    child.stdin.as_mut().unwrap().write_all(b"\x03").unwrap();
    child.wait_with_output().unwrap()
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
    fs::copy(INSTALL, scripts.join("install")).unwrap();
    let sandbox = Sandbox::new(false);

    let out = Command::new(scripts.join("install"))
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
        "new install-hook --settings /tmp/s.json\n"
    );
}

#[test]
fn an_interrupted_splash_returns_130_and_keeps_the_callers_traps() {
    let splash = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install-splash.bash");
    let shell = format!(
        "stty cols 40 rows 10; bash -c 'trap \"echo bye\" EXIT; trap \"echo int\" INT; \
         source {splash}; (sleep 0.3; kill -INT $$) & splash_play; echo \"returned $?\"; trap -p'"
    );
    let Some(mut cmd) = pty(&shell) else {
        eprintln!("skipped: needs util-linux script");
        return;
    };

    let out = without_escapes(&String::from_utf8_lossy(&cmd.output().unwrap().stdout));

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

    let out = ctrl_c_after(&mut cmd, std::time::Duration::from_millis(700));

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

    let out = ctrl_c_after(&mut cmd, std::time::Duration::from_millis(700));

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
    let sky = "splash_colours; splash_layout 30 100; splash_draw_sun; \
               splash_clouds=('7|20|1|1000|&&&&&&&&&&&&&&&&&&&'); \
               splash_sky 0 8 0; printf %s \"$splash_buf\"";
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
fn the_large_scene_fills_the_width_down_to_the_bottom_row() {
    let painted = painted_rows(160, 45);

    assert_eq!(
        painted.keys().copied().collect::<Vec<_>>(),
        (1..=45).collect::<Vec<_>>()
    );
    assert!(
        painted.values().all(|&span| span == (1, 160)),
        "{painted:?}"
    );
}
