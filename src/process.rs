//! Claude processes, read from `/proc`, or `proc_pidinfo` and `sysctl` on
//! macOS. A hook's `$CLAUDE_PID` names the Claude that fired it; the ledger
//! remembers which one is the member's.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ledger::{AgentRecord, Ledger, Process};

pub trait Procs {
    fn exe(&self, pid: u32) -> Option<PathBuf>;
    fn parent(&self, pid: u32) -> Option<u32>;
    /// When `pid` started: clock ticks since boot on Linux, microseconds since
    /// the epoch on macOS, so only comparable with another start on this machine.
    fn start(&self, pid: u32) -> Option<u64>;
    fn cmdline(&self, pid: u32) -> Option<Vec<String>>;
    /// `path` as `pid` would open it: relative to its cwd, symlinks followed.
    fn resolve(&self, pid: u32, path: &str) -> Option<PathBuf>;
}

pub struct ProcFs;

impl Procs for ProcFs {
    fn exe(&self, pid: u32) -> Option<PathBuf> {
        os::exe(pid)
    }

    fn parent(&self, pid: u32) -> Option<u32> {
        os::parent(pid)
    }

    fn start(&self, pid: u32) -> Option<u64> {
        os::start(pid)
    }

    fn cmdline(&self, pid: u32) -> Option<Vec<String>> {
        os::cmdline(pid)
    }

    fn resolve(&self, pid: u32, path: &str) -> Option<PathBuf> {
        std::fs::canonicalize(os::cwd(pid)?.join(path)).ok()
    }
}

#[cfg(not(target_os = "macos"))]
mod os {
    use std::path::PathBuf;

    pub fn exe(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    pub fn parent(pid: u32) -> Option<u32> {
        stat_field(pid, 1)?.parse().ok()
    }

    pub fn start(pid: u32) -> Option<u64> {
        stat_field(pid, 19)?.parse().ok()
    }

    pub fn cmdline(pid: u32) -> Option<Vec<String>> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args = raw.split(|b| *b == 0).filter(|a| !a.is_empty());
        Some(
            args.map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
        )
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        Some(PathBuf::from(format!("/proc/{pid}/cwd")))
    }

    /// Field `index` of `/proc/<pid>/stat` counted from the state (0); the command name
    /// before it may hold spaces and parentheses.
    fn stat_field(pid: u32, index: usize) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = &stat[stat.rfind(')')? + 1..];
        after_comm.split_whitespace().nth(index).map(str::to_string)
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::ffi::{OsStr, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    pub fn exe(pid: u32) -> Option<PathBuf> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = unsafe {
            libc::proc_pidpath(
                pid as c_int,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u32,
            )
        };
        let len = usize::try_from(len).ok().filter(|n| *n > 0)?;
        Some(OsStr::from_bytes(&buf[..len]).into())
    }

    pub fn parent(pid: u32) -> Option<u32> {
        bsd_info(pid).map(|info| info.pbi_ppid)
    }

    pub fn start(pid: u32) -> Option<u64> {
        bsd_info(pid).map(|info| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    pub fn cmdline(pid: u32) -> Option<Vec<String>> {
        let mut max: c_int = 0;
        let mut len = std::mem::size_of::<c_int>();
        sysctl(
            &mut [libc::CTL_KERN, libc::KERN_ARGMAX],
            (&raw mut max).cast(),
            &mut len,
        )?;
        let mut buf = vec![0u8; usize::try_from(max).ok()?];
        let mut len = buf.len();
        sysctl(
            &mut [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as c_int],
            buf.as_mut_ptr().cast(),
            &mut len,
        )?;
        super::parse_procargs2(&buf[..len])
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        let info: libc::proc_vnodepathinfo = pid_info(pid, libc::PROC_PIDVNODEPATHINFO)?;
        let path = info.pvi_cdir.vip_path.as_flattened();
        // SAFETY: c_char and u8 have the same size and alignment.
        let path: &[u8] = unsafe { &*(std::ptr::from_ref(path) as *const [u8]) };
        let end = path.iter().position(|b| *b == 0)?;
        (end > 0).then(|| OsStr::from_bytes(&path[..end]).into())
    }

    fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
        pid_info(pid, libc::PROC_PIDTBSDINFO)
    }

    /// `T` must be the struct that `flavor` fills.
    fn pid_info<T>(pid: u32, flavor: c_int) -> Option<T> {
        let mut info: T = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<T>() as c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                pid as c_int,
                flavor,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        (read == size).then_some(info)
    }

    fn sysctl(mib: &mut [c_int], out: *mut c_void, len: &mut usize) -> Option<()> {
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                out,
                len,
                std::ptr::null_mut(),
                0,
            )
        };
        (status == 0).then_some(())
    }
}

