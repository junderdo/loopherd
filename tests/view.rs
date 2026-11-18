//! `loopherd view` through the binary on a pty, run as the herdr plugin's
//! popup runs it, with no `HERDR_PANE_ID` (D65). Needs util-linux `script`.
#![cfg(target_os = "linux")]

mod fakes;

use fakes::executable;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const ALT_SCREEN: &[u8] = b"\x1b[?1049h";

/// Starts `loopherd view` under `script`, so it gets a pty, with a failing
/// `herdr` stub, and waits until the view has drawn its screen and made its
/// first herdr call.
fn start_view(home: &Path) -> Child {
    let bin = home.join("bin");
    std::fs::create_dir(&bin).unwrap();
    executable(
        &bin.join("herdr"),
        "#!/bin/sh\ntouch \"$HOME/called\"\nexit 1\n",
    );
    let mut child = Command::new("script")
        .args([
            "-qec",
            &format!("exec '{}' view", env!("CARGO_BIN_EXE_loopherd")),
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
    let called = home.join("called");
    let deadline = Instant::now() + Duration::from_secs(10);
    let drew = shown.recv_timeout(Duration::from_secs(10)).is_ok();
    while drew && !called.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if !(drew && called.exists()) {
        end(&mut child);
        panic!("the view never drew and called herdr");
    }
    let built = Path::new(env!("CARGO_BIN_EXE_loopherd"))
        .canonicalize()
        .unwrap();
    let views = children(&child);
    let exes: Vec<_> = views
        .iter()
        .map(|pid| std::fs::read_link(format!("/proc/{pid}/exe")).ok())
        .collect();
    if exes != [Some(built)] {
        end(&mut child);
        panic!("script's child isn't the view itself: {exes:?}");
    }
    child
}

fn children(script: &Child) -> Vec<i32> {
    let id = script.id();
    std::fs::read_to_string(format!("/proc/{id}/task/{id}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect()
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

/// SIGTERMs the view before `script`, so it restores the terminal and leaves
/// no orphan. SIGKILLs a view that outlives SIGTERM.
fn end(script: &mut Child) {
    let views = children(script);
    let alive = |pid: i32| Path::new(&format!("/proc/{pid}")).exists();
    for &pid in &views {
        // SAFETY: kill takes no pointers.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while views.iter().any(|&pid| alive(pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    for &pid in views.iter().filter(|&&pid| alive(pid)) {
        // SAFETY: kill takes no pointers.
        unsafe { libc::kill(pid, libc::SIGKILL) };
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

#[test]
fn a_hung_up_pty_ends_the_view() {
    let home = tempfile::tempdir().unwrap();
    let mut script = start_view(home.path());
    let views = children(&script);

    // SAFETY: kill takes no pointers.
    unsafe { libc::kill(script.id() as i32, libc::SIGKILL) };
    let _ = script.wait();
    // The orphaned view is reparented, so it can linger as a zombie once gone.
    let running = |pid: i32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while views.iter().any(|&pid| running(pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let left: Vec<_> = views.iter().copied().filter(|&pid| running(pid)).collect();
    for &pid in &left {
        // SAFETY: kill takes no pointers.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    assert_eq!(left, Vec::<i32>::new());
}
