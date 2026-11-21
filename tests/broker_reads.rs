//! A lead's `status` and `read` through its broker, and the shepherd's `read`,
//! with a stub `herdr` on PATH that logs each call. The lead's own PATH has
//! no `herdr`, so only its broker can reach one.

mod brokers;
mod fakes;

use brokers::Broker;
use fakes::executable;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SOCKET: &str = "/run/herdr-test.sock";

struct World {
    root: tempfile::TempDir,
    _broker: Broker,
}

impl World {
    /// lead-k leads herd k from w1:p1 with lh-1 in w1:p2; lh-9 works in herd g.
    fn new() -> World {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state/loopherd");
        fs::create_dir_all(&state).unwrap();
        let entry = |name: &str, pane: &str, herd: &str, role: &str| {
            format!(
                r#""{name}": {{"name": "{name}", "pane_id": "{pane}", "session_id": null,
                "repo": "/", "worktree": null, "workspace": "w1", "prompt": null,
                "prompt_sent": true, "created": "2026-11-01T00:00:00Z",
                "socket": "{SOCKET}", "herd": "{herd}", "role": {role}}}"#
            )
        };
        let ledger = [
            entry("lead-k", "w1:p1", "k", r#""lead""#),
            entry("lh-1", "w1:p2", "k", "null"),
            entry("lh-9", "w1:p3", "g", "null"),
        ];
        fs::write(
            state.join("ledger.json"),
            format!("{{{}}}", ledger.join(", ")),
        )
        .unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let agent = |name: &str, pane: &str| {
            format!(
                r#"{{"name": "{name}", "pane_id": "{pane}", "tab_id": "w1:t1", "agent_status": "idle"}}"#
            )
        };
        let agents = [
            agent("lead-k", "w1:p1"),
            agent("lh-1", "w1:p2"),
            agent("lh-9", "w1:p3"),
        ];
        executable(
            &bin.join("herdr"),
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\ncase \"$1 $2\" in\n\
                 'agent list') echo '{{\"result\": {{\"agents\": [{}]}}}}' ;;\n\
                 'agent read') printf 'one\\ntwo\\n' ;;\nesac\n",
                root.path().join("calls").display(),
                agents.join(", ")
            ),
        );
        let mut loopherd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        set_env(&mut loopherd, root.path(), Some(&bin));
        let broker = Broker::start(loopherd, &root.path().join("config"), "lead-k", "k");
        World {
            root,
            _broker: broker,
        }
    }

    /// Runs loopherd with `args` in `pane`, or outside any pane, where only the
    /// shepherd's runs see the stub herdr.
    fn run(&self, pane: Option<&str>, args: &[&str]) -> Output {
        self.loopherd(pane, args).output().unwrap()
    }

    fn loopherd(&self, pane: Option<&str>, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        let bin = self.root.path().join("bin");
        set_env(&mut cmd, self.root.path(), pane.is_none().then_some(&bin));
        match pane {
            Some(pane) => cmd.env("HERDR_PANE_ID", pane),
            None => cmd.env_remove("HERDR_PANE_ID"),
        };
        cmd.args(args);
        cmd
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.root.path().join("calls")).unwrap_or_default()
    }
}