/// argv from a `KERN_PROCARGS2` buffer: `argc` as a native `int`, the exec
/// path, NUL padding, then `argc` NUL-terminated arguments and the environment.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    let (argc, rest) = buf.split_first_chunk::<4>()?;
    let argc = usize::try_from(i32::from_ne_bytes(*argc)).ok()?;
    let after_exec = &rest[rest.iter().position(|b| *b == 0)?..];
    let args = &after_exec[after_exec.iter().position(|b| *b != 0)?..];
    let args: Vec<String> = args
        .split_inclusive(|b| *b == 0)
        .map_while(|a| a.strip_suffix(&[0]))
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (args.len() == argc).then(|| args.into_iter().filter(|a| !a.is_empty()).collect())
}

/// `$CLAUDE_PID` as the hook sees it: only a PID that is an ancestor of this
/// process counts, since an environment can pass down another Claude's.
pub fn claude_pid_from_env(procs: &dyn Procs) -> Option<u32> {
    claude_pid(
        procs,
        std::env::var("CLAUDE_PID").ok().as_deref(),
        std::process::id(),
    )
}

pub fn claude_pid(procs: &dyn Procs, env: Option<&str>, me: u32) -> Option<u32> {
    let pid: u32 = env?.parse().ok()?;
    is_ancestor(procs, pid, me).then_some(pid)
}

/// `pid` is a parent of `me`, or a parent of one, up to 64 steps.
pub fn is_ancestor(procs: &dyn Procs, pid: u32, me: u32) -> bool {
    let mut at = procs.parent(me);
    for _ in 0..64 {
        let Some(current) = at.filter(|p| *p > 1) else {
            return false;
        };
        if current == pid {
            return true;
        }
        at = procs.parent(current);
    }
    false
}

/// `me` and each of its ancestors, nearest first, up to 64 steps.
pub fn lineage(procs: &dyn Procs, me: u32) -> Vec<Process> {
    let mut lineage = vec![];
    let mut at = Some(me);
    while let Some(pid) = at.filter(|p| *p > 1 && lineage.len() < 64) {
        let Some(start) = procs.start(pid) else { break };
        lineage.push(Process { pid, start });
        at = procs.parent(pid);
    }
    lineage
}

/// The member's recorded process, if it is still that process.
pub fn live(procs: &dyn Procs, recorded: Option<Process>) -> Option<Process> {
    let recorded = recorded?;
    (procs.start(recorded.pid) == Some(recorded.start)).then_some(recorded)
}

/// `pid` is the Claude of a pane, not one nested under it: walking up from its
/// parent reaches the herdr server without meeting another Claude. A walk
/// that ends any other way (init, a vanished process, too many steps) is not.
pub fn is_top_level(procs: &dyn Procs, pid: u32) -> bool {
    let mut at = procs.parent(pid);
    for _ in 0..64 {
        let Some(current) = at.filter(|p| *p > 1) else {
            return false;
        };
        match procs.exe(current) {
            Some(exe) if is_herdr(&exe) => return true,
            Some(exe) if is_claude(procs, current, &exe) => return false,
            Some(_) => {}
            None => return false,
        }
        at = procs.parent(current);
    }
    false
}

