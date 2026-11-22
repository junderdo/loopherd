//! `loopherd install-herdr-keys`: binds the herdr plugin's actions in
//! herdr's `config.toml` under a temp `XDG_CONFIG_HOME`, against a stub
//! `herdr` that serves herdr 0.9.5's default config.

mod fakes;

use fakes::executable;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const REPO: &str = env!("CARGO_MANIFEST_DIR");

const ADDED: &str = r#"
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "loopherd.view"
description = "loopherd herd view"

[[keys.command]]
key = "prefix+shift+m"
type = "plugin_action"
command = "loopherd.view-beside-shepherd"
description = "loopherd view beside shepherd"
"#;

const HUMANS: &str = r#"# my herdr config
[ui]
mouse_capture = true # clicks

[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"

[[keys.command]]
key = "prefix+alt+g"
type = "popup"
command = "lazygit"
"#;

struct Sandbox {
    dir: tempfile::TempDir,
    stub: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("stub");
        fs::create_dir_all(&stub).unwrap();
        let stub_dir = stub.display();
        executable(
            &stub.join("herdr"),
            format!(
                r#"#!/bin/sh
echo "herdr $*" >> '{stub_dir}/log'
case "$*" in
  --default-config) cat '{stub_dir}/default-config' 2>/dev/null || cat '{REPO}/tests/fixtures/herdr-0.9.5-default-config.toml' ;;
  "status server --json")
    [ ! -e '{stub_dir}/status-fails' ] || {{ echo "herdr: socket error" >&2; exit 1; }}
    if [ -e '{stub_dir}/running' ]; then echo '{{"running":true,"version":"0.9.5"}}'; else echo '{{"running":false}}'; fi ;;
  "server reload-config")
    [ ! -e '{stub_dir}/reload-fails' ] || {{ echo "herdr: reload refused" >&2; exit 1; }}
    echo "reloaded" ;;
esac
"#
            ),
        );
        Sandbox { dir, stub }
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("xdg/herdr/config.toml")
    }

    fn write_config(&self, text: &str) {
        fs::create_dir_all(self.config().parent().unwrap()).unwrap();
        fs::write(self.config(), text).unwrap();
    }

    fn read_config(&self) -> String {
        fs::read_to_string(self.config()).unwrap()
    }

    fn herdr_calls(&self) -> Vec<String> {
        fs::read_to_string(self.stub.join("log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn backups(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.config().parent().unwrap()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".bak"))
            .collect()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loopherd"))
            .arg("install-herdr-keys")
            .args(args)
            .env("HOME", self.dir.path().join("home"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("xdg"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("LOOPHERD_CONFIG_DIR", self.dir.path().join("loopherd"))
            .env_remove("HERDR_CONFIG_PATH")
            .env_remove("HERDR_SOCKET_PATH")
            .env(
                "PATH",
                format!("{}:{}", self.stub.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap()
    }

    fn install(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

#[test]
fn a_missing_config_is_created_holding_only_the_bindings() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&[]);

    assert_eq!(sandbox.read_config(), ADDED.trim_start());
    assert!(out.contains("bound prefix+m to loopherd.view"), "{out}");
    assert!(sandbox.backups().is_empty());
}

#[test]
fn an_existing_config_keeps_every_line_and_is_backed_up_first() {
    let sandbox = Sandbox::new();
    sandbox.write_config(HUMANS);

    let out = sandbox.install(&[]);

    assert_eq!(sandbox.read_config(), format!("{HUMANS}{ADDED}"));
    let backups = sandbox.backups();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), HUMANS);
    assert!(
        out.contains(&format!("backup {}", backups[0].display())),
        "{out}"
    );
}

#[test]
fn a_rerun_changes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write_config(HUMANS);
    sandbox.install(&[]);

    let out = sandbox.install(&[]);

    assert_eq!(sandbox.read_config(), format!("{HUMANS}{ADDED}"));
    assert_eq!(sandbox.backups().len(), 1);
    assert!(
        out.contains("loopherd.view is already bound to prefix+m; left as is"),
        "{out}"
    );
}

#[test]
fn an_action_bound_to_another_key_is_left_alone() {
    let sandbox = Sandbox::new();
    let mine =
        "[[keys.command]]\nkey = \"f5\"\ntype = \"plugin_action\"\ncommand = \"loopherd.view\"\n";
    sandbox.write_config(mine);

    let out = sandbox.install(&[]);

    assert!(
        out.contains("loopherd.view is already bound to f5; left as is"),
        "{out}"
    );
    assert!(
        !sandbox.read_config().contains("prefix+m\""),
        "{}",
        sandbox.read_config()
    );
    assert!(
        sandbox
            .read_config()
            .contains("loopherd.view-beside-shepherd")
    );
}

#[test]
fn a_key_the_config_holds_is_skipped_naming_what_holds_it() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[keys]\nnew_tab = \"prefix+m\"\n\n[[keys.command]]\nkey = \"prefix+shift+m\"\ntype = \"shell\"\ncommand = \"make\"\n");

    let out = sandbox.install(&[]);

    assert!(
        out.contains(
            "skipped loopherd.view: prefix+m is already bound by keys.new_tab in your config"
        ),
        "{out}"
    );
    assert!(
        out.contains("skipped loopherd.view-beside-shepherd: prefix+shift+m is already bound by a [[keys.command]] running make"),
        "{out}"
    );
    assert!(!sandbox.read_config().contains("loopherd"));
    assert!(sandbox.backups().is_empty());
}

#[test]
fn a_key_herdrs_defaults_hold_is_skipped_unless_the_config_rebinds_that_action() {
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.stub.join("default-config"),
        "[keys]\n# goto = \"prefix+m\"\n# new_tab = \"prefix+shift+m\"\n\n[ui]\n",
    )
    .unwrap();
    sandbox.write_config("[keys]\nnew_tab = \"prefix+c\"\n");

    let out = sandbox.install(&[]);

    assert!(
        out.contains("skipped loopherd.view: prefix+m is already bound by herdr's default goto"),
        "{out}"
    );
    assert!(
        out.contains("bound prefix+shift+m to loopherd.view-beside-shepherd"),
        "{out}"
    );
}

