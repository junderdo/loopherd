//! `loopherd view --beside-shepherd` against a `herdr` stub on PATH that logs
//! each call and answers it, with a socket standing in for herdr's.
#![cfg(unix)]

mod fakes;

use fakes::executable;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;

/// A stub `herdr` for a stacked view split off `w1:p1`, whose tab the human
/// sees and where they had `w1:p3` focused. Its swap answers `swap_changed`.
fn stub(bin: &Path, swap_changed: bool) {
    let script = format!(
        r#"#!/bin/sh
echo "$*" >> "$HOME/calls"
case "$1 $2" in
  "pane get") echo '{{"result":{{"pane":{{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1"}}}}}}' ;;
  "pane list") echo '{{"result":{{"panes":[]}}}}' ;;
  "workspace list") echo '{{"result":{{"workspaces":[{{"focused":true,"active_tab_id":"w1:t1"}}]}}}}' ;;
  "plugin pane") echo '{{"result":{{"plugin_pane":{{"pane":{{"pane_id":"w1:p2"}}}}}}}}' ;;
  "pane layout") echo '{{"result":{{"layout":{{"focused_pane_id":"w1:p3"}}}}}}' ;;
  "pane swap") echo '{{"result":{{"swap":{{"changed":{swap_changed},"reason":"not_found"}}}}}}' ;;
  *) echo '{{"result":{{}}}}' ;;
esac
"#
    );
    executable(&bin.join("herdr"), script);
}

/// Answers every request on `socket` with an empty success, sending each
/// request line to the returned channel.
fn herdr_socket(socket: &Path) -> mpsc::Receiver<String> {
    let listener = UnixListener::bind(socket).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let _ = writeln!(stream, r#"{{"id":"x","result":{{}}}}"#);
            let _ = tx.send(line);
        }
    });
    rx
}

struct Run {
    stdout: String,
    calls: Vec<String>,
    socket: Vec<String>,
}

fn view_beside_shepherd(swap_changed: bool) -> Run {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    stub(&bin, swap_changed);
    let config = home.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[view]\nlayout = \"stacked\"\n").unwrap();
    let socket = home.path().join("herdr.sock");
    let requests = herdr_socket(&socket);

    let out = Command::new(env!("CARGO_BIN_EXE_loopherd"))
        .args(["view", "--beside-shepherd"])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("LOOPHERD_CONFIG_DIR", &config)
        .env("HERDR_SOCKET_PATH", &socket)
        .env("HERDR_PANE_ID", "w1:p1")
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    let calls = std::fs::read_to_string(home.path().join("calls")).unwrap();
    Run {
        stdout: String::from_utf8(out.stdout).unwrap(),
        calls: calls.lines().map(String::from).collect(),
        socket: requests.try_iter().collect(),
    }
}

fn layout_calls(calls: &[String]) -> Vec<&str> {
    calls
        .iter()
        .map(String::as_str)
        .filter(|c| {
            ["plugin pane open", "pane swap", "pane resize", "pane close"]
                .iter()
                .any(|p| c.starts_with(p))
        })
        .collect()
}

#[test]
fn a_stacked_view_splits_down_swaps_up_shrinks_toward_itself_and_refocuses_by_id() {
    let run = view_beside_shepherd(true);

    let calls = layout_calls(&run.calls);
    assert_eq!(calls.len(), 3, "{calls:#?}");
    assert!(
        calls[0].contains("--target-pane w1:p1 --direction down --no-focus"),
        "{}",
        calls[0]
    );
    assert_eq!(
        calls[1],
        "pane swap --source-pane w1:p1 --target-pane w1:p2"
    );
    assert_eq!(
        calls[2],
        "pane resize --pane w1:p1 --direction up --amount 0.09999999999999998"
    );
    assert_eq!(run.socket.len(), 1, "{:?}", run.socket);
    let focus: serde_json::Value = serde_json::from_str(&run.socket[0]).unwrap();
    assert_eq!(focus["method"], "pane.focus");
    assert_eq!(focus["params"]["pane_id"], "w1:p3");
    assert!(
        run.stdout.starts_with("opened the view beside pane w1:p1"),
        "{}",
        run.stdout
    );
}

#[test]
fn a_swap_herdr_answers_unchanged_closes_the_split_and_says_so_in_the_popup() {
    let run = view_beside_shepherd(false);

    let calls = layout_calls(&run.calls);
    assert_eq!(calls.len(), 4, "{calls:#?}");
    assert_eq!(calls[2], "pane close w1:p2");
    assert!(
        !calls[3].contains("--placement"),
        "a popup takes no placement: {}",
        calls[3]
    );
    assert!(calls[3].contains("not_found"), "{}", calls[3]);
    assert!(
        run.stdout.starts_with("opened the view in the popup:") && run.stdout.contains("not_found"),
        "{}",
        run.stdout
    );
}