#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    Claimed,
    /// `pid` is a Claude nested under another, or not under the herdr server.
    Nested,
    Declined,
}

/// Records `pid` as `member`'s Claude when it is the pane's top-level Claude
/// in the member's own session (or the member has none yet), and the member has
/// no live process; a `pid` that isn't top-level is `Nested`. `socket_exact` is whether the pane matched on its socket.
/// Writes only the process (and the session, if the member had none).
pub fn claim(
    ledger: &mut dyn Ledger,
    procs: &dyn Procs,
    member: &AgentRecord,
    socket_exact: bool,
    pid: u32,
    session: Option<&str>,
) -> Result<Claim> {
    let session_agrees = match (&member.session_id, session) {
        (Some(ours), Some(theirs)) => ours == theirs,
        (Some(_), None) => false,
        (None, _) => true,
    };
    if live(procs, member.process).is_some() {
        return Ok(Claim::Declined);
    }
    if !is_top_level(procs, pid) {
        return Ok(Claim::Nested);
    }
    let Some(start) = procs.start(pid).filter(|_| socket_exact && session_agrees) else {
        return Ok(Claim::Declined);
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
    Ok(if claimed {
        Claim::Claimed
    } else {
        Claim::Declined
    })
}

pub fn is_herdr(exe: &Path) -> bool {
    let name = exe.file_name().and_then(|n| n.to_str());
    name.is_some_and(|n| n.strip_suffix(" (deleted)").unwrap_or(n) == "herdr")
}

const CLAUDE_CODE_PACKAGE: &str = "@anthropic-ai/claude-code/";

/// A native install runs from `…/claude/versions/`; an npm install is `node`
/// running the package's `cli.js`, often through npm's `bin/claude` symlink.
fn is_claude(procs: &dyn Procs, pid: u32, exe: &Path) -> bool {
    if exe.to_string_lossy().contains("/claude/versions/") {
        return true;
    }
    let is_node = exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("node"));
    if !is_node {
        return false;
    }
    let Some(args) = procs.cmdline(pid) else {
        return false;
    };
    if args.iter().any(|a| a.contains(CLAUDE_CODE_PACKAGE)) {
        return true;
    }
    // A shebang launch puts the invoked path, not the symlink's target, in argv.
    let script = args.iter().skip(1).find(|a| !a.starts_with('-'));
    script
        .and_then(|s| procs.resolve(pid, s))
        .is_some_and(|r| r.to_string_lossy().contains(CLAUDE_CODE_PACKAGE))
}

#[cfg(test)]
pub use fake::{CLAUDE, FakeProcs, NODE, NPM_CLI, PANE_SHELL};

#[cfg(test)]
mod fake {
    use std::collections::BTreeMap;

    use super::*;

    /// pid → (exe, parent, start), pid → cmdline, and path → resolved path.
    #[derive(Default)]
    pub struct FakeProcs(
        pub BTreeMap<u32, (PathBuf, u32, u64)>,
        pub BTreeMap<u32, Vec<String>>,
        pub BTreeMap<String, PathBuf>,
    );

    /// The shell of [`FakeProcs::pane`].
    pub const PANE_SHELL: Process = Process {
        pid: 20,
        start: 200,
    };

    pub const CLAUDE: &str = "/home/u/.local/share/claude/versions/2.1.316";
    pub const NODE: &str = "/usr/bin/node";
    pub const NPM_CLI: &str = "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js";

    impl FakeProcs {
        pub fn add(&mut self, pid: u32, exe: &str, parent: u32, start: u64) {
            self.0.insert(pid, (exe.into(), parent, start));
        }

        pub fn set_cmdline(&mut self, pid: u32, args: &[&str]) {
            self.1
                .insert(pid, args.iter().map(|a| a.to_string()).collect());
        }

