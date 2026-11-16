//! `scripts/herdr-dev`, run against a stub `herdr` that reports the dev server
//! running on the dev socket, so no test reaches a real herdr.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const REPO: &str = env!("CARGO_MANIFEST_DIR");

const HERDR_STUB: &str = r#"#!/usr/bin/env bash
case $1 in
  status) printf '  socket: %s\nstatus: running\n' "$HOME/.config/herdr/sessions/loopherd-dev/herdr.sock" ;;
  workspace) echo '{"result":{"workspaces":[]}}' ;;
esac
"#;

/// What `scripts/herdr-dev` gives its command as `LOOPHERD_CONFIG_DIR`,
/// with `TMPDIR` at `tmp` and the caller's own `config_dir`, if any.
fn config_dir_seen(tmp: &str, config_dir: Option<&str>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let herdr = bin.join("herdr");
    fs::write(&herdr, HERDR_STUB).unwrap();
    fs::set_permissions(&herdr, fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = Command::new(format!("{REPO}/scripts/herdr-dev"));
    cmd.args(["sh", "-c", "printf %s \"$LOOPHERD_CONFIG_DIR\""])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", dir.path())
        .env("TMPDIR", tmp)
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("LOOPHERD_CONFIG_DIR");
    if let Some(config_dir) = config_dir {
        cmd.env("LOOPHERD_CONFIG_DIR", config_dir);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_run_reads_loopherds_config_from_a_temp_dir_unless_the_caller_named_one() {
    assert_eq!(
        config_dir_seen("/tmp/t1", None),
        "/tmp/t1/loopherd-dev/config"
    );
    assert_eq!(config_dir_seen("/tmp/t1", Some("/elsewhere")), "/elsewhere");
}
