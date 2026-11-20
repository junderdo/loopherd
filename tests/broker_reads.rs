//! A lead's `status` through its broker, with a stub `herdr` on PATH that
//! logs each call. The lead's own PATH has no `herdr`, so only its broker
//! can reach one.

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
                 esac\n",
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loopherd"));
        let bin = self.root.path().join("bin");
        set_env(&mut cmd, self.root.path(), pane.is_none().then_some(&bin));
        match pane {
            Some(pane) => cmd.env("HERDR_PANE_ID", pane),
            None => cmd.env_remove("HERDR_PANE_ID"),
        };
        cmd.args(args).output().unwrap()
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
