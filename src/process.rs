//! Claude processes, read from `/proc`. A hook's `$CLAUDE_PID` names the
//! Claude that fired it; the ledger remembers which one is the member's.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ledger::{AgentRecord, Ledger, Process};

pub trait Procs {
    fn exe(&self, pid: u32) -> Option<PathBuf>;
    fn parent(&self, pid: u32) -> Option<u32>;
    /// Clock ticks since boot at which `pid` started.
    fn start(&self, pid: u32) -> Option<u64>;
}

pub struct ProcFs;

impl Procs for ProcFs {
    fn exe(&self, pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    fn parent(&self, pid: u32) -> Option<u32> {
        stat_field(pid, 1)?.parse().ok()
    }

    fn start(&self, pid: u32) -> Option<u64> {
        stat_field(pid, 19)?.parse().ok()
    }
}

/// Field `index` after the state in `/proc/<pid>/stat`; the command name
/// before it may hold spaces and parentheses.
fn stat_field(pid: u32, index: usize) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(index).map(str::to_string)
}

/// `$CLAUDE_PID`, when set to a PID.
pub fn claude_pid_from_env() -> Option<u32> {
    std::env::var("CLAUDE_PID").ok()?.parse().ok()
}

/// The member's recorded process, if it is still that process.
pub fn live(procs: &dyn Procs, recorded: Option<Process>) -> Option<Process> {
    let recorded = recorded?;
    (procs.start(recorded.pid) == Some(recorded.start)).then_some(recorded)
}

/// `pid` is the Claude of a pane, not one nested under it: walking up from its
/// parent to the herdr server meets no other Claude binary.
pub fn is_top_level(procs: &dyn Procs, pid: u32) -> bool {
    let mut at = procs.parent(pid);
    for _ in 0..64 {
        let Some(current) = at.filter(|p| *p > 1) else {
            return true;
        };
        match procs.exe(current) {
            Some(exe) if is_herdr(&exe) => return true,
            Some(exe) if is_claude_binary(&exe) => return false,
            _ => {}
        }
        at = procs.parent(current);
    }
    true
}

/// Records `pid` as `member`'s Claude when it is the pane's top-level Claude
/// in the member's own session (or the member has none yet), and the member has
/// no live process. `socket_exact` is whether the pane matched on its socket.
/// Writes only the process (and the session, if the member had none).
pub fn claim(
    ledger: &mut dyn Ledger,
    procs: &dyn Procs,
    member: &AgentRecord,
    socket_exact: bool,
    pid: u32,
    session: Option<&str>,
) -> Result<bool> {
    let session_agrees = match (&member.session_id, session) {
        (Some(ours), Some(theirs)) => ours == theirs,
        (Some(_), None) => false,
        (None, _) => true,
    };
    if !socket_exact
        || !session_agrees
        || live(procs, member.process).is_some()
        || !is_top_level(procs, pid)
    {
        return Ok(false);
    }
    let Some(start) = procs.start(pid) else {
        return Ok(false);
    };
    let mut claimed = false;
    ledger.update(&member.name, &mut |record| {
        claimed = false;
        if live(procs, record.process).is_some() {
            return Ok(());
        }
        record.process = Some(Process { pid, start });
        if record.session_id.is_none() {
            record.session_id = session.map(str::to_string);
        }
        claimed = true;
        Ok(())
    })?;
    Ok(claimed)
}

fn is_herdr(exe: &Path) -> bool {
    exe.file_name().is_some_and(|name| name == "herdr")
}

fn is_claude_binary(exe: &Path) -> bool {
    exe.to_string_lossy().contains("/claude/versions/")
}

#[cfg(test)]
pub use fake::{CLAUDE, FakeProcs};

#[cfg(test)]
mod fake {
    use std::collections::BTreeMap;

    use super::*;

    /// pid → (exe, parent, start).
    #[derive(Default)]
    pub struct FakeProcs(pub BTreeMap<u32, (PathBuf, u32, u64)>);

    pub const CLAUDE: &str = "/home/u/.local/share/claude/versions/2.1.316";

    impl FakeProcs {
        pub fn add(&mut self, pid: u32, exe: &str, parent: u32, start: u64) {
            self.0.insert(pid, (exe.into(), parent, start));
        }

        /// herdr server 10 → pane shell 20 → claude 30.
        pub fn pane() -> Self {
            let mut p = FakeProcs::default();
            p.add(10, "/usr/bin/herdr", 1, 100);
            p.add(20, "/bin/bash", 10, 200);
            p.add(30, CLAUDE, 20, 300);
            p
        }
    }

    impl Procs for FakeProcs {
        fn exe(&self, pid: u32) -> Option<PathBuf> {
            self.0.get(&pid).map(|p| p.0.clone())
        }
        fn parent(&self, pid: u32) -> Option<u32> {
            self.0.get(&pid).map(|p| p.1)
        }
        fn start(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).map(|p| p.2)
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn the_real_proc_reads_this_process() {
        let me = std::process::id();
        assert!(ProcFs.start(me).is_some());
        assert_eq!(ProcFs.parent(me), Some(std::os::unix::process::parent_id()));
        assert!(ProcFs.exe(me).is_some());
    }

    #[test]
    fn a_claude_under_the_pane_shell_is_top_level() {
        assert!(is_top_level(&FakeProcs::pane(), 30));
    }

    #[test]
    fn a_claude_nested_in_another_claude_is_not() {
        let mut p = FakeProcs::pane();
        p.add(40, "/bin/sh", 30, 400);
        p.add(50, CLAUDE, 40, 500);
        assert!(!is_top_level(&p, 50));
    }

    #[test]
    fn a_claude_above_the_herdr_server_does_not_count() {
        let mut p = FakeProcs::pane();
        p.add(10, "/usr/bin/herdr", 5, 100);
        p.add(5, CLAUDE, 1, 50);
        assert!(is_top_level(&p, 30));
    }

    #[test]
    fn a_recorded_process_is_live_only_with_its_start_time() {
        let p = FakeProcs::pane();
        let at = |pid, start| Some(Process { pid, start });
        assert!(live(&p, at(30, 300)).is_some());
        assert!(live(&p, at(30, 301)).is_none());
        assert!(live(&p, at(31, 300)).is_none());
        assert!(live(&p, None).is_none());
    }
}
