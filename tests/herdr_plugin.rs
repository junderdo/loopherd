//! The herdr plugin in `herdr-plugin/`: its manifest, the wrapper its
//! commands run loopherd through (D62), and the herd view popup (D63).

mod fakes;

use fakes::executable;
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
    assert_eq!(m["min_herdr_version"].as_str(), Some("0.7.4"));
    assert_eq!(strings(&m["platforms"]), ["linux", "macos"]);
}

/// Every manifest entry's `<kind>.<id>` and command.
fn commands(m: &toml::Table) -> Vec<(String, Vec<&str>)> {
    let mut found = vec![];
    for (key, value) in m {
        let Some(items) = value.as_array() else {
            continue;
        };
        for item in items.iter().filter_map(|i| i.as_table()) {
            if let Some(command) = item.get("command") {
                let id = item.get("id").and_then(|i| i.as_str()).unwrap_or("");
                found.push((format!("{key}.{id}"), strings(command)));
            }
        }
    }
    found
}

#[test]
fn every_manifest_command_runs_a_script_in_the_plugin_through_sh() {
    let m = manifest();
    let commands = commands(&m);
    assert!(!commands.is_empty(), "the manifest declares no commands");
    for (id, command) in commands {
        assert_eq!(command[0], "sh", "{id}");
        assert!(plugin_dir().join(command[1]).is_file(), "{id}: {command:?}");
    }
}

#[test]
fn every_manifest_command_but_opening_the_view_runs_loopherd_through_the_wrapper() {
    let m = manifest();
    for (id, command) in commands(&m) {
        let script = if id == "actions.view" {
            "open-view.sh"
        } else {
            "loopherd.sh"
        };
        assert_eq!(command[1], script, "{id}");
    }
}

fn entry<'a>(m: &'a toml::Table, kind: &str, id: &str) -> &'a toml::Table {
    m[kind]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i.as_table())
        .find(|i| i["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("no [[{kind}]] {id}"))
}

#[test]
fn the_view_pane_is_a_popup_of_most_of_the_screen_running_loopherd_view() {
    let m = manifest();
    let view = entry(&m, "panes", "view");
    assert_eq!(view["placement"].as_str(), Some("popup"));
    assert_eq!(view["width"].as_str(), Some("80%"));
    assert_eq!(view["height"].as_str(), Some("80%"));
    assert_eq!(strings(&view["command"]), ["sh", "loopherd.sh", "view"]);
}

#[test]
fn the_view_action_opens_the_view_pane() {
    let m = manifest();
    let action = entry(&m, "actions", "view");
    assert_eq!(strings(&action["command"]), ["sh", "open-view.sh"]);
    assert_eq!(strings(&action["contexts"]), ["workspace"]);
}

/// Runs `open-view.sh` as herdr runs an action, with `herdr` at
/// `HERDR_BIN_PATH` and nowhere on `PATH`.
#[test]
fn open_view_asks_the_herdr_that_ran_it_to_open_the_view_pane() {
    let tmp = tempfile::tempdir().unwrap();
    let herdr = tmp.path().join("herdr-bin");
    executable(&herdr, "#!/bin/sh\necho \"$*\"\n");

    let out = Command::new("/bin/sh")
        .arg("open-view.sh")
        .current_dir(plugin_dir())
        .env_clear()
        .env("PATH", tmp.path().join("empty"))
        .env("HERDR_BIN_PATH", &herdr)
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "plugin pane open --plugin loopherd --entrypoint view\n"
    );
}

/// Runs the wrapper as herdr does, from the plugin dir, with only `bin` on
/// `PATH` and `home` as `HOME`.
fn wrapper(bin: &Path, home: &Path, args: &[&str]) -> Output {
    wrapper_command(bin, home, args).output().unwrap()
}

fn wrapper_command(bin: &Path, home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("loopherd.sh")
        .args(args)
        .current_dir(plugin_dir())
        .env_clear()
        .env("PATH", bin)
        .env("HOME", home);
    cmd
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

#[test]
fn wrapper_falls_back_to_the_herdr_that_ran_it_when_path_has_none() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    executable(&bin.join("loopherd"), "#!/bin/sh\nherdr\n");
    let herdr_dir = tmp.path().join("herdr dir");
    fs::create_dir(&herdr_dir).unwrap();
    executable(&herdr_dir.join("herdr"), "#!/bin/sh\necho herdr-bin-path\n");

    let out = wrapper_command(&bin, tmp.path(), &["view"])
        .env("HERDR_BIN_PATH", herdr_dir.join("herdr"))
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "herdr-bin-path\n");
}

#[test]
fn wrapper_keeps_paths_order_ahead_of_herdrs_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    executable(&bin.join("herdr"), "#!/bin/sh\necho path-herdr\n");
    executable(&bin.join("loopherd"), "#!/bin/sh\nherdr\n");
    let herdr_dir = tmp.path().join("herdr dir");
    fs::create_dir(&herdr_dir).unwrap();
    executable(&herdr_dir.join("herdr"), "#!/bin/sh\necho herdr-bin-path\n");

    let out = wrapper_command(&bin, tmp.path(), &["view"])
        .env("HERDR_BIN_PATH", herdr_dir.join("herdr"))
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "path-herdr\n");
}

#[test]
fn wrapper_finds_loopherd_on_path_before_herdrs_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fake_loopherd(&bin, "on-path");
    let herdr_dir = tmp.path().join("herdr-dir");
    fake_loopherd(&herdr_dir, "beside-herdr");

    let out = wrapper_command(&bin, tmp.path(), &["view"])
        .env("HERDR_BIN_PATH", herdr_dir.join("herdr"))
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "on-path view\n");
}
