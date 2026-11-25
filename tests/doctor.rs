//! `loopherd doctor` through the binary, with a stub `herdr` on PATH and the
//! binary under test installed on PATH by a symlink.

mod fakes;

use fakes::executable;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LOOPHERD: &str = env!("CARGO_BIN_EXE_loopherd");

/// Grill and wayfinding templates, so `herd upgrade` doesn't offer the herd them.
const GRILL: &str = "[template.grill]\nparams = []\nattended = true\nprompt = \"grill\"\n\
                     [template.wayfinding]\nparams = []\nprompt = \"answer\"\n";

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
/// `status server --json` by running `status`, and lists its plugins from
/// `plugins.json` in the sandbox, which starts with the loopherd plugin
/// installed at this crate's version.
fn sandbox_with(status: &str) -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(LOOPHERD, bin.join("loopherd")).unwrap();
    // So the sandbox check passes whatever this machine has installed.
    for program in ["bwrap", "socat"] {
        executable(&bin.join(program), "#!/bin/sh\n");
    }
    executable(
        &bin.join("claude"),
        "#!/bin/sh\necho '2.1.319 (Claude Code)'\n",
    );
    // Where an npm-installed Claude Code looks for its seccomp filter, refusing as it would.
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let filter = bin.join("vendor/seccomp").join(arch);
    fs::create_dir_all(&filter).unwrap();
    executable(
        &filter.join("apply-seccomp"),
        "#!/bin/sh\necho '2026/11/20 09:40:00 socat[7] E socket(1, 1, 0): Operation not permitted' >&2\nexit 1\n",
    );
    executable(
        &bin.join("herdr"),
        format!(
            "#!/bin/sh\ncase \"$*\" in\n\
             'status server --json') {status} ;;\n\
             'plugin list --plugin loopherd --json') [ -e '{dir}/stall' ] && sleep 60; cat '{dir}/plugins.json' ;;\n\
             *) exit 64 ;;\nesac\n",
            dir = dir.path().display()
        ),
    );
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/config.toml"),
        "context_window = 200000\n[lead_defaults]\nmerge = false\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("config/install.toml"),
        "kind = \"release\"\ntag = \"v0.2.0\"\n",
    )
    .unwrap();
    let sandbox = Sandbox {
        settings: dir.path().join("settings.json"),
        dir,
        bin,
    };
    sandbox.list_plugins(&[plugin(true, VERSION, "github")]);
    sandbox
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A plugin as `herdr plugin list --json` lists it, with `source.kind` `kind`.
fn plugin(enabled: bool, version: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({
        "plugin_id": "loopherd",
        "name": "loopherd",
        "enabled": enabled,
        "version": version,
        "plugin_root": "/src/loopherd/herdr-plugin",
        "source": {"kind": kind},
    })
}

impl Sandbox {
    fn list_plugins(&self, plugins: &[serde_json::Value]) {
        let reply = serde_json::json!({
            "id": "cli:plugin",
            "result": {"plugins": plugins, "type": "plugin_list"},
        });
        fs::write(self.dir.path().join("plugins.json"), reply.to_string()).unwrap();
    }

    fn loopherd(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_for(&self.bin.join("loopherd"), args)
    }

