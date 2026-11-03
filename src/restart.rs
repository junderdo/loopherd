//! `loopherd restart`: give a member that handed off a fresh session in the
//! same pane, which resumes from its handoff file (D20).

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow, bail};

use crate::context::handoff_path;
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::report::last_stop;

pub struct RestartRequest<'a> {
    pub name: &'a str,
    /// Restart even if the member's last stop isn't `finished`.
    pub force: bool,
    /// loopherd's state dir, holding `reports/` and `handoffs/`.
    pub state: &'a Path,
    /// How long to wait for the SessionStart hook to record the new session.
    pub settle_timeout: Duration,
    pub poll: Duration,
}

/// Sends `/clear` to the member, waits for its ledger entry to move to the
/// new session, marks a start in its mailbox, then prompts it to read its
/// handoff. Refuses, without `force`, unless its last stop since its last
/// start is `finished` with nothing in flight.
pub fn restart(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    req: &RestartRequest,
    now: SystemTime,
) -> Result<String> {
    let name = req.name;
    let record = ledger
        .entries()?
        .remove(name)
        .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
    if record.retired {
        bail!("{name} is retired");
    }
    let handoff = handoff_path(req.state, name);
    if !handoff.is_file() {
        bail!("{name} has no handoff at {}", handoff.display());
    }
    if !req.force {
        match last_stop(&req.state.join("reports"), name)? {
            None => {
                bail!("{name} hasn't stopped since it started; pass --force to restart it anyway")
            }
            Some(stop) if stop.error.is_some() || stop.pending().is_some() => bail!(
                "{name} isn't finished: its last stop reads `{}`; pass --force to restart it anyway",
                stop.describe()
            ),
            Some(_) => {}
        }
    }
    let agent = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
        .ok_or_else(|| anyhow!("no live agent is named {name}; run loopherd revive first"))?;
    if agent.status == AgentStatus::Blocked {
        bail!(
            "{name} is blocked: answer it in pane {} first",
            agent.pane_id
        );
    }
    let old = record.session_id;
    herdr.prompt(name, "/clear")?;
    let new = wait_for_new_session(ledger, name, old.as_deref(), req)?;
    ledger.started(name, now)?;
    herdr.prompt(
        name,
        &format!(
            "Read your handoff at {}, then continue from it.",
            handoff.display()
        ),
    )?;
    Ok(format!(
        "restarted {name} in session {new}; prompted it to read {}",
        handoff.display()
    ))
}

