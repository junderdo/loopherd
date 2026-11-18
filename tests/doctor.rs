//! `loopherd doctor` through the binary, with a stub `herdr` on PATH and the
//! binary under test installed on PATH by a symlink.

mod fakes;

use fakes::executable;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const LOOPHERD: &str = env!("CARGO_BIN_EXE_loopherd");

struct Sandbox {
    dir: tempfile::TempDir,
    bin: PathBuf,
    settings: PathBuf,
}

/// A PATH holding the binary under test and a herdr whose server runs
/// `version`, or none when `None`.
fn sandbox(version: Option<&str>) -> Sandbox {
    let status = match version {
        Some(v) => format!(r#"{{"running":true,"version":"{v}","socket":"/run/stub/herdr.sock"}}"#),
        None => r#"{"running":false,"version":null,"socket":"/run/stub/herdr.sock"}"#.into(),
    };
    sandbox_with(&format!("echo '{status}'"))
}

/// A PATH holding the binary under test and a herdr that answers
/// `status server --json` by running `status`.
fn sandbox_with(status: &str) -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(LOOPHERD, bin.join("loopherd")).unwrap();
    executable(
        &bin.join("herdr"),
        format!("#!/bin/sh\n[ \"$*\" = 'status server --json' ] || exit 64\n{status}\n"),
    );
    let settings = dir.path().join("settings.json");
    Sandbox { dir, bin, settings }
}

