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

/// What `scripts/herdr-dev` gives its command as `LOOPHERD_CONFIG_DIR`, run
/// with `env` set and the variables in `unset` removed.
fn config_dir_seen(env: &[(&str, &str)], unset: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let source = dir.path().join("herdr.source");
    fs::write(&source, HERDR_STUB).unwrap();
    // Copied in a child: a file this process held open for writing could leak
    // into a sibling test's fork and make running it fail with ETXTBSY.
    let copied = Command::new("/bin/cp")
        .arg(&source)
        .arg(bin.join("herdr"))
        .status()
        .unwrap();
    assert!(copied.success());
    fs::set_permissions(bin.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = Command::new(format!("{REPO}/scripts/herdr-dev"));
    cmd.args(["sh", "-c", "printf %s \"$LOOPHERD_CONFIG_DIR\""])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", dir.path())
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("LOOPHERD_CONFIG_DIR");
    for name in unset {
        cmd.env_remove(name);
    }
    cmd.envs(env.iter().copied());
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn a_run_reads_loopherds_config_from_the_users_runtime_dir() {
    assert_eq!(
        config_dir_seen(&[("XDG_RUNTIME_DIR", "/run/user/7")], &[]),
        "/run/user/7/loopherd-dev/config"
    );
}

#[test]
fn without_a_runtime_dir_a_run_reads_a_fresh_private_temp_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let tmpdir = tmp.path().to_str().unwrap();
    let env = [("TMPDIR", tmpdir)];

    let first = config_dir_seen(&env, &["XDG_RUNTIME_DIR"]);
    let second = config_dir_seen(&env, &["XDG_RUNTIME_DIR"]);

    assert!(first.starts_with(tmpdir), "{first}");
    assert_ne!(first, second);
    let mode = fs::metadata(&first).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "{mode:o}");
}

#[test]
fn a_config_dir_the_caller_set_is_kept() {
    assert_eq!(
        config_dir_seen(
            &[
                ("LOOPHERD_CONFIG_DIR", "/elsewhere"),
                ("XDG_RUNTIME_DIR", "/run/user/7")
            ],
            &[]
        ),
        "/elsewhere"
    );
}
