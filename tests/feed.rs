//! `loopherd feed` through the binary.

use std::fs::{self, File};
use std::os::fd::AsRawFd;

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
