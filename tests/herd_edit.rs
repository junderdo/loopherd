//! `loopherd herd get|set|unset` through the binary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn herd(config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("herd")
        .args(args)
        .env("LOOPHERD_CONFIG_DIR", config)
        .env("XDG_STATE_HOME", config.join("state"))
        .output()
        .unwrap()
}

fn out(output: &Output) -> (String, String, Option<i32>) {
    (
        String::from_utf8_lossy(&output.stdout).into(),
        String::from_utf8_lossy(&output.stderr).into(),
        output.status.code(),
    )
}

#[test]
fn herd_gets_sets_and_unsets_one_value() {
    let config = tempfile::tempdir().unwrap();
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    let file = herds.join("h.toml");
    fs::write(&file, "repo = \"/r\" # where\n").unwrap();

    let ok = |stdout: &str| (stdout.to_string(), String::new(), Some(0));
    assert_eq!(
        out(&herd(config.path(), &["get", "h", "repo"])),
        ok("\"/r\"\n")
    );
    assert_eq!(
        out(&herd(config.path(), &["set", "h", "lead.prompt", "\"go\""])),
        ok("set lead.prompt = \"go\" in h\n")
    );
    assert_eq!(
        out(&herd(config.path(), &["set", "h", "lead.merge", "true"])),
        ok("set lead.merge = true in h\n")
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "repo = \"/r\" # where\n\n[lead]\nprompt = \"go\"\nmerge = true\n"
    );
    assert_eq!(
        out(&herd(config.path(), &["unset", "h", "lead.merge"])),
        ok("unset lead.merge in h\n")
    );
    assert_eq!(
        out(&herd(config.path(), &["unset", "h", "lead.merge"])),
        ok("lead.merge was not set in h\n")
    );
    assert_eq!(
        out(&herd(config.path(), &["get", "h", "lead.merge"])),
        (
            String::new(),
            "loopherd herd: lead.merge is not set in h\n".into(),
            Some(1)
        )
    );

    let (stdout, stderr, code) = out(&herd(config.path(), &["set", "h", "lead.prompt", "1"]));
    assert_eq!((stdout.as_str(), code), ("", Some(1)));
    assert!(stderr.starts_with("loopherd herd: refused, "), "{stderr}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "repo = \"/r\" # where\n\n[lead]\nprompt = \"go\"\n"
    );
    let mut left: Vec<_> = fs::read_dir(&herds)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, [".h.toml.lock", "h.toml"], "a temp file was left");
}
