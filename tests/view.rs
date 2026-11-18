//! `loopherd view` through the binary on a pty, run as the herdr plugin's
//! popup runs it: no `HERDR_PANE_ID`, and the socket herdr hands it (D63).

mod fakes;

use fakes::executable;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const ALT_SCREEN: &[u8] = b"\x1b[?1049h";

/// Starts `loopherd view` under `script`, so it gets a pty, with a `herdr`
/// stub that logs the socket it was given, and waits until the view has
/// drawn its screen.
fn start_view(home: &Path) -> Child {
    let bin = home.join("bin");
    std::fs::create_dir(&bin).unwrap();
    executable(
        &bin.join("herdr"),
        "#!/bin/sh\necho \"$HERDR_SOCKET_PATH\" >> \"$HOME/sockets\"\nexit 1\n",
    );
    let mut child = Command::new("script")
        .args([
            "-qec",
            &format!("'{}' view", env!("CARGO_BIN_EXE_loopherd")),
            "/dev/null",
        ])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", home)
        .env("TERM", "xterm-256color")
        .env("XDG_STATE_HOME", home.join("state"))
        .env("LOOPHERD_CONFIG_DIR", home.join("config"))
        .env("HERDR_SOCKET_PATH", home.join("herdr.sock"))
        .env("HERDR_PLUGIN_ENTRYPOINT_ID", "view")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (drawn, shown) = mpsc::channel();
    thread::spawn(move || {
        let mut seen = vec![];
        let mut buf = [0; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(ALT_SCREEN.len()).any(|w| w == ALT_SCREEN) {
                let _ = drawn.send(());
            }
        }
    });
    shown
        .recv_timeout(Duration::from_secs(10))
        .expect("the view never drew");
    child
}

/// Types `keys` into the view: its exit code, or `None` if it was still
/// running after `within`.
fn exit_code_after(child: &mut Child, keys: &[u8], within: Duration) -> Option<i32> {
    child.stdin.as_mut().unwrap().write_all(keys).unwrap();
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Sends SIGTERM to the view itself: ending `script` first hangs up the
/// view's pty, and crossterm then spins on the dead tty instead of exiting.
fn end(script: &mut Child) {
    let id = script.id();
    let children = std::fs::read_to_string(format!("/proc/{id}/task/{id}/children"));
    for pid in children.unwrap_or_default().split_whitespace() {
        // SAFETY: kill takes no pointers.
        unsafe { libc::kill(pid.parse().unwrap(), libc::SIGTERM) };
    }
    let _ = script.wait();
}

fn quits_on(keys: &[u8]) {
    let home = tempfile::tempdir().unwrap();
    let mut view = start_view(home.path());

    let code = exit_code_after(&mut view, keys, Duration::from_secs(5));
    if code.is_none() {
        end(&mut view);
    }

    assert_eq!(code, Some(0));
    let sockets = std::fs::read_to_string(home.path().join("sockets")).unwrap();
    let handed = home.path().join("herdr.sock");
    assert!(sockets.lines().all(|s| Path::new(s) == handed), "{sockets}");
}

#[test]
fn q_ends_the_view_with_no_herdr_pane_id() {
    quits_on(b"q");
}

#[test]
fn escape_ends_the_view_with_no_herdr_pane_id() {
    quits_on(b"\x1b");
}

#[test]
fn other_keys_leave_the_view_running() {
    let home = tempfile::tempdir().unwrap();
    let mut view = start_view(home.path());

    let after_x = exit_code_after(&mut view, b"x", Duration::from_millis(500));
    let after_q = exit_code_after(&mut view, b"q", Duration::from_secs(5));
    if after_q.is_none() {
        end(&mut view);
    }

    assert_eq!((after_x, after_q), (None, Some(0)));
}