impl Sandbox {
    fn loopherd(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(self.bin.join("loopherd"));
        command
            .args(args)
            .env("HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", self.dir.path().join("config"))
            .env("CLAUDE_CONFIG_DIR", self.dir.path().join("claude"))
            .env_remove("HERDR_SOCKET_PATH")
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()));
        command
    }

    /// `doctor` with `HERDR_SOCKET_PATH` set, as in a herdr pane.
    fn doctor_on(&self, socket: &str) -> Output {
        let settings = self.settings.to_str().unwrap();
        self.command(&["doctor", "--settings", settings])
            .env("HERDR_SOCKET_PATH", socket)
            .output()
            .unwrap()
    }

    fn install_hooks(&self) {
        let settings = self.settings.to_str().unwrap();
        let out = self.loopherd(&["install-hook", "--settings", settings]);
        assert!(out.status.success(), "{out:?}");
    }

    fn doctor(&self, args: &[&str]) -> Output {
        let settings = self.settings.to_str().unwrap();
        let mut all = vec!["doctor", "--settings", settings];
        all.extend(args);
        self.loopherd(&all)
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn a_sound_install_prints_one_line_and_exits_0() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let out = s.doctor(&[]);
    assert_eq!(stdout(&out), "doctor: 9 ok, 0 warnings, 0 failures\n");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn missing_hooks_in_the_given_settings_fail_naming_install_hook_and_exit_1() {
    let s = sandbox(Some("0.9.5"));
    let out = s.doctor(&[]);
    let fix = format!(
        "fix: loopherd install-hook --settings '{}'",
        s.settings.display()
    );
    let text = stdout(&out);
    assert!(
        text.starts_with(&format!(
            "failure Stop hook: not in {}; {fix}\n",
            s.settings.display()
        )),
        "{text}"
    );
    assert!(
        text.ends_with("doctor: 4 ok, 1 warnings, 4 failures\n"),
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn pane_label_on_makes_a_missing_prompt_hook_fail() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let mut settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&s.settings).unwrap()).unwrap();
    settings["hooks"]
        .as_object_mut()
        .unwrap()
        .remove("UserPromptSubmit");
    fs::write(&s.settings, settings.to_string()).unwrap();
    assert_eq!(s.doctor(&[]).status.code(), Some(0));
    let config = s.dir.path().join("config");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        "[notify]\nenabled = true\npane_label = true\n",
    )
    .unwrap();
    let out = s.doctor(&[]);
    assert!(
        stdout(&out).starts_with("failure UserPromptSubmit hook: not in"),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn a_herdr_not_running_or_too_old_fails() {
    for (version, line) in [
        (
            None,
            "failure herdr: no herdr server answers on /run/stub/herdr.sock; fix: herdr\n",
        ),
        (
            Some("0.9.4"),
            "failure herdr version: 0.9.4 is older than 0.9.5; fix: upgrade herdr to 0.9.5 or later and restart its server\n",
        ),
    ] {
        let s = sandbox(version);
        s.install_hooks();
        let out = s.doctor(&[]);
        assert!(stdout(&out).starts_with(line), "{}", stdout(&out));
        assert_eq!(out.status.code(), Some(1));
    }
}

#[test]
fn a_wedged_herdr_fails_after_the_deadline_instead_of_hanging() {
    let s = sandbox_with("sleep 60");
    s.install_hooks();
    let started = std::time::Instant::now();
    let out = s.doctor_on("/run/wedged.sock");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(
        stdout(&out).starts_with(
            "failure herdr: herdr didn't answer on /run/wedged.sock within 5s; \
             fix: restart herdr's server\n"
        ),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn herdr_is_asked_about_the_panes_socket() {
    let s = sandbox_with(
        r#"echo "{\"running\":false,\"version\":null,\"socket\":\"$HERDR_SOCKET_PATH\"}""#,
    );
    s.install_hooks();
    let out = s.doctor_on("/run/pane/herdr.sock");
    assert!(
        stdout(&out).starts_with(
            "failure herdr: no herdr server answers on /run/pane/herdr.sock; fix: herdr\n"
        ),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn a_failing_herdr_status_is_reported_without_install_advice() {
    let s = sandbox_with("echo 'protocol mismatch' >&2; exit 3");
    s.install_hooks();
    let out = s.doctor(&[]);
    assert!(
        stdout(&out).starts_with(
            "failure herdr: herdr status server --json failed: protocol mismatch; \
             fix: restart herdr's server\n"
        ),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn settings_given_doctor_runs_without_home_or_a_config_dir() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let out = Command::new(s.bin.join("loopherd"))
        .args(["doctor", "--settings", s.settings.to_str().unwrap()])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", s.bin.display()))
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "doctor: 9 ok, 0 warnings, 0 failures\n");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn running_a_binary_other_than_paths_warns_and_exits_0() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let other = s.dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    executable(&other.join("loopherd"), fs::read(LOOPHERD).unwrap());
    let out = Command::new(other.join("loopherd"))
        .args(["doctor", "--settings", s.settings.to_str().unwrap()])
        .env("HOME", s.dir.path())
        .env("LOOPHERD_CONFIG_DIR", s.dir.path().join("config"))
        .env_remove("HERDR_SOCKET_PATH")
        .env("PATH", format!("{}:/usr/bin:/bin", s.bin.display()))
        .output()
        .unwrap();
    assert!(
        stdout(&out).starts_with("warning running binary: this is "),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn verbose_lists_every_check_and_json_gives_them_as_data() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let verbose = stdout(&s.doctor(&["--verbose"]));
    assert_eq!(verbose.lines().count(), 10, "{verbose}");
    assert!(verbose.contains("ok herdr version: 0.9.5\n"), "{verbose}");
    let out = s.doctor(&["--json"]);
    let data: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(data["ok"], 9);
    assert_eq!(data["checks"][8]["name"], "herdr version");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn doctor_exits_2_when_it_cannot_run() {
    let s = sandbox(Some("0.9.5"));
    let out = Command::new(s.bin.join("loopherd"))
        .arg("doctor")
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", s.bin.display()))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("loopherd doctor: "),
        "{out:?}"
    );
}

#[test]
fn doctor_help_documents_its_flags() {
    let out = Command::new(LOOPHERD)
        .args(["doctor", "--help"])
        .output()
        .unwrap();
    let help = stdout(&out);
    for flag in ["--verbose", "--json", "--settings"] {
        assert!(help.contains(flag), "{help}");
    }
    let top = stdout(&Command::new(LOOPHERD).arg("--help").output().unwrap());
    assert!(top.contains("doctor"), "{top}");
}
