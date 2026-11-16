//! `loopherd config get|set|unset` through the binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn config(dir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("config")
        .args(args)
        .env("LOOPHERD_CONFIG_DIR", dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&output.stdout).into(),
        String::from_utf8_lossy(&output.stderr).into(),
        output.status.code(),
    )
}

#[test]
fn config_gets_sets_and_unsets_one_value() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    fs::write(&file, "# mine\n[notify]\nenabled = false\n").unwrap();
    let ok = |stdout: &str| (stdout.to_string(), String::new(), Some(0));

    assert_eq!(
        config(dir.path(), &["set", "shepherd.context_limit", "400000"]),
        ok("set shepherd.context_limit = 400000\n")
    );
    assert_eq!(
        config(dir.path(), &["get", "shepherd.context_limit"]),
        ok("400000\n")
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "# mine\n[notify]\nenabled = false\n\n[shepherd]\ncontext_limit = 400000\n"
    );
    assert_eq!(
        config(dir.path(), &["unset", "shepherd.context_limit"]),
        ok("unset shepherd.context_limit\n")
    );
    assert_eq!(
        config(dir.path(), &["unset", "shepherd.context_limit"]),
        ok("shepherd.context_limit was not set\n")
    );
    let (stdout, stderr, code) = config(dir.path(), &["get", "shepherd.context_limit"]);
    assert_eq!((stdout.as_str(), code), ("", Some(1)), "{stderr}");
    assert!(stderr.contains("not set"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "# mine\n[notify]\nenabled = false\n"
    );
}

#[test]
fn config_set_refuses_an_unknown_key_or_a_too_small_value() {
    let dir = tempfile::tempdir().unwrap();

    for (args, says) in [
        (["set", "max", "3"], "unknown key"),
        (["set", "context_window", "5"], "at least 10000"),
    ] {
        let (_, stderr, code) = config(dir.path(), &args);
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stderr.contains(says), "{stderr}");
    }
    assert!(!dir.path().join("config.toml").exists());
}
