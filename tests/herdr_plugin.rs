//! The herdr plugin in `herdr-plugin/`: its manifest and the wrapper its
//! commands run loopherd through (D62).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("herdr-plugin")
}

fn manifest() -> toml::Table {
    let text = fs::read_to_string(plugin_dir().join("herdr-plugin.toml")).unwrap();
    text.parse().unwrap()
}

fn strings(value: &toml::Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

#[test]
fn manifest_names_the_loopherd_plugin_at_the_crate_version() {
    let m = manifest();
    assert_eq!(m["id"].as_str(), Some("loopherd"));
    assert_eq!(m["name"].as_str(), Some("loopherd"));
    assert_eq!(m["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(m["min_herdr_version"].as_str(), Some("0.7.5"));
    assert_eq!(strings(&m["platforms"]), ["linux", "macos"]);
}

#[test]
fn every_manifest_command_runs_through_the_wrapper() {
    let m = manifest();
    let mut commands = 0;
    for (key, value) in &m {
        let Some(items) = value.as_array() else {
            continue;
        };
        for item in items.iter().filter_map(|i| i.as_table()) {
            let Some(command) = item.get("command") else {
                continue;
            };
            assert_eq!(strings(command)[..2], ["sh", "loopherd.sh"], "[[{key}]]");
            commands += 1;
        }
    }
    assert!(commands > 0, "the manifest declares no commands");
}

#[test]
fn the_startup_hook_runs_the_startup_revive_and_nothing_else() {
    let m = manifest();
    let hooks = m["startup"].as_array().unwrap();
    let [hook] = hooks.as_slice() else {
        panic!("expected one startup hook: {hooks:?}");
    };
    assert_eq!(
        strings(&hook["command"]),
        ["sh", "loopherd.sh", "revive", "--startup"]
    );
}

/// Runs the wrapper as herdr does, from the plugin dir, with only `bin` on
/// `PATH` and `home` as `HOME`.
fn wrapper(bin: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new("/bin/sh")
        .arg("loopherd.sh")
        .args(args)
        .current_dir(plugin_dir())
        .env_clear()
        .env("PATH", bin)
        .env("HOME", home)
        .output()
        .unwrap()
}

fn fake_loopherd(dir: &Path, says: &str) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("loopherd");
    fs::write(&path, format!("#!/bin/sh\necho \"{says} $*\"\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn wrapper_runs_the_loopherd_on_path_with_its_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fake_loopherd(&bin, "on-path");
    fake_loopherd(&tmp.path().join(".local/bin"), "local-bin");

    let out = wrapper(&bin, tmp.path(), &["status", "--json"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "on-path status --json\n"
    );
}

#[test]
fn wrapper_falls_back_to_local_bin_when_path_has_no_loopherd() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fake_loopherd(&tmp.path().join(".local/bin"), "local-bin");

    let out = wrapper(&bin, tmp.path(), &["status"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "local-bin status\n");
}

#[test]
fn wrapper_fails_with_how_to_install_when_loopherd_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();

    let out = wrapper(&bin, tmp.path(), &["status"]);

    assert_eq!(out.status.code(), Some(127), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("loopherd plugin: loopherd not found on PATH or in ~/.local/bin")
            && stderr.contains("https://github.com/junderdo/loopherd#install"),
        "{stderr}"
    );
}

/// `loopherd revive --startup` with only `config` as its config dir and no
/// herdr on `PATH`, so any herdr call fails.
fn startup_revive(config: &Path) -> Output {
    startup_revive_without_socket(config)
        .env("HERDR_SOCKET_PATH", config.join("herdr.sock"))
        .output()
        .unwrap()
}

fn startup_revive_without_socket(config: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
    cmd.args(["revive", "--startup"])
        .env_clear()
        .env("PATH", config)
        .env("HOME", config)
        .env("LOOPHERD_CONFIG_DIR", config)
        .env("XDG_STATE_HOME", config.join("state"));
    cmd
}

#[test]
fn the_startup_revive_does_nothing_until_config_turns_it_on() {
    let config = tempfile::tempdir().unwrap();
    let file = config.path().join("config.toml");
    for text in [None, Some("[herdr_plugin]\nrevive_on_startup = false\n")] {
        if let Some(text) = text {
            fs::write(&file, text).unwrap();
        }

        let out = startup_revive(config.path());

        assert_eq!(out.status.code(), Some(0), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!(
                "revive on startup is off; set [herdr_plugin] revive_on_startup = true in {} to turn it on\n",
                file.display()
            )
        );
        assert!(!config.path().join("state").exists(), "{out:?}");
    }

    fs::write(&file, "[herdr_plugin]\nrevive_on_startup = true\n").unwrap();
    let out = startup_revive(config.path());
    assert!(
        !out.status.success(),
        "it ran without reaching herdr: {out:?}"
    );
    assert!(
        config
            .path()
            .join("state/loopherd/reports/revive.lock")
            .exists()
    );
}

#[test]
fn the_startup_revive_refuses_an_invalid_config() {
    let config = tempfile::tempdir().unwrap();
    fs::write(
        config.path().join("config.toml"),
        "[herdr_plugin]\nrevive_on_startup = \"yes\"\n",
    )
    .unwrap();

    let out = startup_revive(config.path());

    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("revive_on_startup"),
        "{out:?}"
    );
    assert!(!config.path().join("state").exists(), "{out:?}");
}

#[test]
fn the_startup_revive_refuses_without_the_hooks_socket() {
    let config = tempfile::tempdir().unwrap();
    fs::write(
        config.path().join("config.toml"),
        "[herdr_plugin]\nrevive_on_startup = true\n",
    )
    .unwrap();

    let out = startup_revive_without_socket(config.path())
        .output()
        .unwrap();

    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("HERDR_SOCKET_PATH is unset"),
        "{out:?}"
    );
    assert!(!config.path().join("state").exists(), "{out:?}");
}

fn one<'a>(m: &'a toml::Table, key: &str, id: &str) -> &'a toml::Value {
    m[key]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("no [[{key}]] with id {id}"))
}