        pub fn link(&mut self, path: &str, target: &str) {
            self.2.insert(path.into(), target.into());
        }

        pub fn add_npm_claude(&mut self, pid: u32, parent: u32, start: u64) {
            self.add(pid, NODE, parent, start);
            self.set_cmdline(pid, &["node", NPM_CLI, "--permission-mode", "auto"]);
        }

        /// herdr server 10 → pane shell 20 → claude 30.
        pub fn pane() -> Self {
            let mut p = FakeProcs::default();
            p.add(10, "/usr/bin/herdr", 1, 100);
            p.add(20, "/bin/bash", 10, 200);
            p.add(30, CLAUDE, 20, 300);
            p
        }

        /// [`FakeProcs::pane`] after a herdr restart: the same pids, started anew.
        pub fn restarted_pane() -> Self {
            let mut p = FakeProcs::default();
            p.add(10, "/usr/bin/herdr", 1, 900);
            p.add(20, "/bin/bash", 10, 910);
            p.add(30, CLAUDE, 20, 920);
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
        fn cmdline(&self, pid: u32) -> Option<Vec<String>> {
            self.1.get(&pid).cloned()
        }
        fn resolve(&self, _pid: u32, path: &str) -> Option<PathBuf> {
            self.2.get(path).cloned()
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
        assert!(ProcFs.cmdline(me).is_some_and(|args| !args.is_empty()));
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(ProcFs.resolve(me, "."), Some(cwd));
        assert_eq!(ProcFs.resolve(me, "/no/such/path"), None);
    }

    fn procargs2(argc: i32, exec: &str, rest: &[&str]) -> Vec<u8> {
        let mut buf = argc.to_ne_bytes().to_vec();
        buf.extend_from_slice(exec.as_bytes());
        buf.extend_from_slice(&[0; 5]);
        for arg in rest {
            buf.extend_from_slice(arg.as_bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn procargs2_gives_argv_without_the_exec_path_or_environment() {
        let buf = procargs2(
            3,
            NODE,
            &["node", NPM_CLI, "--resume", "HOME=/Users/u", "TERM=xterm"],
        );
        assert_eq!(
            parse_procargs2(&buf),
            Some(vec!["node".into(), NPM_CLI.into(), "--resume".into()])
        );
    }

    #[test]
    fn procargs2_drops_empty_args_as_proc_cmdline_does() {
        let buf = procargs2(3, NODE, &["claude", "", ""]);
        assert_eq!(parse_procargs2(&buf), Some(vec!["claude".into()]));
    }

    #[test]
    fn a_truncated_procargs2_is_unreadable() {
        assert_eq!(
            parse_procargs2(&procargs2(3, NODE, &["node", NPM_CLI])),
            None
        );
        assert_eq!(parse_procargs2(&procargs2(1, NODE, &[])), None);
        assert_eq!(parse_procargs2(&[1, 0]), None);
        assert_eq!(parse_procargs2(&procargs2(-1, NODE, &["node"])), None);
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
    fn an_npm_claude_under_the_pane_shell_is_top_level() {
        let mut p = FakeProcs::pane();
        p.add_npm_claude(30, 20, 300);
        assert!(is_top_level(&p, 30));
    }

    #[test]
    fn an_npm_claude_nested_in_an_npm_claude_is_not() {
        let mut p = FakeProcs::pane();
        p.add_npm_claude(30, 20, 300);
        p.add(40, "/bin/sh", 30, 400);
        p.add_npm_claude(50, 40, 500);
        assert!(!is_top_level(&p, 50));
    }

    #[test]
    fn claudes_nested_across_install_kinds_are_not_top_level() {
        let mut native_in_npm = FakeProcs::pane();
        native_in_npm.add_npm_claude(30, 20, 300);
        native_in_npm.add(40, "/bin/sh", 30, 400);
        native_in_npm.add(50, CLAUDE, 40, 500);
        assert!(!is_top_level(&native_in_npm, 50));

        let mut npm_in_native = FakeProcs::pane();
        npm_in_native.add(40, "/bin/sh", 30, 400);
        npm_in_native.add_npm_claude(50, 40, 500);
        assert!(!is_top_level(&npm_in_native, 50));
    }

    #[test]
    fn a_plain_node_process_above_the_claude_does_not_nest_it() {
        let mut p = FakeProcs::pane();
        p.add(25, NODE, 20, 250);
        p.set_cmdline(25, &[NODE, "/home/u/tools/watcher.js"]);
        p.add(30, CLAUDE, 25, 300);
        assert!(is_top_level(&p, 30));

        p.0.remove(&25);
        p.add(25, "/usr/bin/nodejs", 20, 250);
        assert!(is_top_level(&p, 30));
    }

    /// npm's `bin/claude` is a symlink to the package's `cli.js`; a shebang
    /// launch puts the invoked path in argv.
    fn with_npm_bin_claude(p: &mut FakeProcs, pid: u32, parent: u32) {
        p.add(pid, NODE, parent, pid as u64 * 10);
        p.set_cmdline(pid, &["node", "/usr/local/bin/claude", "--resume", "x"]);
        p.link("/usr/local/bin/claude", NPM_CLI);
    }

    #[test]
    fn an_npm_claude_launched_through_its_bin_link_is_recognised() {
        let mut p = FakeProcs::pane();
        with_npm_bin_claude(&mut p, 30, 20);
        assert!(is_top_level(&p, 30));
        p.add(40, "/bin/sh", 30, 400);
        p.add(50, CLAUDE, 40, 500);
        assert!(!is_top_level(&p, 50));
    }

    #[test]
    fn a_node_script_resolving_outside_claude_code_is_not_claude() {
        let mut p = FakeProcs::pane();
        p.add(25, NODE, 20, 250);
        p.set_cmdline(25, &[NODE, "--inspect", "/usr/local/bin/tool"]);
        p.link("/usr/local/bin/tool", "/opt/tool/index.js");
        p.add(30, CLAUDE, 25, 300);
        assert!(is_top_level(&p, 30));
    }

    #[test]
    fn a_node_script_that_does_not_resolve_is_not_claude() {
        let mut p = FakeProcs::pane();
        p.add(25, NODE, 20, 250);
        p.set_cmdline(25, &[NODE, "/usr/local/bin/claude"]);
        p.add(30, CLAUDE, 25, 300);
        assert!(is_top_level(&p, 30));
    }

    #[test]
    fn a_herdr_binary_replaced_while_running_is_still_herdr() {
        let mut p = FakeProcs::pane();
        p.add(10, "/usr/bin/herdr (deleted)", 5, 100);
        p.add(5, CLAUDE, 1, 50);
        assert!(is_top_level(&p, 30));
    }

    #[test]
    fn a_walk_that_never_reaches_herdr_is_not_top_level() {
        let mut orphan = FakeProcs::default();
        orphan.add(50, CLAUDE, 1, 500);
        assert!(!is_top_level(&orphan, 50));

        let mut vanished = FakeProcs::pane();
        vanished.add(30, CLAUDE, 25, 300);
        assert!(!is_top_level(&vanished, 30));

        let mut no_exe = FakeProcs::pane();
        no_exe.0.remove(&20);
        no_exe.add(30, CLAUDE, 20, 300);
        assert!(!is_top_level(&no_exe, 30));
    }

    #[test]
    fn claude_pid_counts_only_as_an_ancestor_of_the_hook() {
        let mut p = FakeProcs::pane();
        p.add(35, "/bin/sh", 30, 350);
        p.add(99, CLAUDE, 1, 900);
        assert_eq!(claude_pid(&p, Some("30"), 35), Some(30));
        assert_eq!(claude_pid(&p, Some("99"), 35), None);
        assert_eq!(claude_pid(&p, Some("x"), 35), None);
        assert_eq!(claude_pid(&p, None, 35), None);
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
