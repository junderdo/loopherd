//! `loopherd herds` through the binary, with a `herdr` on PATH that records any call.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run_herds(config: &Path, bin: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("herds")
        .env("LOOPHERD_CONFIG_DIR", config)
        .env("XDG_STATE_HOME", config.join("state"))
        .env("PATH", bin)
        .output()
        .unwrap()
}

fn recording_herdr(bin: &Path) -> std::path::PathBuf {
    let calls = bin.join("calls");
    let shim = bin.join("herdr");
    fs::write(
        &shim,
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", calls.display()),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    calls
}

#[test]
fn herds_lists_herd_files_flags_invalid_ones_and_never_calls_herdr() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let calls = recording_herdr(bin.path());
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    fs::write(
        herds.join("prs.toml"),
        "repo = \"/r\"\n[template.pr]\nparams = [\"number\"]\nprompt = \"go\"\n",
    )
    .unwrap();

    let ok = run_herds(config.path(), bin.path());
    assert_eq!(
        String::from_utf8_lossy(&ok.stdout),
        "prs  repo /r  workspace (current)  paused no\n  loopherd add prs pr <number>\n"
    );
    assert_eq!(ok.status.code(), Some(0));

    fs::write(herds.join("broken.toml"), "repo = ").unwrap();
    let invalid = run_herds(config.path(), bin.path());
    let stdout = String::from_utf8_lossy(&invalid.stdout);
    assert!(stdout.starts_with("invalid broken.toml: "), "{stdout}");
    assert!(stdout.contains("loopherd add prs pr <number>"), "{stdout}");
    assert_eq!(invalid.status.code(), Some(1));

    fs::remove_file(herds.join("broken.toml")).unwrap();
    fs::write(
        herds.join("typo.toml"),
        "repo = \"/r\"\n[template.t]\nparams = []\nprompt = \"hi {nmae}\"\n",
    )
    .unwrap();
    let typo = run_herds(config.path(), bin.path());
    let stdout = String::from_utf8_lossy(&typo.stdout);
    assert!(
        stdout.ends_with("invalid typo.toml: template t: prompt: unknown placeholder {nmae}\n"),
        "{stdout}"
    );
    assert_eq!(typo.status.code(), Some(1));

    assert!(!calls.exists(), "herdr was called");
}

#[test]
fn herds_without_a_herds_directory_hints_and_exits_0() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let out = run_herds(config.path(), bin.path());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "no herds: add a herd file at {}/<herd>.toml\n",
            config.path().join("herds").display()
        )
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn herds_shows_when_a_herd_was_paused() {
    let config = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    for herd in ["docs", "prs"] {
        fs::write(herds.join(format!("{herd}.toml")), "repo = \"/r\"\n").unwrap();
    }
    let paused = config.path().join("state/loopherd/paused");
    fs::create_dir_all(&paused).unwrap();
    fs::write(
        paused.join("prs"),
        r#"{"since": "2026-11-05T09:00:00Z", "reason": "freeze"}"#,
    )
    .unwrap();

    let out = run_herds(config.path(), bin.path());

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "docs  repo /r  workspace (current)  paused no\n\
         prs  repo /r  workspace (current)  paused since 2026-11-05T09:00:00Z\n"
    );
    assert_eq!(out.status.code(), Some(0));
}