#[test]
fn a_ctrl_clicked_github_pr_link_runs_loopherd_pr() {
    let m = manifest();
    let handlers = m["link_handlers"].as_array().unwrap();
    let [handler] = handlers.as_slice() else {
        panic!("expected one link handler: {handlers:?}");
    };
    assert_eq!(
        handler["pattern"].as_str(),
        Some("^https://github\\.com/[^/]+/[^/]+/pull/[0-9]+")
    );
    let action = one(&m, "actions", handler["action"].as_str().unwrap());
    assert_eq!(strings(&action["command"]), ["sh", "loopherd.sh", "pr"]);
}

#[test]
fn the_pr_popup_pane_runs_loopherd_pr_as_the_popup() {
    let m = manifest();
    let pane = one(&m, "panes", "pr-member");
    assert_eq!(pane["placement"].as_str(), Some("popup"));
    assert_eq!(
        strings(&pane["command"]),
        ["sh", "loopherd.sh", "pr", "--popup"]
    );
}

/// A stub on `bin` that appends its arguments to `<name>.args` and runs `then`.
fn stub(bin: &Path, name: &str, then: &str) {
    fs::create_dir_all(bin).unwrap();
    let path = bin.join(name);
    let log = bin.join(format!("{name}.args"));
    let script = format!("#!/bin/sh\necho \"$@\" >> '{}'\n{then}\n", log.display());
    fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn stub_args(bin: &Path, name: &str) -> Option<String> {
    fs::read_to_string(bin.join(format!("{name}.args"))).ok()
}

/// A ledger with one open member, `lh-1` on branch `feat/lh-1`.
fn ledger_with_lh_1(state: &Path) {
    let dir = state.join("loopherd");
    fs::create_dir_all(&dir).unwrap();
    let ledger = r#"{"lh-1": {
        "name": "lh-1", "pane_id": "w1:p1", "session_id": "abc",
        "repo": "/src/repo", "worktree": null, "branch": "feat/lh-1",
        "workspace": "w1", "claude_args": [], "prompt": null,
        "prompt_sent": true, "created": "2026-10-31T00:00:00Z",
        "retired": false, "herd": "loopherd"
    }}"#;
    fs::write(dir.join("ledger.json"), ledger).unwrap();
}

/// `loopherd pr` as the link handler runs it, on a click of `url`, with only
/// `bin` on `PATH`.
fn clicked(tmp: &Path, bin: &Path, url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("pr")
        .env_clear()
        .env("PATH", bin)
        .env("HOME", tmp)
        .env("XDG_STATE_HOME", tmp.join("state"))
        .env("LOOPHERD_CONFIG_DIR", tmp.join("config"))
        .env("HERDR_PLUGIN_CLICKED_URL", url)
        .output()
        .unwrap()
}

const PR: &str = "https://github.com/o/r/pull/12";

#[test]
fn a_pr_no_member_owns_opens_in_the_browser() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    ledger_with_lh_1(&tmp.path().join("state"));
    stub(&bin, "gh", "echo feat/someone-else");
    stub(&bin, "xdg-open", "");
    stub(&bin, "open", "");
    stub(&bin, "herdr", "exit 1");

    let out = clicked(tmp.path(), &bin, PR);

    assert!(out.status.success(), "{out:?}");
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    assert_eq!(
        stub_args(&bin, opener).as_deref(),
        Some(&*format!("{PR}\n"))
    );
    assert_eq!(stub_args(&bin, "herdr"), None);
}

#[test]
fn a_pr_on_a_members_branch_opens_the_popup() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    ledger_with_lh_1(&tmp.path().join("state"));
    stub(&bin, "gh", "echo feat/lh-1");
    stub(&bin, "xdg-open", "");
    stub(&bin, "open", "");
    stub(&bin, "herdr", "");

    let out = clicked(tmp.path(), &bin, &format!("{PR}/files"));

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        stub_args(&bin, "herdr").as_deref(),
        Some(&*format!(
            "plugin pane open --plugin loopherd --entrypoint pr-member \
             --env LOOPHERD_PR_MEMBER=lh-1 --env LOOPHERD_PR_URL={PR}\n"
        ))
    );
    assert_eq!(stub_args(&bin, "xdg-open"), None);
    assert_eq!(stub_args(&bin, "open"), None);
}

#[test]
fn a_popup_that_cant_show_its_card_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    ledger_with_lh_1(&tmp.path().join("state"));

    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["pr", "--popup"])
        .env_clear()
        .env("PATH", tmp.path())
        .env("HOME", tmp.path())
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("LOOPHERD_PR_URL", PR)
        .output()
        .unwrap();

    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "loopherd pr: LOOPHERD_PR_MEMBER is not set\n\nq or Esc to close\n"
    );
}