    fn command_for(&self, binary: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(binary);
        command
            .args(args)
            .env("HOME", self.dir.path())
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", self.dir.path().join("config"))
            .env("CLAUDE_CONFIG_DIR", self.dir.path().join("claude"))
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("LOOPHERD_BROKER_OF")
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
    assert_eq!(stdout(&out), "doctor: 19 ok, 0 warnings, 0 failures\n");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_missing_install_record_warns_naming_both_installers() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    fs::remove_file(s.dir.path().join("config/install.toml")).unwrap();
    let out = s.doctor(&[]);
    let said = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{said}");
    let record = s.dir.path().join("config/install.toml");
    assert!(
        said.contains(&format!(
            "warning install record: no install record at {}",
            record.display()
        )),
        "{said}"
    );
    assert!(
        said.contains("junderdo.dev/loopherd/install.sh | sh"),
        "{said}"
    );
    assert!(said.contains("`scripts/install` from a checkout"), "{said}");
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
        text.contains(&format!(
            "warning self_restart: [shepherd] self_restart is on, but loopherd's UserPromptSubmit \
             hook isn't in {}, so restart --self refuses; {fix}\n",
            s.settings.display()
        )),
        "{text}"
    );
    assert!(
        text.ends_with("doctor: 11 ok, 5 warnings, 4 failures\n"),
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
        "context_window = 200000\n[lead_defaults]\nmerge = false\n\
         [notify]\nenabled = true\npane_label = true\n",
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
    assert_eq!(
        stdout(&out),
        "warning config and state: not checked: HOME is not set; \
         fix: set HOME\n\
         doctor: 14 ok, 1 warnings, 0 failures\n"
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn without_home_doctor_skips_the_skill_check_beside_config_and_state() {
    let s = sandbox(Some("0.9.5"));
    let run = |args: &[&str]| s.command(args).env_remove("HOME").output().unwrap();
    let installed = run(&["install-hook"]);
    assert!(installed.status.success(), "{installed:?}");

    let out = run(&["doctor", "--verbose"]);

    let checks = stdout(&out);
    assert!(
        checks.contains("warning config and state: not checked: HOME is not set; fix: set HOME\n"),
        "{checks}"
    );
    assert!(checks.contains(" ok, 1 warnings, 0 failures\n"), "{checks}");
    assert!(!checks.contains(" skill:"), "{checks}");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn running_a_binary_other_than_paths_warns_and_exits_0() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let other = s.dir.path().join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    executable(&other.join("loopherd"), fs::read(LOOPHERD).unwrap());
    let settings = s.settings.to_str().unwrap();
    let out = s
        .command_for(&other.join("loopherd"), &["doctor", "--settings", settings])
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
    assert_eq!(verbose.lines().count(), 20, "{verbose}");
    assert!(verbose.contains("ok herdr version: 0.9.5\n"), "{verbose}");
    let out = s.doctor(&["--json"]);
    let data: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(data["ok"], 19);
    assert_eq!(data["checks"][12]["name"], "herdr version");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn the_herdr_plugin_only_ever_warns() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    for (plugins, line) in [
        (
            vec![],
            "warning herdr plugin: loopherd's herdr plugin isn't installed, so its actions, \
             popups and event hooks are unavailable; \
             fix: herdr plugin install junderdo/loopherd/herdr-plugin\n"
                .to_string(),
        ),
        (
            vec![plugin(false, VERSION, "github")],
            "warning herdr plugin: loopherd's herdr plugin is disabled, so its actions, \
             popups and event hooks are unavailable; fix: herdr plugin enable loopherd\n"
                .to_string(),
        ),
        (
            vec![plugin(true, "0.0.1", "local")],
            format!(
                "warning herdr plugin: plugin 0.0.1 differs from loopherd {VERSION}; \
                 fix: bring the linked plugin at /src/loopherd/herdr-plugin to loopherd {VERSION}\n"
            ),
        ),
    ] {
        s.list_plugins(&plugins);
        let out = s.doctor(&[]);
        assert_eq!(
            stdout(&out),
            format!("{line}doctor: 18 ok, 1 warnings, 0 failures\n")
        );
        assert_eq!(out.status.code(), Some(0));
    }
}

#[test]
fn a_plugin_list_herdr_refuses_warns() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    fs::remove_file(s.dir.path().join("plugins.json")).unwrap();
    let out = s.doctor(&[]);
    assert!(
        stdout(&out).starts_with("warning herdr plugin: can't list herdr's plugins: "),
        "{}",
        stdout(&out)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_plugin_list_failure_warns_with_a_diagnostic_not_a_retry() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    fs::remove_file(s.dir.path().join("plugins.json")).unwrap();
    let text = stdout(&s.doctor(&[]));
    assert!(
        text.contains("; fix: run `herdr plugin list --plugin loopherd` to see why\n"),
        "{text}"
    );
}

#[test]
fn a_stalled_plugin_list_warns_after_the_deadline_instead_of_hanging() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    fs::write(s.dir.path().join("stall"), "").unwrap();
    let started = std::time::Instant::now();
    let out = s.doctor(&[]);
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(
        stdout(&out).starts_with(
            "warning herdr plugin: can't list herdr's plugins: \
             herdr didn't answer plugin list within 5s; "
        ),
        "{}",
        stdout(&out)
    );
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
fn a_herd_narrows_the_herd_checks_and_an_unknown_one_exits_2() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let herds = s.dir.path().join("config/herds");
    let repo = s.dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(&herds).unwrap();
    fs::write(
        herds.join("h.toml"),
        format!("repo = {:?}\n{GRILL}", repo.to_str().unwrap()),
    )
    .unwrap();
    fs::write(herds.join("broken.toml"), "colour = 1\n").unwrap();
    assert_eq!(s.doctor(&[]).status.code(), Some(1));
    let out = s.doctor(&["h"]);
    assert_eq!(stdout(&out), "doctor: 21 ok, 0 warnings, 0 failures\n");
    assert_eq!(out.status.code(), Some(0));
    let out = s.doctor(&["nope"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("loopherd doctor: herd: read "),
        "{out:?}"
    );
}

#[test]
fn a_herdr_that_wedges_listing_agents_fails_after_the_deadline_instead_of_hanging() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    executable(
        &s.bin.join("herdr"),
        "#!/bin/sh\n\
         [ \"$*\" = 'status server --json' ] && \
         echo '{\"running\":true,\"version\":\"0.9.5\",\"socket\":\"/run/stub/herdr.sock\"}' && exit 0\n\
         sleep 60\n",
    );
    let state = s.dir.path().join("state/loopherd");
    fs::create_dir_all(&state).unwrap();
    let member = serde_json::json!({"a": {
        "name": "a", "pane_id": "w1:p1", "session_id": null, "repo": "/r",
        "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
        "created": "2026-11-01T00:00:00Z", "herd": "h",
    }});
    fs::write(state.join("ledger.json"), member.to_string()).unwrap();
    let started = std::time::Instant::now();
    let out = s.doctor(&[]);
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(
        stdout(&out),
        "warning herdr plugin: can't list herdr's plugins: \
         herdr didn't answer plugin list within 5s; \
         fix: run `herdr plugin list --plugin loopherd` to see why\n\
         failure members: herdr didn't list its agents within 5s; fix: restart herdr's server\n\
         doctor: 18 ok, 1 warnings, 1 failures\n"
    );
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn doctor_help_documents_its_flags() {
    let out = Command::new(LOOPHERD)
        .args(["doctor", "--help"])
        .output()
        .unwrap();
    let help = stdout(&out);
    for flag in ["--verbose", "--json", "--settings", "--trackers", "[HERD]"] {
        assert!(help.contains(flag), "{help}");
    }
    let top = stdout(&Command::new(LOOPHERD).arg("--help").output().unwrap());
    assert!(top.contains("doctor"), "{top}");
}

/// A herd whose `source` touches `ran` in its repo and prints two tasks.
fn herd_with_a_source(s: &Sandbox) -> PathBuf {
    let repo = s.dir.path().join("repo");
    let herds = s.dir.path().join("config/herds");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(&herds).unwrap();
    fs::write(
        herds.join("h.toml"),
        format!(
            "repo = {repo:?}\n[template.t]\nparams = [\"n\"]\nprompt = \"go\"\n\
             [queue]\ntemplate = \"t\"\nsource = \"touch ran; echo LH-1; echo LH-2\"\nmax = 1\n"
        ),
    )
    .unwrap();
    repo.join("ran")
}

#[test]
fn trackers_runs_each_herds_source_and_counts_its_tasks() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let ran = herd_with_a_source(&s);
    let out = s.doctor(&["--trackers", "--verbose"]);
    let text = stdout(&out);
    assert!(text.contains("ok h source: exit 0, 2 tasks\n"), "{text}");
    assert!(!text.contains("LH-1"), "{text}");
    assert!(ran.exists());
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_herd_narrows_trackers_to_that_herd() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let ran = herd_with_a_source(&s);
    let other = s.dir.path().join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    fs::write(
        s.dir.path().join("config/herds/o.toml"),
        format!(
            "repo = {other:?}\n[template.t]\nparams = [\"n\"]\nprompt = \"go\"\n\
             [queue]\ntemplate = \"t\"\nsource = \"touch ran\"\nmax = 1\n"
        ),
    )
    .unwrap();
    let out = s.doctor(&["o", "--trackers", "--verbose"]);
    let text = stdout(&out);
    assert!(text.contains("ok o source: exit 0, 0 tasks\n"), "{text}");
    assert!(!text.contains("h source"), "{text}");
    assert!(other.join("ran").exists());
    assert!(!ran.exists());
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn without_trackers_no_herd_command_runs() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let ran = herd_with_a_source(&s);
    let out = s.doctor(&["--verbose"]);
    assert!(!stdout(&out).contains("h source"), "{}", stdout(&out));
    assert!(!ran.exists());
}

#[test]
fn an_unknown_herd_with_trackers_exits_2_and_runs_no_source() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let ran = herd_with_a_source(&s);
    let out = s.doctor(&["nope", "--trackers"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(!ran.exists());
}

#[test]
fn a_missing_repo_fails_once_under_trackers() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    let ran = herd_with_a_source(&s);
    let repo = ran.parent().unwrap();
    fs::remove_dir_all(repo).unwrap();
    let out = s.doctor(&["--trackers"]);
    let text = stdout(&out);
    let failures: Vec<_> = text.lines().filter(|l| l.starts_with("failure")).collect();
    assert_eq!(
        failures,
        [format!(
            "failure herd h repo: {} is missing; fix: loopherd herd set h repo '\"<path>\"'",
            repo.display()
        )],
        "{text}"
    );
    assert_eq!(out.status.code(), Some(1));
}

/// A herd `h` whose lead lacks `follow_ups_ready`, the one pending upgrade,
/// with `declined` above its tables.
fn herd_pending_one_upgrade(s: &Sandbox, declined: &str) {
    let repo = s.dir.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(s.dir.path().join("config/herds")).unwrap();
    fs::write(
        s.dir.path().join("config/herds/h.toml"),
        format!(
            "repo = {repo:?}\n{declined}[template.t]\nparams = [\"n\"]\nprompt = \"go\"\n\
             {GRILL}[lead]\nprompt = \"lead {{herd}} to {{herd}} drained in {{review_rounds}} rounds\"\n"
        ),
    )
    .unwrap();
}

#[test]
fn a_herd_with_pending_upgrades_gets_an_info_line_never_a_failure() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    herd_pending_one_upgrade(&s, "");
    let without = sandbox(Some("0.9.5"));
    without.install_hooks();
    herd_pending_one_upgrade(&without, "declined = [\"lead-follow-ups-ready\"]\n");

    let out = s.doctor(&[]);

    let text = stdout(&out);
    assert!(
        text.contains(
            "info herd h upgrade: 1 new feature: lead-follow-ups-ready; fix: loopherd herd upgrade h\n"
        ),
        "{text}"
    );
    assert_eq!(
        out.status.code(),
        without.doctor(&[]).status.code(),
        "{text}"
    );
}

#[test]
fn declined_upgrades_get_no_info_line() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    herd_pending_one_upgrade(&s, "declined = [\"lead-follow-ups-ready\"]\n");

    let text = stdout(&s.doctor(&["--verbose"]));

    assert!(!text.contains("herd h upgrade"), "{text}");
}

#[test]
fn json_gives_pending_upgrades_the_info_status() {
    let s = sandbox(Some("0.9.5"));
    s.install_hooks();
    herd_pending_one_upgrade(&s, "");

    let doctor: serde_json::Value = serde_json::from_slice(&s.doctor(&["--json"]).stdout).unwrap();

    let upgrade = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "herd h upgrade")
        .unwrap();
    assert_eq!(upgrade["status"], "info");
}
