//! `loopherd pause` and `resume` through the binary, with a `herdr` on PATH that records any call.

mod fakes;

use fakes::executable;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new(herds: &[&str]) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config/herds");
        fs::create_dir_all(&config).unwrap();
        for herd in herds {
            fs::write(config.join(format!("{herd}.toml")), "repo = \"/r\"\n").unwrap();
        }
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let shim = bin.join("herdr");
        let calls = dir.path().join("herdr-calls");
        executable(
            &shim,
            format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", calls.display()),
        );
        Setup { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = |p: &str| self.dir.path().join(p);
        Command::new(env!("CARGO_BIN_EXE_loopherd"))
            .args(args)
            .env("XDG_STATE_HOME", path("state"))
            .env("LOOPHERD_CONFIG_DIR", path("config"))
            .env("PATH", format!("{}:/usr/bin:/bin", path("bin").display()))
            .env_remove("HERDR_PANE_ID")
            .output()
            .unwrap()
    }

    fn pause_file(&self, herd: &str) -> std::path::PathBuf {
        self.dir.path().join("state/loopherd/paused").join(herd)
    }

    fn herdr_called(&self) -> bool {
        Path::exists(&self.dir.path().join("herdr-calls"))
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn pause_then_resume_one_herd_without_calling_herdr() {
    let s = Setup::new(&["prs"]);

    let paused = s.run(&["pause", "prs", "--reason", "release freeze"]);
    assert_eq!(paused.status.code(), Some(0), "{paused:?}");
    assert!(stdout(&paused).starts_with("prs now paused since "));
    assert!(stdout(&paused).ends_with(" — release freeze\n"));
    assert!(s.pause_file("prs").is_file());

    let resumed = s.run(&["resume", "prs"]);
    assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
    assert_eq!(
        stdout(&resumed),
        "resumed prs; run `loopherd feed prs` to spawn workers\n"
    );
    assert!(!s.pause_file("prs").exists());

    let again = s.run(&["resume", "prs"]);
    assert_eq!(stdout(&again), "prs is not paused\n");
    assert_eq!(again.status.code(), Some(0));

    assert!(!s.herdr_called(), "herdr was called");
}

#[test]
fn all_acts_on_every_herd_file() {
    let s = Setup::new(&["docs", "prs"]);

    let paused = s.run(&["pause", "--all"]);
    assert_eq!(paused.status.code(), Some(0), "{paused:?}");
    assert_eq!(stdout(&paused).lines().count(), 2);
    assert!(s.pause_file("docs").is_file() && s.pause_file("prs").is_file());

    let resumed = s.run(&["resume", "--all"]);
    assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
    assert!(!s.pause_file("docs").exists() && !s.pause_file("prs").exists());
}

#[test]
fn a_herd_with_no_herd_file_is_refused() {
    let s = Setup::new(&["prs"]);
    for verb in ["pause", "resume"] {
        let out = s.run(&[verb, "nope"]);
        assert_eq!(out.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.starts_with(&format!("loopherd {verb}: herd: read ")),
            "{stderr}"
        );
    }
}

#[test]
fn a_herd_and_all_are_exclusive_and_one_is_required() {
    let s = Setup::new(&["prs"]);
    for args in [&["pause", "prs", "--all"][..], &["pause"], &["resume"]] {
        let out = s.run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    }
}

#[test]
fn all_prints_every_herd_and_exits_1_when_one_fails() {
    let s = Setup::new(&["docs", "mid", "prs"]);
    fs::create_dir_all(s.pause_file("mid").join("blocker")).unwrap();

    for verb in ["pause", "resume"] {
        let out = s.run(&[verb, "--all"]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let stdout = stdout(&out);
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 3, "{stdout}");
        assert!(lines[0].contains("docs"), "{stdout}");
        assert!(lines[1].starts_with("error: mid: "), "{stdout}");
        assert!(lines[2].contains("prs"), "{stdout}");
    }
    assert!(!s.pause_file("docs").exists() && !s.pause_file("prs").exists());
}

#[test]
fn resume_lifts_the_pause_of_a_herd_whose_herd_file_was_deleted() {
    let s = Setup::new(&["prs"]);
    s.run(&["pause", "prs"]);
    fs::remove_file(s.dir.path().join("config/herds/prs.toml")).unwrap();

    let out = s.run(&["resume", "prs"]);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(!s.pause_file("prs").exists());
}

#[test]
fn a_reason_with_a_newline_is_refused() {
    let s = Setup::new(&["prs"]);

    let out = s.run(&["pause", "prs", "--reason", "freeze\nprs"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "loopherd pause: a reason can't hold control or format characters such as a newline\n"
    );
    assert!(!s.pause_file("prs").exists());
}