fn set_env(cmd: &mut Command, root: &Path, herdr_bin: Option<&PathBuf>) {
    let first = herdr_bin
        .map(|bin| format!("{}:", bin.display()))
        .unwrap_or_default();
    cmd.env("XDG_STATE_HOME", root.join("state"))
        .env("LOOPHERD_CONFIG_DIR", root.join("config"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("PATH", format!("{first}/usr/bin:/bin"))
        .env("HERDR_SOCKET_PATH", SOCKET)
        .env_remove("CLAUDE_PID");
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into()
}

#[test]
fn the_shepherds_read_asks_herdr_for_the_members_recent_unwrapped_lines() {
    let world = World::new();

    let read = world.run(None, &["read", "lh-9", "--lines", "5"]);

    assert_eq!(stdout(&read), "one\ntwo\n");
    assert_eq!(
        world.calls(),
        "agent list\nagent read w1:p3 --source recent-unwrapped --lines 5 --format text\n"
    );
}

#[test]
fn a_leads_read_goes_through_its_broker_for_its_own_herds_workers_alone() {
    let world = World::new();

    let read = world.run(Some("w1:p1"), &["read", "lh-1"]);
    let outside = world.run(Some("w1:p1"), &["read", "lh-9"]);

    assert_eq!(stdout(&read), "one\ntwo\n");
    assert_eq!(
        world.calls(),
        "agent list\nagent read w1:p2 --source recent-unwrapped --lines 40 --format text\n"
    );
    assert!(!outside.status.success());
    assert_eq!(
        String::from_utf8_lossy(&outside.stderr),
        "loopherd read: lead-k's broker refused read: lh-9 is in herd g; \
         lead-k reaches only herd k's workers\n"
    );
}

#[test]
fn a_leads_status_goes_through_its_broker_and_shows_its_own_herd_alone() {
    let world = World::new();

    let status = world.run(Some("w1:p1"), &["status"]);
    let strays = world.run(Some("w1:p1"), &["status", "--strays"]);

    assert_eq!(
        stdout(&status),
        "k\n  lead-k (lead)  idle  -  -  w1:t1/w1:p1\n  lh-1           idle  -  -  w1:t1/w1:p2\n"
    );
    assert_eq!(world.calls(), "agent list\n");
    assert!(!strays.status.success());
    assert!(
        String::from_utf8_lossy(&strays.stderr).contains(
            "lead-k's broker refused status: strays are herdr's agents outside every herd"
        ),
        "{strays:?}"
    );
}

#[test]
fn read_refuses_more_lines_than_its_cap() {
    let world = World::new();

    let read = world.run(None, &["read", "lh-9", "--lines", "1001"]);

    assert_eq!(read.status.code(), Some(2));
    assert_eq!(world.calls(), "");
}

#[test]
fn status_in_a_sandbox_whose_pane_holds_no_lead_is_refused_before_herdr() {
    use std::os::unix::fs::PermissionsExt;
    // Root writes past file modes, so nothing is read-only to it.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    /// Makes the state dir writable again, so it can be removed.
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    let world = World::new();
    let state = world.root.path().join("state/loopherd");
    let _restore = Restore(state.clone());
    let _ = fs::set_permissions(state.join("ledger.lock"), fs::Permissions::from_mode(0o444));
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();

    let mut status = world.loopherd(Some("w1:p9"), &["status"]);
    let bin = world.root.path().join("bin");
    set_env(&mut status, world.root.path(), Some(&bin));
    let out = status.output().unwrap();

    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(
            "refused: this loopherd runs in a sandbox, where it can't run loopherd status itself"
        ),
        "{out:?}"
    );
    assert_eq!(world.calls(), "");
}

/// Records the test process as the shepherd's Claude in w1:p1, lead-k's pane
/// id, and runs loopherd there as the shepherd with `args`.
fn run_as_shepherd_in_lead_ks_pane(world: &World, args: &[&str]) -> Output {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let (_, fields) = stat.rsplit_once(')').unwrap();
    let start: u64 = fields.split_whitespace().nth(19).unwrap().parse().unwrap();
    let pid = std::process::id();
    let record = format!(
        r#"{{"pane": "w1:p1", "socket": "{SOCKET}", "process": {{"pid": {pid}, "start": {start}}},
        "session": "s-1", "at": "2026-11-19T00:00:00Z", "used": true}}"#
    );
    fs::write(
        world.root.path().join("state/loopherd/shepherd.json"),
        record,
    )
    .unwrap();
    let mut cmd = world.loopherd(Some("w1:p1"), args);
    let bin = world.root.path().join("bin");
    set_env(&mut cmd, world.root.path(), Some(&bin));
    cmd.env("CLAUDE_PID", pid.to_string()).output().unwrap()
}

#[test]
fn the_shepherds_status_runs_directly_in_a_pane_whose_id_a_lead_once_had() {
    let world = World::new();

    let out = run_as_shepherd_in_lead_ks_pane(&world, &["status"]);

    let shown = stdout(&out);
    assert!(shown.contains("lh-9"), "{shown}");
    assert!(shown.contains("strays"), "{shown}");
}

#[test]
fn the_shepherds_read_runs_directly_in_a_pane_whose_id_a_lead_once_had() {
    let world = World::new();

    let out = run_as_shepherd_in_lead_ks_pane(&world, &["read", "lh-9", "--lines", "5"]);

    assert_eq!(stdout(&out), "one\ntwo\n");
    assert_eq!(
        world.calls(),
        "agent list\nagent read w1:p3 --source recent-unwrapped --lines 5 --format text\n"
    );
}
