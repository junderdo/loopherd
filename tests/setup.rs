//! `loopherd setup`: its questions on a terminal, its flags, and what it
//! leaves alone without one.

use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Config {
    _dir: tempfile::TempDir,
    dir: PathBuf,
}

impl Config {
    fn new(text: Option<&str>) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        if let Some(text) = text {
            fs::write(path.join("config.toml"), text).unwrap();
        }
        Config {
            dir: path,
            _dir: dir,
        }
    }

    fn text(&self) -> Option<String> {
        fs::read_to_string(self.dir.join("config.toml")).ok()
    }

    fn values(&self) -> toml::Table {
        self.text().unwrap_or_default().parse().unwrap()
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("LOOPHERD_CONFIG_DIR", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"));
    }

    /// Runs setup in a new session, so it has no terminal at all.
    fn setup(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        cmd.arg("setup").args(args).stdin(Stdio::null());
        self.env(&mut cmd);
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.output().unwrap()
    }

    /// Runs setup on a pty, typing `answers`, and returns what showed on it;
    /// None where util-linux `script` isn't available.
    fn setup_on_terminal(&self, args: &[&str], answers: &str) -> Option<String> {
        on_terminal(
            &format!(
                "exec '{}' setup {}",
                env!("CARGO_BIN_EXE_loopherd"),
                args.join(" ")
            ),
            answers,
            |cmd| self.env(cmd),
        )
    }
}

