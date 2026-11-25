//! `loopherd file-hook`, the hook a credentials member's settings run before
//! each Read and Grep.

use std::io::Write;
use std::process::{Command, Stdio};

fn file_hook(input: serde_json::Value) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .arg("file-hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.to_string().as_bytes()).unwrap();
    drop(stdin);
    child.wait_with_output().unwrap()
}

#[test]
fn a_read_of_a_processs_environ_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let out = file_hook(serde_json::json!({
        "tool_name": "Read",
        "tool_input": {"file_path": "/proc/self/environ"},
        "cwd": dir.path(),
    }));

    assert!(out.status.success(), "{out:?}");
    let decision: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let decision = &decision["hookSpecificOutput"];
    assert_eq!(decision["permissionDecision"], "deny");
    assert!(
        decision["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("/proc/self/environ")
    );
}

#[test]
fn a_read_outside_proc_prints_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.md"), "hi").unwrap();
    let out = file_hook(serde_json::json!({
        "tool_name": "Read",
        "tool_input": {"file_path": "notes.md"},
        "cwd": dir.path(),
    }));

    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}
