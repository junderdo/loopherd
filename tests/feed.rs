//! `loopherd feed` through the binary.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;

#[test]
fn feed_is_refused_while_another_run_holds_the_lock() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fs::create_dir(config.path().join("herds")).unwrap();
    fs::write(
        config.path().join("herds/prs.toml"),
        "repo = \"/\"\n[template.pr]\nparams = [\"n\"]\nprompt = \"go\"\n\
         [queue]\ntemplate = \"pr\"\nsource = \"touch /nonexistent-source-ran\"\nmax = 1\n",
    )
    .unwrap();
    fs::create_dir(state.path().join("loopherd")).unwrap();
    let held = File::create(state.path().join("loopherd/feed.lock")).unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["feed", "prs", "--dry-run"])
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_CONFIG_DIR", config.path())
        .env("PATH", "")
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd feed: feed already running\n"
    );
    assert!(out.stdout.is_empty());
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn feed_exits_4_when_malformed_source_lines_are_its_only_errors() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    fs::create_dir(config.path().join("herds")).unwrap();
    fs::write(
        config.path().join("herds/prs.toml"),
        "repo = \"/\"\n[template.pr]\nparams = [\"n\"]\nprompt = \"go\"\n\
         [queue]\ntemplate = \"pr\"\nsource = \"printf '1 2\\\\n3\\\\n'\"\nmax = 5\n",
    )
    .unwrap();
    let shim = bin.path().join("herdr");
    fs::write(
        &shim,
        "#!/bin/sh\necho '{\"result\": {\"workspaces\": [], \"agents\": []}}'\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["feed", "prs", "--dry-run"])
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_CONFIG_DIR", config.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "error: 1 2: template pr takes 1 param (n), got 2\n\
         would add pr-3\n\
         prs: 0 would retire, 1 would add, 0 skipped, 1 error\n",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(4));
}