fn wait_for_new_session(
    ledger: &dyn Ledger,
    name: &str,
    old: Option<&str>,
    req: &RestartRequest,
) -> Result<String> {
    let deadline = Instant::now() + req.settle_timeout;
    loop {
        let session = ledger.entries()?.remove(name).and_then(|r| r.session_id);
        if let Some(new) = session.filter(|s| Some(s.as_str()) != old) {
            return Ok(new);
        }
        if Instant::now() >= deadline {
            bail!(
                "sent /clear, but {name}'s ledger session is still {} after {:?}; check its pane and that loopherd's SessionStart hook is installed",
                old.unwrap_or("unset"),
                req.settle_timeout
            );
        }
        std::thread::sleep(req.poll);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::report::{Background, Body, Entry, Stop, Tally, append};

    fn record(name: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            session_id: Some("old".into()),
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: None,
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-01T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: None,
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
        }
    }

    /// A ledger whose member moves to session `new` once `/clear` was sent,
    /// as loopherd's SessionStart hook does.
    struct Clearing<'a> {
        ledger: MemLedger,
        herdr: &'a FakeHerdr,
        follows: bool,
    }

    impl Ledger for Clearing<'_> {
        fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
            let mut entries = self.ledger.entries()?;
            let cleared = self.herdr.prompts().iter().any(|(_, t)| t == "/clear");
            if self.follows && cleared {
                for record in entries.values_mut() {
                    record.session_id = Some("new".into());
                }
            }
            Ok(entries)
        }
        fn record(&mut self, r: AgentRecord) -> Result<()> {
            self.ledger.record(r)
        }
        fn update(
            &mut self,
            n: &str,
            c: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            self.ledger.update(n, c)
        }
        fn started(&mut self, n: &str, at: SystemTime) -> Result<()> {
            self.ledger.started(n, at)
        }
    }

    struct World {
        state: tempfile::TempDir,
        herdr: FakeHerdr,
    }

    impl World {
        fn new() -> World {
            let state = tempfile::tempdir().unwrap();
            fs::create_dir_all(state.path().join("handoffs")).unwrap();
            fs::write(state.path().join("handoffs/lh-1.md"), "## State\n").unwrap();
            World {
                state,
                herdr: FakeHerdr::new(&[("w1", "loopherd")]).with_agent("lh-1"),
            }
        }

        fn stop(&self, shells: u32, error: Option<&str>) {
            let stop = Stop {
                background: Tally::Known(Background {
                    shells,
                    ..Background::default()
                }),
                crons: Tally::Known(0),
                error: error.map(str::to_string),
                session: None,
            };
            self.put(Body::Stop(stop));
        }

        fn put(&self, body: Body) {
            let at = humantime::parse_rfc3339("2026-11-02T09:00:00Z").unwrap();
            append(
                &self.state.path().join("reports"),
                "lh-1",
                &Entry::new(at, body),
            )
            .unwrap();
        }

        fn restart(&self, force: bool, follows: bool) -> (Result<String>, MemLedger) {
            let mut ledger = Clearing {
                ledger: MemLedger([("lh-1".to_string(), record("lh-1"))].into(), vec![]),
                herdr: &self.herdr,
                follows,
            };
            let req = RestartRequest {
                name: "lh-1",
                force,
                state: self.state.path(),
                settle_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(1),
            };
            let result = restart(&self.herdr, &mut ledger, &req, SystemTime::UNIX_EPOCH);
            let mut after = ledger.ledger.clone();
            after.0 = ledger.entries().unwrap();
            (result, after)
        }

        fn handoff(&self) -> String {
            self.state
                .path()
                .join("handoffs/lh-1.md")
                .display()
                .to_string()
        }
    }

    #[test]
    fn a_finished_member_is_cleared_then_prompted_to_read_its_handoff() {
        let w = World::new();
        w.stop(0, None);
        let (result, ledger) = w.restart(false, true);
        assert_eq!(
            result.unwrap(),
            format!(
                "restarted lh-1 in session new; prompted it to read {}",
                w.handoff()
            )
        );
        assert_eq!(
            w.herdr.prompts(),
            [
                ("lh-1".to_string(), "/clear".to_string()),
                (
                    "lh-1".to_string(),
                    format!(
                        "Read your handoff at {}, then continue from it.",
                        w.handoff()
                    )
                ),
            ]
        );
        assert_eq!(ledger.0["lh-1"].session_id.as_deref(), Some("new"));
        assert_eq!(ledger.1, ["lh-1"]);
    }

    #[test]
    fn a_member_not_finished_is_refused_without_force_and_sent_nothing() {
        for (setup, message) in [
            (
                &(|w: &World| w.stop(1, None)) as &dyn Fn(&World),
                "lh-1 isn't finished: its last stop reads `paused: 1 shell`; pass --force to restart it anyway",
            ),
            (
                &|w: &World| w.stop(0, Some("rate_limit")),
                "lh-1 isn't finished: its last stop reads `error rate_limit; finished`; pass --force to restart it anyway",
            ),
            (
                &|w: &World| {
                    w.stop(0, None);
                    w.put(Body::Start);
                },
                "lh-1 hasn't stopped since it started; pass --force to restart it anyway",
            ),
            (
                &|_: &World| {},
                "lh-1 hasn't stopped since it started; pass --force to restart it anyway",
            ),
        ] {
            let w = World::new();
            setup(&w);
            let (result, _) = w.restart(false, true);
            assert_eq!(format!("{:#}", result.unwrap_err()), message);
            assert!(w.herdr.prompts().is_empty());

            let (result, _) = w.restart(true, true);
            assert!(result.is_ok(), "{message}");
        }
    }

    #[test]
    fn without_a_handoff_file_nothing_is_sent() {
        let w = World::new();
        w.stop(0, None);
        fs::remove_file(w.state.path().join("handoffs/lh-1.md")).unwrap();
        let (result, _) = w.restart(true, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("lh-1 has no handoff at "),);
        assert!(w.herdr.prompts().is_empty());
    }

    #[test]
    fn a_session_that_never_moves_fails_before_the_handoff_prompt() {
        let w = World::new();
        w.stop(0, None);
        let (result, ledger) = w.restart(false, false);
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "sent /clear, but lh-1's ledger session is still old after 50ms; check its pane and that loopherd's SessionStart hook is installed"
        );
        assert_eq!(w.herdr.prompts().len(), 1);
        assert!(ledger.1.is_empty());
    }

    #[test]
    fn a_blocked_or_missing_agent_is_refused() {
        let w = World::new();
        w.stop(0, None);
        w.herdr.set_status("lh-1", AgentStatus::Blocked);
        let (result, _) = w.restart(false, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("lh-1 is blocked"));
        w.herdr.remove_agent("lh-1");
        let (result, _) = w.restart(false, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("no live agent is named lh-1"));
        assert!(w.herdr.prompts().is_empty());
    }
}
