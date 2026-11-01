//! `loopherd herds` through the binary, with a `herdr` on PATH that records any call.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run_herds(config: &Path, bin: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("herds")
        .env("LOOPHERD_CONFIG_DIR", config)
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
        "prs  repo /r  workspace (current)\n  loopherd add prs pr <number>\n"
    );
    assert_eq!(ok.status.code(), Some(0));

    fs::write(herds.join("broken.toml"), "repo = ").unwrap();
    let invalid = run_herds(config.path(), bin.path());
    let stdout = String::from_utf8_lossy(&invalid.stdout);
    assert!(stdout.starts_with("invalid broken.toml: "), "{stdout}");
    assert!(stdout.contains("loopherd add prs pr <number>"), "{stdout}");
    assert_eq!(invalid.status.code(), Some(1));

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
