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
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["revive", "--startup"])
        .env_clear()
        .env("PATH", config)
        .env("HOME", config)
        .env("LOOPHERD_CONFIG_DIR", config)
        .env("XDG_STATE_HOME", config.join("state"))
        .env("HERDR_SOCKET_PATH", config.join("herdr.sock"))
        .output()
        .unwrap()
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
