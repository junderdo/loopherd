//! `loopherd herd get|set|unset` through the binary.

mod fakes;

use fakes::executable;
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

/// `loopherd herd` with a stub `herdr` first on PATH: one that answers empty
/// agent and workspace lists, or with `up` false, fails every call.
fn herd_with_herdr(config: &Path, up: bool, args: &[&str]) -> Output {
    let bin = config.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let body = match up {
        true => concat!(
            "case \"$1 $2\" in\n",
            "  'agent list') echo '{\"result\": {\"agents\": []}}' ;;\n",
            "  'workspace list') echo '{\"result\": {\"workspaces\": []}}' ;;\n",
            "  *) echo \"unexpected herdr $*\" >&2; exit 1 ;;\n",
            "esac\n"
        ),
        false => "echo 'herdr: server not running' >&2; exit 1\n",
    };
    let shim = bin.join("herdr");
    executable(&shim, format!("#!/bin/sh\n{body}"));
    Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("herd")
        .args(args)
        .env("LOOPHERD_CONFIG_DIR", config)
        .env("XDG_STATE_HOME", config.join("state"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_PANE_ID")
        .output()
        .unwrap()
}

#[test]
fn herd_remove_deletes_a_finished_herd_and_its_ledger_entries() {
    let config = tempfile::tempdir().unwrap();
    let herds = config.path().join("herds");
    fs::create_dir(&herds).unwrap();
    fs::write(herds.join("old.toml"), "repo = \"/r\"\n").unwrap();
    let state = config.path().join("state/loopherd");
    fs::create_dir_all(&state).unwrap();
    let member = |name: &str, herd: &str| {
        serde_json::json!({
            "name": name, "pane_id": "w1:p1", "session_id": null, "repo": "/r",
            "worktree": null, "workspace": "w1", "prompt": null, "prompt_sent": true,
            "created": "2026-11-01T00:00:00Z", "herd": herd, "retired": true,
        })
    };
    let ledger = serde_json::json!({"a": member("a", "old"), "b": member("b", "live")});
    fs::write(state.join("ledger.json"), ledger.to_string()).unwrap();

    let (stdout, stderr, code) = out(&herd_with_herdr(config.path(), false, &["remove", "old"]));
    assert_eq!((stdout.as_str(), code), ("", Some(1)), "{stderr}");
    assert!(
        stderr.starts_with("loopherd herd: refused: can't ask herdr whether a's tab is closed: "),
        "{stderr}"
    );
    assert!(herds.join("old.toml").exists());

    let (stdout, stderr, code) = out(&herd_with_herdr(config.path(), true, &["remove", "old"]));

    assert_eq!((stderr.as_str(), code), ("", Some(0)), "{stdout}");
    assert_eq!(
        stdout,
        format!(
            "removed ledger entry a\nremoved {}\n",
            herds.join("old.toml").display()
        )
    );
    let left: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(state.join("ledger.json")).unwrap()).unwrap();
    let names: Vec<_> = left.as_object().unwrap().keys().cloned().collect();
    assert_eq!(names, ["b"]);

    let (_, stderr, code) = out(&herd_with_herdr(config.path(), true, &["remove", "old"]));
    assert_eq!(code, Some(1));
    assert!(
        stderr.starts_with("loopherd herd: no herd old: "),
        "{stderr}"
    );
}
