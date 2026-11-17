//! `loopherd restart`: give a member that handed off a fresh session in the
//! same pane, which resumes from its handoff file (D20).

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::context::handoff_path;
use crate::continuation::follow_recorded;
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::report::{SinceHandoff, since_handoff};
use crate::shell::quote;
use crate::subagent_view::shows_subagent_view;

pub struct RestartRequest<'a> {
    /// Resolves a session to the one Claude continued it in.
    pub live: &'a dyn Fn(&str) -> String,
    pub name: &'a str,
    /// Restart even if the member isn't finished since its handoff.
    pub force: bool,
    /// `$HERDR_PANE_ID` of the caller; a member can't restart itself.
    pub own_pane: Option<&'a str>,
    /// loopherd's state dir, holding `reports/` and `handoffs/`.
    pub state: &'a Path,
    /// How long to wait for the SessionStart hook to record the new session.
    pub settle_timeout: Duration,
    pub poll: Duration,
}

/// Sends `/clear` to the member, waits for its ledger entry to move to the
/// new session, records the session Claude continued that one in, marks a
/// start in its mailbox, then prompts it to read its handoff. Refuses a pane
/// showing a subagent's view, where `/clear` opens a dialog instead. Refuses,
/// without `force`, unless it reported a handoff since its last start, its
/// last stop since is `finished` with nothing in flight, and herdr shows it
/// idle or done.
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
    let socket = herdr.socket();
    match &record.socket {
        None => bail!(
            "{name} was recorded without a herdr socket, so loopherd can't follow its /clear; retire it and add it again"
        ),
        Some(theirs) if socket.as_ref().is_some_and(|ours| ours != theirs) => bail!(
            "{name} is in the herdr at {}, not this one",
            theirs.display()
        ),
        Some(_) => {}
    }
    if req.own_pane == Some(record.pane_id.as_str()) {
        bail!("{name} can't restart itself; whoever reads its handoff line restarts it");
    }
    let handoff = handoff_path(req.state, name);
    if !handoff.is_file() {
        bail!("{name} has no handoff at {}", handoff.display());
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
    if !req.force {
        let refuse = |why: String| anyhow!("{why}; pass --force to restart it anyway");
        match since_handoff(&req.state.join("reports"), name)? {
            SinceHandoff::None => {
                return Err(refuse(format!(
                    "{name} hasn't handed off since it started (after a revive, check `loopherd reports --all {name}` for its handoff)"
                )));
            }
            SinceHandoff::NotStopped => {
                return Err(refuse(format!("{name} hasn't stopped since its handoff")));
            }
            SinceHandoff::Stopped(stop) if stop.error.is_some() || stop.pending().is_some() => {
                return Err(refuse(format!(
                    "{name} isn't finished: its last stop reads `{}`",
                    stop.describe()
                )));
            }
            SinceHandoff::Stopped(_) => {}
        }
        if !matches!(agent.status, AgentStatus::Idle | AgentStatus::Done) {
            return Err(refuse(format!(
                "{name} is {} in herdr",
                agent.status.as_str()
            )));
        }
    }
    let pane = &agent.pane_id;
    let screen = herdr
        .visible_text(pane)
        .with_context(|| format!("read {name}'s pane {pane} to check for a subagent view"))?;
    if shows_subagent_view(&screen) {
        bail!(
            "{name}'s pane {pane} is showing a subagent's view, and /clear there doesn't reach the main \
             conversation: switch the pane back to the main conversation, then run loopherd restart {name} again"
        );
    }
    let lead = record.is_lead();
    let old = record.session_id;
    let old_live = old.as_deref().map(req.live);
    herdr.prompt(name, "/clear")?;
    let new = wait_for_new_session(ledger, name, old.as_deref(), old_live, req);
    if let Err(e) = &new {
        // A narrow pane wraps the dialog's text inside its box.
        let shows_dialog = herdr.visible_text(pane).is_ok_and(|t| {
            t.replace('│', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(CLEAR_DIALOG)
        });
        if shows_dialog {
            bail!(
                "{e:#}: {name}'s pane {pane} shows Claude's \"{CLEAR_DIALOG}\" dialog, so it was in a subagent's view. \
                 No handoff prompt was sent and the handoff is still at {}. Pick Cancel, switch the pane back to the \
                 main conversation, then run loopherd restart {name} again.",
                handoff.display()
            );
        }
        let resume = resume_prompt(lead, &handoff);
        bail!(
            "{e:#}: the /clear may be queued behind a running turn, or loopherd's SessionStart hook isn't installed. \
             No handoff prompt was sent and the handoff is still at {}. Once {name}'s pane shows a cleared session, \
             run: loopherd tell {} {}",
            handoff.display(),
            quote(name),
            quote(&resume)
        );
    }
    let new = new?;
    let entries = ledger.entries()?;
    let current = entries
        .get(name)
        .ok_or_else(|| anyhow!("{name} left the ledger during the restart"))?;
    let live = follow_recorded(ledger, &entries, current, req.live)?
        .unwrap_or_else(|| current.clone())
        .session_id
        .unwrap_or(new);
    // Moved aside once the new session is confirmed, so no later restart resumes from it.
    let read = handoff.with_extension("prev.md");
    std::fs::rename(&handoff, &read)
        .with_context(|| format!("move {} to {}", handoff.display(), read.display()))?;
    ledger.started(name, now)?;
    herdr.prompt(name, &resume_prompt(lead, &read))?;
    Ok(format!(
        "restarted {name} in session {live}; prompted it to read {}",
        read.display()
    ))
}

const CLEAR_DIALOG: &str = "/clear acts on the main conversation";

/// A lead stopped its watch to hand off, so it's told to re-arm it.
fn resume_prompt(lead: bool, handoff: &Path) -> String {
    let resume = format!(
        "Read your handoff at {}, then continue from it.",
        handoff.display()
    );
    match lead {
        true => format!("{resume} {LEAD_REARM}"),
        false => resume,
    }
}

const LEAD_REARM: &str = "Your `loopherd watch` is no longer running: \
    run `loopherd status` and `loopherd reports --all --tail 40` (for a fuller history, \
    `--since <time>` or plain `--all`), then read the skill's Lead section \
    (`loopherd --skill --section lead`, not the whole skill) and re-arm your \
    watch as it says.";

/// `old_live` is the session Claude continued `old` in before the `/clear`; a
/// hook moving the ledger there isn't the clear.
fn wait_for_new_session(
    ledger: &dyn Ledger,
    name: &str,
    old: Option<&str>,
    old_live: Option<String>,
    req: &RestartRequest,
) -> Result<String> {
    let deadline = Instant::now() + req.settle_timeout;
    loop {
        let session = ledger.entries()?.remove(name).and_then(|r| r.session_id);
        if let Some(new) = session
            .as_ref()
            .filter(|s| Some(s.as_str()) != old && Some(*s) != old_live.as_ref())
        {
            return Ok(new.clone());
        }
        if Instant::now() >= deadline {
            bail!(
                "sent /clear, but {name}'s ledger session is still {} after {:?}",
                session.as_deref().unwrap_or("unset"),
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
    use crate::herdr::fake::{FAKE_SOCKET, FakeHerdr};
    use crate::ledger::{AgentRecord, MemLedger};
    use crate::report::{Background, Body, Entry, Stop, Tally, append};
    use crate::subagent_view::screens::{MAIN_VIEW, SUBAGENT_PICKER, SUBAGENT_PROMPT};

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
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: Some(FAKE_SOCKET.into()),
            process: None,
            role: None,
            after_merge: None,
        }
    }

    /// A ledger whose member moves to session `new` once `/clear` was sent,
    /// as loopherd's SessionStart hook does.
    struct Clearing<'a> {
        ledger: MemLedger,
        herdr: &'a FakeHerdr,
        follows: bool,
        clears_before: usize,
        /// Where a hook moves the recorded session as the `/clear` lands,
        /// when no new session follows.
        continued: Option<&'static str>,
        recorded: Option<String>,
    }

    impl Ledger for Clearing<'_> {
        fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
            let mut entries = self.ledger.entries()?;
            let clears = self
                .herdr
                .prompts()
                .iter()
                .filter(|(_, t)| t == "/clear")
                .count();
            let cleared = clears > self.clears_before;
            let session = match cleared {
                true if self.follows => Some("new"),
                true => self.continued,
                false => None,
            };
            // Leaves alone a session restart has written since.
            let untouched = |r: &AgentRecord| r.session_id == self.recorded;
            if let Some(session) = session {
                for record in entries.values_mut().filter(|r| untouched(r)) {
                    record.session_id = Some(session.into());
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
            // Sees the session the hook wrote, as a real ledger's fresh read would.
            if let Some(seen) = self.entries()?.remove(n) {
                self.ledger.record(seen)?;
            }
            self.ledger.update(n, c)
        }
        fn remove_herd(
            &mut self,
            _: &str,
            _: &mut dyn FnMut(&[String]) -> anyhow::Result<()>,
        ) -> anyhow::Result<Vec<String>> {
            unreachable!()
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
                context: None,
                limit: None,
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

        /// A finished stop after a handoff report.
        fn handed_off(&self) {
            self.put(Body::Handoff {
                text: "at step 3".into(),
            });
            self.stop(0, None);
        }

        fn restart(&self, force: bool, follows: bool) -> (Result<String>, MemLedger) {
            self.restart_as(force, follows, None, &|_| {})
        }

        fn restart_as(
            &self,
            force: bool,
            follows: bool,
            own_pane: Option<&str>,
            edit: &dyn Fn(&mut AgentRecord),
        ) -> (Result<String>, MemLedger) {
            self.restart_continued(force, follows, own_pane, edit, None, &|s| s.to_string())
        }

        fn restart_continued(
            &self,
            force: bool,
            follows: bool,
            own_pane: Option<&str>,
            edit: &dyn Fn(&mut AgentRecord),
            continued: Option<&'static str>,
            live: &dyn Fn(&str) -> String,
        ) -> (Result<String>, MemLedger) {
            let mut member = record("lh-1");
            edit(&mut member);
            let mut ledger = Clearing {
                recorded: member.session_id.clone(),
                ledger: MemLedger([("lh-1".to_string(), member)].into(), vec![]),
                herdr: &self.herdr,
                follows,
                continued,
                clears_before: self
                    .herdr
                    .prompts()
                    .iter()
                    .filter(|(_, t)| t == "/clear")
                    .count(),
            };
            let req = RestartRequest {
                name: "lh-1",
                force,
                own_pane,
                state: self.state.path(),
                settle_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(1),
                live,
            };
            let result = restart(&self.herdr, &mut ledger, &req, SystemTime::UNIX_EPOCH);
            let mut after = ledger.ledger.clone();
            after.0 = ledger.entries().unwrap();
            (result, after)
        }

        fn handoff(&self) -> String {
            self.state
                .path()
                .join("handoffs/lh-1.prev.md")
                .display()
                .to_string()
        }
    }

    #[test]
    fn a_finished_member_is_cleared_then_prompted_to_read_its_handoff() {
        let w = World::new();
        w.handed_off();
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

    fn continued(session: &str) -> String {
        match session {
            "stub" => "old".into(),
            "new" => "newer".into(),
            other => other.into(),
        }
    }

    #[test]
    fn a_cleared_session_that_continued_is_restarted_in_and_recorded_as_the_live_one() {
        let w = World::new();
        w.handed_off();
        let (result, ledger) = w.restart_continued(false, true, None, &|_| {}, None, &continued);
        assert!(
            result
                .unwrap()
                .starts_with("restarted lh-1 in session newer;"),
        );
        assert_eq!(ledger.0["lh-1"].session_id.as_deref(), Some("newer"));
    }

    #[test]
    fn a_clear_that_continues_the_old_session_into_the_new_one_is_confirmed() {
        let w = World::new();
        w.handed_off();
        let cleared = || w.herdr.prompts().iter().any(|(_, t)| t == "/clear");
        let live = |s: &str| match s {
            "old" if cleared() => "new".to_string(),
            other => other.to_string(),
        };
        let (result, _) = w.restart_continued(false, true, None, &|_| {}, None, &live);
        assert!(
            result
                .unwrap()
                .starts_with("restarted lh-1 in session new;")
        );
    }

    #[test]
    fn a_recorded_stub_moving_to_its_live_session_is_not_taken_for_the_clear() {
        let w = World::new();
        w.handed_off();
        let stub = |m: &mut AgentRecord| m.session_id = Some("stub".into());
        let (result, _) = w.restart_continued(false, false, None, &stub, Some("old"), &continued);
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("ledger session is still old after"),
            "{error}"
        );
        assert_eq!(
            w.herdr.prompts(),
            [("lh-1".to_string(), "/clear".to_string())]
        );
    }

    #[test]
    fn a_clear_that_never_lands_names_the_session_the_ledger_still_records() {
        let w = World::new();
        w.handed_off();
        let stub = |m: &mut AgentRecord| m.session_id = Some("stub".into());
        let (result, _) = w.restart_continued(false, false, None, &stub, None, &continued);
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("ledger session is still stub after"),
            "{error}"
        );
    }

    #[test]
    fn a_member_not_finished_since_its_handoff_is_refused_without_force_and_sent_nothing() {
        let handoff = || Body::Handoff { text: "x".into() };
        type Setup<'a> = &'a dyn Fn(&World);
        let cases: [(Setup, &str); 7] = [
            (
                &|w| {
                    w.put(handoff());
                    w.stop(1, None);
                },
                "lh-1 isn't finished: its last stop reads `paused: 1 shell`",
            ),
            (
                &|w| {
                    w.put(handoff());
                    w.stop(0, Some("rate_limit"));
                },
                "lh-1 isn't finished: its last stop reads `error rate_limit; finished`",
            ),
            (
                &|w| {
                    w.stop(0, None);
                    w.put(handoff());
                },
                "lh-1 hasn't stopped since its handoff",
            ),
            (
                &|w| {
                    w.handed_off();
                    w.put(Body::Start);
                    w.stop(0, None);
                },
                "lh-1 hasn't handed off since it started (after a revive, check `loopherd reports --all lh-1` for its handoff)",
            ),
            (
                &|w| w.stop(0, None),
                "lh-1 hasn't handed off since it started (after a revive, check `loopherd reports --all lh-1` for its handoff)",
            ),
            (
                &|_| {},
                "lh-1 hasn't handed off since it started (after a revive, check `loopherd reports --all lh-1` for its handoff)",
            ),
            (
                &|w| {
                    w.handed_off();
                    w.herdr.set_status("lh-1", AgentStatus::Working);
                },
                "lh-1 is working in herdr",
            ),
        ];
        for (setup, message) in cases {
            let w = World::new();
            setup(&w);
            let (result, _) = w.restart(false, true);
            assert_eq!(
                format!("{:#}", result.unwrap_err()),
                format!("{message}; pass --force to restart it anyway")
            );
            assert!(w.herdr.prompts().is_empty());

            let (result, _) = w.restart(true, true);
            assert!(result.is_ok(), "{message}");
        }
    }

    #[test]
    fn a_member_it_cant_follow_or_its_own_pane_is_refused_even_with_force() {
        type Edit<'a> = &'a dyn Fn(&mut AgentRecord);
        let cases: [(Option<&str>, Edit, &str); 3] = [
            (
                None,
                &|r| r.socket = None,
                "lh-1 was recorded without a herdr socket",
            ),
            (
                None,
                &|r| r.socket = Some("/run/other.sock".into()),
                "lh-1 is in the herdr at /run/other.sock, not this one",
            ),
            (Some("w1:p1"), &|_| {}, "lh-1 can't restart itself"),
        ];
        for (own_pane, edit, message) in cases {
            let w = World::new();
            w.handed_off();
            let (result, _) = w.restart_as(true, true, own_pane, edit);
            let err = format!("{:#}", result.unwrap_err());
            assert!(err.starts_with(message), "{err}");
            assert!(w.herdr.prompts().is_empty());
        }
    }

    #[test]
    fn a_handoff_is_used_once_even_by_a_forced_restart() {
        let w = World::new();
        w.handed_off();
        assert!(w.restart(false, true).0.is_ok());
        let (again, _) = w.restart(true, true);
        assert!(format!("{:#}", again.unwrap_err()).starts_with("lh-1 has no handoff at "),);
        assert_eq!(w.herdr.prompts().len(), 2);
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
    fn a_session_that_never_moves_fails_before_the_handoff_prompt_and_keeps_the_handoff() {
        let w = World::new();
        w.handed_off();
        let (result, ledger) = w.restart(false, false);
        let md = w
            .state
            .path()
            .join("handoffs/lh-1.md")
            .display()
            .to_string();
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            format!(
                "sent /clear, but lh-1's ledger session is still old after 50ms: the /clear may be \
                 queued behind a running turn, or loopherd's SessionStart hook isn't installed. No \
                 handoff prompt was sent and the handoff is still at {md}. Once lh-1's pane shows a \
                 cleared session, run: loopherd tell 'lh-1' 'Read your handoff at {md}, then \
                 continue from it.'"
            )
        );
        assert_eq!(w.herdr.prompts().len(), 1);
        assert!(ledger.1.is_empty());

        let (retry, _) = w.restart(false, true);
        assert!(retry.is_ok(), "{retry:?}");
    }

    #[test]
    fn a_leads_resume_prompt_has_it_re_arm_its_watch_and_a_workers_doesnt() {
        let rearm = "Your `loopherd watch` is no longer running: run `loopherd status` and \
                     `loopherd reports --all --tail 40` (for a fuller history, `--since <time>` \
                     or plain `--all`), then read the skill's Lead section \
                     (`loopherd --skill --section lead`, not the whole skill) and re-arm \
                     your watch as it says.";
        for (lead, wants) in [(true, true), (false, false)] {
            let w = World::new();
            w.handed_off();
            let (result, _) = w.restart_as(false, true, None, &|r| {
                r.role = lead.then_some(crate::ledger::Role::Lead)
            });
            result.unwrap();
            let resume = &w.herdr.prompts()[1].1;
            assert!(
                resume.starts_with(&format!(
                    "Read your handoff at {}, then continue from it.",
                    w.handoff()
                )),
                "{resume}"
            );
            assert_eq!(resume.contains(rearm), wants, "{resume}");
        }
    }

    const PANE: &str = "w9:p9";

    const MAIN_VIEW_QUOTING_A_SUBAGENT_VIEW: &str = "\
● Bash(cat src/restart.rs)
  ⎿  ────────────────────────────────────────
     ❯ Message @reviewer…
     ────────────────────────────────────────
       ◯ main
       ● local_agent · Review PR #97 …
● Next up:
  ◯ main loop rewrite
  ● local_agent cleanup

────────────────────────────────────────
❯ 
────────────────────────────────────────
  ? for shortcuts
";

    const SUBAGENT_ROUNDED_BOX: &str = "\
● Reviewing PR #97 against the card.

╭──────────────────────────────────────╮
│ ❯ Message @reviewer…                 │
╰──────────────────────────────────────╯
  ← 1 agent
";

    const SUBAGENT_LABELLED_RULE: &str = "\
● Reviewing PR #97 against the card.

──────────────── reviewer ──────────────
❯ Message @reviewer…
────────────────────────────────────────
  ← 1 agent
";

    const SUBAGENT_PICKER_NO_RULES: &str = "\
● Reviewing PR #97 against the card.

❯ 
  ◯ main
  ● local_agent · Review PR #97 …
";

    const MAIN_VIEW_WITH_A_STATUS_BULLET: &str = "\
● Handoff written; finished.

────────────────────────────────────────
❯ 
────────────────────────────────────────
  ● 1 background task
  ⏵⏵ auto mode on · ← 1 agent
  ◯ local_agent · Review PR #97 …
  ● main
";

    const BOXED_WRAPPED_CLEAR_DIALOG: &str = "\
╭──────────────────────────╮
│ /clear acts on the main  │
│ conversation, not this   │
│ agent                    │
│                          │
│ ❯ Cancel                 │
╰──────────────────────────╯
";

    const WRAPPED_CLEAR_DIALOG: &str = "\
 /clear acts on the main
 conversation, not this agent

 ❯ Cancel
   Run /clear on the main
   conversation
";

    const CLEAR_DIALOG: &str = "\
 /clear acts on the main conversation, not this agent

 ❯ Cancel
   Run /clear on the main conversation
";

    #[test]
    fn a_pane_showing_a_subagent_view_is_refused_even_with_force_and_sent_nothing() {
        for screen in [
            SUBAGENT_PROMPT,
            SUBAGENT_PICKER,
            SUBAGENT_ROUNDED_BOX,
            SUBAGENT_LABELLED_RULE,
            SUBAGENT_PICKER_NO_RULES,
        ] {
            for force in [false, true] {
                let w = World::new();
                w.handed_off();
                w.herdr.set_screen(PANE, screen);
                let (result, ledger) = w.restart(force, true);
                assert_eq!(
                    format!("{:#}", result.unwrap_err()),
                    "lh-1's pane w9:p9 is showing a subagent's view, and /clear there doesn't reach \
                     the main conversation: switch the pane back to the main conversation, then run \
                     loopherd restart lh-1 again"
                );
                assert!(w.herdr.prompts().is_empty());
                assert!(w.state.path().join("handoffs/lh-1.md").is_file());
                assert!(ledger.1.is_empty());
            }
        }
    }

    #[test]
    fn a_pane_showing_the_main_conversation_restarts() {
        for screen in [
            MAIN_VIEW,
            MAIN_VIEW_QUOTING_A_SUBAGENT_VIEW,
            MAIN_VIEW_WITH_A_STATUS_BULLET,
        ] {
            let w = World::new();
            w.handed_off();
            w.herdr.set_screen(PANE, screen);
            let (result, _) = w.restart(false, true);
            assert!(result.is_ok(), "{screen}: {result:?}");
            assert_eq!(w.herdr.prompts().len(), 2);
        }
    }

    #[test]
    fn a_clear_dialog_in_the_pane_after_a_timeout_is_named() {
        for dialog in [
            CLEAR_DIALOG,
            WRAPPED_CLEAR_DIALOG,
            BOXED_WRAPPED_CLEAR_DIALOG,
        ] {
            a_clear_dialog_after_a_timeout_is_named(dialog);
        }
    }

    fn a_clear_dialog_after_a_timeout_is_named(dialog: &str) {
        let w = World::new();
        w.handed_off();
        w.herdr.set_screen_on_prompt(PANE, dialog);
        let (result, _) = w.restart(false, false);
        let md = w
            .state
            .path()
            .join("handoffs/lh-1.md")
            .display()
            .to_string();
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            format!(
                "sent /clear, but lh-1's ledger session is still old after 50ms: lh-1's pane w9:p9 \
                 shows Claude's \"/clear acts on the main conversation\" dialog, so it was in a \
                 subagent's view. No handoff prompt was sent and the handoff is still at {md}. Pick \
                 Cancel, switch the pane back to the main conversation, then run loopherd restart \
                 lh-1 again."
            )
        );
        assert_eq!(w.herdr.prompts().len(), 1);
    }

    #[test]
    fn a_blocked_or_missing_agent_is_refused() {
        let w = World::new();
        w.handed_off();
        w.herdr.set_status("lh-1", AgentStatus::Blocked);
        let (result, _) = w.restart(false, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("lh-1 is blocked"));
        w.herdr.remove_agent("lh-1");
        let (result, _) = w.restart(false, true);
        assert!(format!("{:#}", result.unwrap_err()).starts_with("no live agent is named lh-1"));
        assert!(w.herdr.prompts().is_empty());
    }
}
