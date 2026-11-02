//! `loopherd feed` through the binary.

use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::Path;

fn write_herd(config: &Path, source: &str) {
    fs::create_dir(config.join("herds")).unwrap();
    fs::write(
        config.join("herds/prs.toml"),
        format!(
            "repo = \"/\"\n[template.pr]\nparams = [\"n\"]\nname = \"zpr-{{n}}\"\nprompt = \"go\"\n\
             [queue]\ntemplate = \"pr\"\nsource = {source:?}\nmax = 1\n"
        ),
    )
    .unwrap();
}

#[test]
fn dry_run_needs_no_herdr_and_leaves_state_untouched() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    write_herd(config.path(), "echo 1");
    // Only sh is on PATH, so any herdr call would fail.
    std::os::unix::fs::symlink("/bin/sh", bin.path().join("sh")).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["feed", "prs", "--dry-run"])
        .env("XDG_STATE_HOME", state.path())
        .env("LOOPHERD_CONFIG_DIR", config.path())
        .env("PATH", bin.path())
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_WORKSPACE_ID")
        .output()
        .unwrap();

    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "would add zpr-1\nprs: 0 would retire, 1 would add, 0 skipped, 0 errors\n"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(fs::read_dir(state.path()).unwrap().count(), 0);
}

#[test]
fn dry_run_proceeds_while_a_real_run_holds_the_lock() {
    let state = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    write_herd(config.path(), "echo 1");
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
        .output()
        .unwrap();

    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(out.status.code(), Some(0));
}

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
        .args(["feed", "prs"])
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