#[test]
fn the_chosen_keys_are_unbound_in_herdr_0_9_5s_defaults() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&[]);

    assert!(!out.contains("skipped"), "{out}");
    assert!(out.contains("bound prefix+m to loopherd.view in"), "{out}");
    assert!(
        out.contains("bound prefix+shift+m to loopherd.view-beside-shepherd in"),
        "{out}"
    );
}

#[test]
fn remove_takes_out_only_loopherds_entries_leaving_the_rest_byte_identical() {
    let sandbox = Sandbox::new();
    sandbox.write_config(HUMANS);
    sandbox.install(&[]);
    let later = "\n[[keys.command]]\nkey = \"f6\"\ntype = \"plugin_action\"\ncommand = \"loopherd.status\"\ndescription = \"my status key\"\n\n[theme]\nname = \"dark\" # mine\n";
    let mut text = sandbox.read_config();
    text.push_str(later);
    sandbox.write_config(&text);

    let out = sandbox.install(&["--remove"]);

    assert_eq!(sandbox.read_config(), format!("{HUMANS}{later}"));
    assert!(
        out.contains("removed the prefix+m binding for loopherd.view"),
        "{out}"
    );
}

#[test]
fn remove_after_creating_the_file_leaves_it_empty() {
    let sandbox = Sandbox::new();
    sandbox.install(&[]);

    sandbox.install(&["--remove"]);

    assert_eq!(sandbox.read_config(), "");
}

#[test]
fn remove_without_bindings_writes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.write_config(HUMANS);

    let out = sandbox.install(&["--remove"]);

    assert!(out.contains("no loopherd key bindings in"), "{out}");
    assert!(sandbox.backups().is_empty());
    assert_eq!(sandbox.read_config(), HUMANS);
}

