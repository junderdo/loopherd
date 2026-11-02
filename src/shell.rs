//! Running a child process with a time limit.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Runs `command` with no stdin in its own process group, capturing its
/// output. Returns `None` if it, or anything holding its output open, outlives
/// `timeout`. Whatever is left of the group is killed either way.
pub fn run_within(command: &mut Command, timeout: Duration) -> Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so a kill also reaches whatever the command spawned.
        .process_group(0)
        .spawn()?;
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());
    let deadline = Instant::now() + timeout;
    while !exited(&child)? {
        if Instant::now() >= deadline {
            kill_group(&child);
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
    // Background children would otherwise hold the pipes open past the exit.
    kill_group(&child);
    let status = child.wait().context("wait")?;
    while !(stdout.is_finished() && stderr.is_finished()) {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
    Ok(Some(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }))
}

/// `value` as one single-quoted shell word.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

const POLL: Duration = Duration::from_millis(20);

/// Whether `child` has exited, leaving it unreaped so its pid, which is also
/// its group's id, can't be reused before the group is killed.
fn exited(child: &Child) -> Result<bool> {
    // SAFETY: waitid writes only into `info`; WNOWAIT leaves the child to be reaped by wait.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, flags) } != 0 {
        return Err(std::io::Error::last_os_error()).context("wait");
    }
    // SAFETY: si_pid is set (or left zeroed) for every waitid result.
    Ok(unsafe { info.si_pid() } != 0)
}

fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut out);
        }
        out
    })
}

fn kill_group(child: &Child) {
    let group = child.id() as libc::pid_t;
    // SAFETY: killpg only sends a signal; the group is the child's own, made by
    // process_group(0), and the unreaped child keeps its id from being reused.
    unsafe { libc::killpg(group, libc::SIGKILL) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> Option<Output> {
        run_within(Command::new("sh").arg("-c").arg(script), timeout).unwrap()
    }

    #[test]
    fn captures_the_status_and_output_of_a_command_that_finishes() {
        let out = sh("echo out; echo err >&2; exit 3", Duration::from_secs(5)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn a_child_left_running_after_the_command_exits_is_killed_and_does_not_hold_the_output() {
        let started = Instant::now();

        let out = sh("echo out; sleep 30 & exit 1", Duration::from_secs(20)).unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(out.stdout, b"out\n");
    }

    #[test]
    fn a_hung_command_and_its_children_are_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let started = Instant::now();

        assert!(sh(&script, Duration::from_millis(300)).is_none());

        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The orphaned sleep is reaped by init a moment after it dies.
        let gone_by = Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only checks whether the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < gone_by, "sleep {pid} outlived the timeout");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
