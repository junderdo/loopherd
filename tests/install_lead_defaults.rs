//! `scripts/install`'s lead defaults: the questions on a terminal,
//! `--lead-defaults`, and when `config.toml` is left alone.

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

/// A temp HOME, state dir and `$LOOPHERD_CONFIG_DIR`, and a fake cargo whose
/// "build" runs the real loopherd for `lead-defaults` and nothing else.
struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    config_dir: PathBuf,
    settings: PathBuf,
    path: String,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&fake_bin).unwrap();
        let built = dir.path().join("built-loopherd");
        executable(
            &built,
            &format!(
                "#!/bin/sh\n[ \"$1\" != lead-defaults ] || exec '{}' \"$@\"\n",
                env!("CARGO_BIN_EXE_loopherd")
            ),
        );
        executable(
            &fake_bin.join("cargo"),
            &format!(
                "#!/bin/sh\necho '{{\"reason\":\"compiler-artifact\",\"target\":{{\"name\":\"loopherd\"}},\"executable\":\"{}\"}}'\n",
                built.display()
            ),
        );
        // Reports the plugin installed, so a run never reaches a real claude or the network.
        executable(
            &fake_bin.join("claude"),
            "#!/bin/sh\n[ \"$*\" != 'plugin list --json' ] || echo '[{\"id\":\"mattpocock-skills@claude-plugins-official\",\"scope\":\"user\"}]'\n",
        );
        let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
        Sandbox {
            home,
            state: dir.path().join("state"),
            config_dir: dir.path().join("config"),
            settings: dir.path().join("claude/settings.json"),
            path,
            _dir: dir,
        }
    }

    fn config(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    fn write_config(&self, text: &str) {
        fs::create_dir_all(&self.config_dir).unwrap();
        fs::write(self.config(), text).unwrap();
    }

    fn read_config(&self) -> Option<String> {
        fs::read_to_string(self.config()).ok()
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state)
            .env("LOOPHERD_CONFIG_DIR", &self.config_dir)
            .env("PATH", &self.path)
            .env("LOOPHERD_NO_SPLASH", "1")
            .env_remove("CLAUDE_CONFIG_DIR");
    }

    /// Runs the installer with stdin not a terminal.
    fn install(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(INSTALL);
        cmd.arg("--settings")
            .arg(&self.settings)
            .args(args)
            .stdin(Stdio::null());
        self.env(&mut cmd);
        cmd.output().unwrap()
    }

    /// Runs the installer on a pty, typing `answers`, and returns what it
    /// printed; None where util-linux `script` isn't available.
    fn install_on_terminal(&self, args: &[&str], answers: &str) -> Option<String> {
        let util_linux = Command::new("script")
            .arg("--version")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("util-linux"));
        if !util_linux {
            eprintln!("skipped: needs util-linux script");
            return None;
        }
        let shell = format!(
            "exec '{INSTALL}' --settings '{}' {}",
            self.settings.display(),
            args.join(" ")
        );
        let mut cmd = Command::new("script");
        cmd.args(["-qec", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.env(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answers.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        Some(String::from_utf8_lossy(&out.stdout).replace('\r', ""))
    }
}

fn succeeded(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const ALL_OFF: &str = "merge = false\ninstall = false\nrebase = false\nnew_cards = false\n\
                       design_escalation = false\nblocked_dialogs = false\n";

#[test]
fn a_first_install_on_a_terminal_asks_each_switch_and_keeps_the_rest_of_config_toml() {
    let sandbox = Sandbox::new();
    sandbox.write_config("# mine\ncontext_window = 1000000\n");

    // No to the agent types, then merge yes, install no, rebase yes, the rest by default.
    let Some(out) = sandbox.install_on_terminal(&[], "n\ny\nn\ny\n\n\n\n") else {
        return;
    };

    assert!(
        out.contains("merge: merge a PR its reviewer verified? [y/N]"),
        "{out}"
    );
    assert!(
        out.contains("blocked_dialogs: answer a member's blocking dialog? [y/N]"),
        "{out}"
    );
    assert_eq!(
        sandbox.read_config().unwrap(),
        "# mine\ncontext_window = 1000000\n\n[lead_defaults]\nmerge = true\ninstall = false\n\
         rebase = true\nnew_cards = false\ndesign_escalation = false\nblocked_dialogs = false\n"
    );
}

#[test]
fn once_lead_defaults_exist_a_plain_install_asks_nothing_about_them() {
    let sandbox = Sandbox::new();
    let text = "[lead_defaults]\nmerge = true\n";
    sandbox.write_config(text);

    let Some(out) = sandbox.install_on_terminal(&[], "n\n") else {
        return;
    };

    assert!(!out.contains("merge a PR"), "{out}");
    assert_eq!(sandbox.read_config().unwrap(), text);
}

#[test]
fn lead_defaults_alone_asks_again_with_the_current_values_as_defaults() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[lead_defaults]\nmerge = true\nrebase = true\n");

    // Keep merge, turn rebase off, the rest by default.
    let Some(out) = sandbox.install_on_terminal(&["--lead-defaults"], "\n\nn\n\n\n\n") else {
        return;
    };

    assert!(
        out.contains("merge: merge a PR its reviewer verified? [Y/n]"),
        "{out}"
    );
    assert!(!out.contains("agent types"), "{out}");
    assert_eq!(
        sandbox.read_config().unwrap(),
        "[lead_defaults]\nmerge = true\nrebase = false\ninstall = false\nnew_cards = false\n\
         design_escalation = false\nblocked_dialogs = false\n"
    );
}

#[test]
fn a_lead_defaults_list_sets_those_on_and_the_rest_off_without_asking() {
    let sandbox = Sandbox::new();

    succeeded(&sandbox.install(&["--lead-defaults", "merge,install"]));
    assert_eq!(
        sandbox.read_config().unwrap(),
        "[lead_defaults]\nmerge = true\ninstall = true\nrebase = false\nnew_cards = false\n\
         design_escalation = false\nblocked_dialogs = false\n"
    );

    succeeded(&sandbox.install(&["--lead-defaults=none"]));
    assert_eq!(
        sandbox.read_config().unwrap(),
        format!("[lead_defaults]\n{ALL_OFF}")
    );
}

#[test]
fn a_bad_list_fails_before_installing_anything() {
    for list in ["merj", "no", "ask", "reask", "merge,", "", "none,merge"] {
        let sandbox = Sandbox::new();

        let out = sandbox.install(&[&format!("--lead-defaults={list}")]);

        assert_eq!(out.status.code(), Some(2), "{list:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("unknown lead switch"),
            "{list:?}: {out:?}"
        );
        assert!(!sandbox.home.join(".local").exists(), "{list:?}");
        assert!(!sandbox.settings.exists(), "{list:?}");
        assert_eq!(sandbox.read_config(), None, "{list:?}");
    }
}

#[test]
fn without_a_terminal_or_with_no_extras_config_toml_is_left_alone() {
    let sandbox = Sandbox::new();

    let out = succeeded(&sandbox.install(&[]));
    assert!(out.contains("--lead-defaults"), "{out}");
    assert_eq!(sandbox.read_config(), None);

    let quiet = succeeded(&sandbox.install(&["--no-extras"]));
    assert!(!quiet.contains("lead defaults"), "{quiet}");
    assert_eq!(sandbox.read_config(), None);

    let both = sandbox.install(&["--no-extras", "--lead-defaults", "merge"]);
    assert_eq!(both.status.code(), Some(2), "{both:?}");
    assert_eq!(sandbox.read_config(), None);
}

#[test]
fn naming_another_extra_skips_the_lead_defaults_questions() {
    let sandbox = Sandbox::new();

    let Some(out) = sandbox.install_on_terminal(&["--with-skills"], "y\n") else {
        return;
    };

    assert!(!out.contains("merge a PR"), "{out}");
    assert_eq!(sandbox.read_config(), None);
}