#[test]
fn a_running_server_reloads_its_config_after_a_change() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.stub.join("running"), "").unwrap();

    let out = sandbox.install(&[]);

    assert!(
        sandbox
            .herdr_calls()
            .contains(&"herdr server reload-config".to_string())
    );
    assert!(out.contains("reloaded herdr's config"), "{out}");
}

#[test]
fn without_a_running_server_nothing_is_reloaded() {
    let sandbox = Sandbox::new();

    let out = sandbox.install(&[]);

    assert!(
        !sandbox
            .herdr_calls()
            .contains(&"herdr server reload-config".to_string())
    );
    assert!(out.contains("no herdr server running"), "{out}");
}

#[test]
fn a_failed_reload_is_reported_and_the_bindings_stay() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.stub.join("running"), "").unwrap();
    fs::write(sandbox.stub.join("reload-fails"), "").unwrap();

    let out = sandbox.run(&[]);

    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("herdr: reload refused"), "{stderr}");
    assert_eq!(sandbox.read_config(), ADDED.trim_start());
}

#[test]
fn a_config_herdr_couldnt_parse_is_left_unchanged() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[keys\n");

    let out = sandbox.run(&[]);

    assert!(!out.status.success(), "{out:?}");
    assert_eq!(sandbox.read_config(), "[keys\n");
    assert!(sandbox.backups().is_empty());
}

#[test]
fn herdr_config_path_wins() {
    let sandbox = Sandbox::new();
    let elsewhere = sandbox.dir.path().join("elsewhere.toml");

    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["install-herdr-keys"])
        .env("HERDR_CONFIG_PATH", &elsewhere)
        .env("HOME", sandbox.dir.path().join("home"))
        .env("XDG_CONFIG_HOME", sandbox.dir.path().join("xdg"))
        .env("XDG_STATE_HOME", sandbox.dir.path().join("state"))
        .env("LOOPHERD_CONFIG_DIR", sandbox.dir.path().join("loopherd"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                sandbox.stub.display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(fs::read_to_string(elsewhere).unwrap(), ADDED.trim_start());
    assert!(!sandbox.config().exists());
}

#[test]
fn keys_are_matched_the_way_herdr_reads_them() {
    let sandbox = Sandbox::new();
    sandbox.write_config("[keys]\ngoto = \"prefix+M\"\nnew_tab = \"Prefix + M + Control\"\n");

    let out = sandbox.install(&[]);

    assert!(
        out.contains("skipped loopherd.view-beside-shepherd: prefix+shift+m is already bound by keys.goto in your config"),
        "{out}"
    );
    assert!(out.contains("bound prefix+m to loopherd.view"), "{out}");
}

#[test]
fn the_commented_example_in_herdrs_defaults_holds_no_key() {
    let sandbox = Sandbox::new();
    fs::write(
        sandbox.stub.join("default-config"),
        "[keys]\n# goto = \"prefix+g\"\n# [[keys.command]]\n# key = \"prefix+m\"\n",
    )
    .unwrap();

    let out = sandbox.install(&[]);

    assert!(out.contains("bound prefix+m to loopherd.view"), "{out}");
}

#[test]
fn an_inline_keys_command_array_is_left_for_the_human_to_bind() {
    let sandbox = Sandbox::new();
    let inline = "[keys]\ncommand = [{ key = \"f5\", type = \"shell\", command = \"make\" }]\n";
    sandbox.write_config(inline);

    let out = sandbox.run(&[]);

    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("keys.command is an inline array"),
        "{stderr}"
    );
    assert_eq!(sandbox.read_config(), inline);
}

#[test]
fn a_server_that_doesnt_answer_is_reported_not_called_stopped() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.stub.join("status-fails"), "").unwrap();

    let out = sandbox.run(&[]);

    assert!(out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("no herdr server running"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("run herdr server reload-config"),
        "{stderr}"
    );
}