fn on_terminal(shell: &str, answers: &str, env: impl Fn(&mut Command)) -> Option<String> {
    let util_linux = Command::new("script")
        .arg("--version")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("util-linux"));
    if !util_linux {
        eprintln!("skipped: needs util-linux script");
        return None;
    }
    let mut cmd = Command::new("script");
    cmd.args(["-qec", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    env(&mut cmd);
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

fn succeeded(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn int(table: &toml::Table, path: &str) -> Option<i64> {
    let mut value = table;
    let mut keys = path.split('.').peekable();
    while let Some(key) = keys.next() {
        if keys.peek().is_none() {
            return value.get(key).and_then(toml::Value::as_integer);
        }
        value = value.get(key)?.as_table()?;
    }
    None
}

fn on(table: &toml::Table, switch: &str) -> Option<bool> {
    table
        .get("lead_defaults")?
        .get(switch)
        .and_then(toml::Value::as_bool)
}

#[test]
fn a_first_setup_asks_the_lead_switches_then_the_context_values() {
    let config = Config::new(Some("# mine\n"));

    // merge yes, install no, rebase yes, the rest by default; a 1M window; a
    // limit over its cap, then one under; Enter for the shepherd.
    let Some(out) = config.setup_on_terminal(&[], "y\nn\ny\n\n\n\n1000000\n900000\n500000\n\n")
    else {
        return;
    };

    assert!(
        out.contains("merge: merge a PR its reviewer verified? [y/N]"),
        "{out}"
    );
    assert!(out.contains("context_window: "), "{out}");
    assert!(out.contains("[200000]"), "{out}");
    assert!(out.contains("over the cap of 800000"), "{out}");
    assert!(
        out.contains("shepherd.context_limit: ") && out.contains("[500000]"),
        "{out}"
    );
    let values = config.values();
    assert_eq!(on(&values, "merge"), Some(true));
    assert_eq!(on(&values, "install"), Some(false));
    assert_eq!(on(&values, "rebase"), Some(true));
    assert_eq!(on(&values, "blocked_dialogs"), Some(false));
    assert_eq!(int(&values, "context_window"), Some(1_000_000));
    assert_eq!(int(&values, "default_context_limit"), Some(500_000));
    assert_eq!(int(&values, "shepherd.context_limit"), None);
    let text = config.text().unwrap();
    assert!(text.starts_with("# mine\n"), "{text}");
}

#[test]
fn enter_keeps_each_context_value_and_writes_nothing() {
    let text = "[lead_defaults]\nmerge = true\n";
    let config = Config::new(Some(text));

    let Some(out) = config.setup_on_terminal(&[], "\n\n\n") else {
        return;
    };

    assert!(!out.contains("merge a PR"), "{out}");
    assert!(out.contains("default_context_limit: "), "{out}");
    assert!(out.contains("[160000]"), "{out}");
    assert_eq!(config.text().unwrap(), text);
}

#[test]
fn once_both_groups_are_set_setup_asks_nothing_until_again() {
    let text = "context_window = 1000000\n\n[lead_defaults]\nmerge = true\n";
    let config = Config::new(Some(text));

    let Some(out) = config.setup_on_terminal(&[], "") else {
        return;
    };
    assert!(!out.contains('?') && !out.contains('['), "{out}");
    assert_eq!(config.text().unwrap(), text);

    // Keep merge, turn rebase on, the rest by default; keep the window, set the shepherd's limit.
    let Some(out) = config.setup_on_terminal(&["--again"], "\n\ny\n\n\n\n\n\n300000\n") else {
        return;
    };
    assert!(
        out.contains("merge: merge a PR its reviewer verified? [Y/n]"),
        "{out}"
    );
    assert!(
        out.contains("[1000000]") && out.contains("[250000]"),
        "{out}"
    );
    let values = config.values();
    assert_eq!(on(&values, "merge"), Some(true));
    assert_eq!(on(&values, "rebase"), Some(true));
    assert_eq!(int(&values, "context_window"), Some(1_000_000));
    assert_eq!(int(&values, "default_context_limit"), None);
    assert_eq!(int(&values, "shepherd.context_limit"), Some(300_000));
}

#[test]
fn a_bare_group_flag_asks_just_that_group() {
    let config = Config::new(None);

    let Some(out) = config.setup_on_terminal(&["--context"], "\n250000\n\n") else {
        return;
    };

    assert!(!out.contains("merge a PR"), "{out}");
    assert!(out.contains("over the cap of 160000"), "{out}");
    assert_eq!(config.text(), None);
}

#[test]
fn flags_set_values_without_asking() {
    let config = Config::new(None);

    let out = succeeded(&config.setup(&[
        "--lead-defaults",
        "merge,install",
        "--context",
        "window=1000000,shepherd=400000",
    ]));

    assert!(
        out.contains("set the lead defaults: merge,install"),
        "{out}"
    );
    let values = config.values();
    assert_eq!(on(&values, "install"), Some(true));
    assert_eq!(on(&values, "rebase"), Some(false));
    assert_eq!(int(&values, "context_window"), Some(1_000_000));
    assert_eq!(int(&values, "shepherd.context_limit"), Some(400_000));
    assert_eq!(int(&values, "default_context_limit"), None);
}

#[test]
fn a_bad_flag_is_refused_and_changes_nothing() {
    for args in [
        ["--lead-defaults=merj", "--context=limit=100000"],
        ["--lead-defaults=merge", "--context=limit=170000"],
        ["--lead-defaults=merge", "--context=size=500000"],
        ["--lead-defaults=merge", "--context=window=5000"],
        ["--lead-defaults=merge", "--context=window=lots"],
    ] {
        let config = Config::new(None);

        let out = config.setup(&args);

        assert!(!out.status.success(), "{args:?}: {out:?}");
        assert_eq!(config.text(), None, "{args:?}");
    }
}

#[test]
fn without_a_terminal_setup_asks_nothing_and_names_the_flags_for_whats_unset() {
    let config = Config::new(None);
    let out = succeeded(&config.setup(&[]));
    assert!(
        out.contains("--lead-defaults <switches|none>")
            && out.contains("--context window=<n>,limit=<n>,shepherd=<n>"),
        "{out}"
    );
    assert_eq!(config.text(), None);

    let config = Config::new(Some("[lead_defaults]\nmerge = true\n"));
    let out = succeeded(&config.setup(&["--context=limit=100000"]));
    assert!(
        !out.contains("--lead-defaults") && !out.contains("--context"),
        "{out}"
    );

    let config = Config::new(Some("[lead_defaults]\nreview_rounds = 5\n"));
    let out = succeeded(&config.setup(&["--context=limit=100000"]));
    assert!(out.contains("--lead-defaults <switches|none>"), "{out}");

    let config = Config::new(Some("[shepherd]\ncontext_limit = 100000\n"));
    let out = succeeded(&config.setup(&[]));
    assert!(
        out.contains("--lead-defaults") && !out.contains("--context"),
        "{out}"
    );

    let config = Config::new(None);
    let out = succeeded(&config.setup(&["--lead-defaults=merge"]));
    assert!(
        out.contains("set the lead defaults: merge")
            && out.contains("--context window=<n>,limit=<n>,shepherd=<n>")
            && !out.contains("--lead-defaults <"),
        "{out}"
    );

    let config = Config::new(None);
    let out = succeeded(&config.setup(&["--context=window=1000000"]));
    assert!(
        out.contains("--lead-defaults <switches|none>") && !out.contains("--context window="),
        "{out}"
    );
}

#[test]
fn a_smaller_window_is_refused_when_a_limit_already_set_goes_over_its_cap() {
    let text = "context_window = 1000000\ndefault_context_limit = 700000\n";
    let config = Config::new(Some(text));

    let out = config.setup(&["--context", "window=300000"]);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("default_context_limit 700000 is over the cap of 240000"),
        "{err}"
    );
    assert_eq!(config.text().unwrap(), text);

    let config = Config::new(Some("[shepherd]\ncontext_limit = 700000\n"));
    let out = config.setup(&["--context", "window=300000,limit=200000"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("shepherd.context_limit 700000"),
        "{out:?}"
    );

    let config = Config::new(Some(text));
    succeeded(&config.setup(&["--context", "window=300000,limit=200000"]));
    assert_eq!(
        config.values()["default_context_limit"].as_integer(),
        Some(200000)
    );
}

#[test]
fn an_invalid_config_fails_setup() {
    let text = "[lead_defaults]\nmerge = tru\n";
    let config = Config::new(Some(text));

    let out = config.setup(&[]);

    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("loopherd setup:"),
        "{out:?}"
    );
    assert_eq!(config.text().unwrap(), text);
}

#[test]
fn lead_defaults_prints_nothing_while_the_table_sets_only_review_rounds() {
    let config = Config::new(Some("[lead_defaults]\nreview_rounds = 5\n"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    cmd.arg("lead-defaults");
    config.env(&mut cmd);
    assert_eq!(succeeded(&cmd.output().unwrap()), "");
}

fn on_path(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn setup_names_the_package_when_claude_code_has_no_seccomp_filter_and_installs_nothing() {
    if !cfg!(target_os = "linux") || !on_path("bwrap") || !on_path("socat") {
        eprintln!("skipped: needs Linux with bubblewrap and socat");
        return;
    }
    let config = Config::new(Some(
        "context_window = 1000000\n\n[lead_defaults]\nmerge = true\n",
    ));
    // A claude that isn't Claude Code's native binary, with no npm package beside it.
    let bin = config.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    fs::write(&claude, "#!/bin/sh\necho '2.1.329 (Claude Code)'\n").unwrap();
    fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let empty_npm = config.dir.join("npm-root");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    cmd.arg("setup")
        .stdin(Stdio::null())
        .env("PATH", path)
        .env("npm_config_prefix", &empty_npm);
    config.env(&mut cmd);
    let out = cmd.output().unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains(
            "note: Claude Code's sandbox can't block Unix sockets here (Claude Code finds no \
             seccomp filter to block them with), so a sandboxed lead can reach herdr, your ssh \
             agent and WSL2's Windows interop; to close them: npm install -g \
             @anthropic-ai/sandbox-runtime"
        ),
        "{stdout}"
    );
    assert!(!empty_npm.exists(), "setup installed something");
}
