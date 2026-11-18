//! `loopherd restart`: give a member that handed off a fresh session in the
//! same pane, which resumes from its handoff file (D20).

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::context::handoff_path;
use crate::continuation::follow_recorded;
use crate::herd::{Paths, Switches, lead_settings};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::lock::Lock;
use crate::report::{Body, Entry, SinceHandoff, append, member_of_own_pane, since_handoff};
use crate::shell::quote;
use crate::subagent_view::ensure_main_view;

pub struct RestartRequest<'a> {
    /// Resolves a session to the one Claude continued it in.
    pub live: &'a dyn Fn(&str) -> String,
    pub name: &'a str,
    /// Restart even if the member isn't finished since its handoff.
    pub force: bool,
    /// `$HERDR_PANE_ID` of the caller; a member can't restart itself mid-turn.
    pub own_pane: Option<&'a str>,
    /// loopherd's state dir, holding `reports/` and `handoffs/`.
    pub state: &'a Path,
    /// Where a lead's switches and review-round limit are read from.
    pub paths: &'a Paths,
    /// How long to wait for the SessionStart hook to record the new session.
    pub settle_timeout: Duration,
    pub poll: Duration,
}

/// Sends `/clear` to the member, waits for its ledger entry to move to the
/// new session, records the session Claude continued that one in and the
/// pane and tab it runs in, marks a
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
    let _running = take(req.state, name, "restart")?.ok_or_else(|| Busy(name.to_string()))?;
    if req.own_pane.is_some_and(|pane| record.is_own_pane(pane)) {
        bail!(
            "{name} can't restart itself mid-turn; run `loopherd restart --self` to restart once this turn ends"
        );
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
    if !req.force
        && let Some(why) = unfinished(req.state, name, agent.status)?
    {
        bail!("{why}; pass --force to restart it anyway");
    }
    let lead = match (record.is_lead(), &record.herd) {
        (false, _) => None,
        (true, None) => bail!("{name} is a lead with no herd recorded"),
        (true, Some(herd)) => Some(
            lead_settings(req.paths, herd)
                .with_context(|| format!("{name}'s switches, from herd {herd}"))?,
        ),
    };
    let pane = &agent.pane_id;
    ensure_main_view(
        herdr,
        name,
        pane,
        "and /clear there doesn't reach the main conversation",
        &format!("run loopherd restart {name} again"),
    )?;
    let lead = lead.as_ref();
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
    ledger.update(name, &mut |current| {
        current.pane_id = agent.pane_id.clone();
        current.tab_id = Some(agent.tab_id.clone());
        Ok(())
    })?;
    ledger.started(name, now, false)?;
    herdr.prompt(name, &resume_prompt(lead, &read))?;
    Ok(format!(
        "restarted {name} in session {live}; prompted it to read {}",
        read.display()
    ))
}

/// Why `name` can't be restarted without `--force` yet, if anything: it must
/// have reported a handoff since its last start, stopped finished with nothing
/// in flight since, and show idle or done in herdr.
fn unfinished(state: &Path, name: &str, status: AgentStatus) -> Result<Option<String>> {
    Ok(match since_handoff(&state.join("reports"), name)? {
        SinceHandoff::None => Some(format!(
            "{name} hasn't handed off since it started (after a revive, check `loopherd reports --all {name}` for its handoff)"
        )),
        SinceHandoff::NotStopped => Some(format!("{name} hasn't stopped since its handoff")),
        SinceHandoff::Stopped(stop) if stop.error.is_some() || stop.pending().is_some() => {
            Some(format!(
                "{name} isn't finished: its last stop reads `{}`",
                stop.describe()
            ))
        }
        SinceHandoff::Stopped(_) if !matches!(status, AgentStatus::Idle | AgentStatus::Done) => {
            Some(format!("{name} is {} in herdr", status.as_str()))
        }
        SinceHandoff::Stopped(_) => None,
    })
}

#[derive(Debug)]
struct Busy(String);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "a restart of {} is already running", self.0)
    }
}

impl std::error::Error for Busy {}

#[derive(Debug)]
pub enum Waited {
    Restarted,
    /// Another restart got there first.
    RestartedElsewhere,
    /// Left a note in the member's mailbox naming why.
    GaveUp,
}

/// A self-restart's waiter: polls every `every` until `req.name` is finished
/// since its handoff, then runs the restart with every non-forced check. Gives
/// up after `give_up`, or on a failed restart, with a note in its mailbox.
pub fn wait_then_restart(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    req: &RestartRequest,
    give_up: Duration,
    every: Duration,
) -> Result<Waited> {
    let req = RestartRequest {
        force: false,
        own_pane: None,
        ..*req
    };
    let name = req.name;
    let deadline = Instant::now() + give_up;
    loop {
        let handoff = handoff_path(req.state, name);
        if !handoff.is_file()
            || since_handoff(&req.state.join("reports"), name)? == SinceHandoff::None
        {
            return Ok(Waited::RestartedElsewhere);
        }
        let waiting = match waiting_on(herdr, &req) {
            Ok(Some(why)) => why,
            Ok(None) => match restart(herdr, ledger, &req, SystemTime::now()) {
                Ok(_) => return Ok(Waited::Restarted),
                Err(e) if e.is::<Busy>() => format!("{e}"),
                Err(e) => return gave_up(&req, format!("self-restart failed: {e:#}")),
            },
            Err(e) => format!("{e:#}"),
        };
        if Instant::now() >= deadline {
            return gave_up(
                &req,
                format!(
                    "self-restart gave up after {}: {waiting}. Its handoff is still in place; run loopherd restart {name} once it's finished.",
                    humantime::format_duration(give_up)
                ),
            );
        }
        std::thread::sleep(every);
    }
}

fn waiting_on(herdr: &dyn Herdr, req: &RestartRequest) -> Result<Option<String>> {
    let name = req.name;
    let Some(agent) = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name.as_deref() == Some(name))
    else {
        return Ok(Some(format!("no live agent is named {name}")));
    };
    if agent.status == AgentStatus::Blocked {
        return Ok(Some(format!("{name} is blocked in pane {}", agent.pane_id)));
    }
    unfinished(req.state, name, agent.status)
}

fn gave_up(req: &RestartRequest, text: String) -> Result<Waited> {
    let note = Entry::new(SystemTime::now(), Body::Note { text });
    append(&req.state.join("reports"), req.name, &note)?;
    Ok(Waited::GaveUp)
}

/// A self-restart claimed for a member, held until its waiter exits.
pub struct Scheduled {
    pub name: String,
    pub waiter: Lock,
    pub message: String,
}

/// How long a self-restart waits for the member's `finished` stop.
pub const SELF_RESTART_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Claims a self-restart for the member in `own_pane`, which must have a
/// handoff file and a `handoff` report since its last start. Only one waiter
/// per member holds the claim; the caller hands `waiter` to it.
pub fn schedule_self(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    own_pane: Option<&str>,
    state: &Path,
) -> Result<Scheduled> {
    let pane = own_pane.ok_or_else(|| {
        anyhow!(
            "loopherd restart --self runs only in a member's own pane ($HERDR_PANE_ID is unset)"
        )
    })?;
    let entries = ledger.entries()?;
    let name = member_of_own_pane(&entries, pane, herdr.socket().as_deref())
        .ok_or_else(|| anyhow!("no member runs in pane {pane} of this herdr"))?
        .name
        .clone();
    let handoff = handoff_path(state, &name);
    if !handoff.is_file() {
        bail!("{name} has no handoff at {}", handoff.display());
    }
    if since_handoff(&state.join("reports"), &name)? == SinceHandoff::None {
        bail!(
            "{name} hasn't reported --kind handoff since it started; write its handoff, report it, then run this"
        );
    }
    let waiter = take(state, &name, "waiter")?
        .ok_or_else(|| anyhow!("{name} already has a restart waiting for its stop"))?;
    let message = format!(
        "{name} restarts in this pane once its stop after the handoff reads finished and herdr shows it idle or done, \
         giving up after {} minutes with a note in its mailbox. End your turn now.",
        SELF_RESTART_TIMEOUT.as_secs() / 60
    );
    Ok(Scheduled {
        name,
        waiter,
        message,
    })
}

/// `name`'s `kind` lock under the state dir, or `None` while another holds it.
fn take(state: &Path, name: &str, kind: &str) -> Result<Option<Lock>> {
    let dir = state.join("restarts");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.{kind}.lock"));
    let file = std::fs::File::create(&path).with_context(|| format!("open {}", path.display()))?;
    Lock::try_take(file).with_context(|| format!("lock {}", path.display()))
}

const CLEAR_DIALOG: &str = "/clear acts on the main conversation";

/// A lead stopped its watch to hand off, so it's told to re-arm it. `/clear`
/// dropped its original prompt, so its switches and limit are given again.
fn resume_prompt(lead: Option<&(Switches, u32)>, handoff: &Path) -> String {
    let resume = format!(
        "Read your handoff at {}, then continue from it.",
        handoff.display()
    );
    match lead {
        Some((switches, rounds)) => format!(
            "{resume} Your switches: {}. Your review-round limit: {rounds}. {LEAD_REARM}",
            switches.lines().replace('\n', ", ")
        ),
        None => resume,
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
            env_pane_id: None,
            tab_id: None,
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
            earlier: vec![],
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
        ) -> anyhow::Result<crate::ledger::RemovedHerd> {
            unreachable!()
        }
        fn started(&mut self, n: &str, at: SystemTime, resumed: bool) -> Result<()> {
            self.ledger.started(n, at, resumed)
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
            fs::create_dir_all(state.path().join("config/herds")).unwrap();
            fs::write(
                state.path().join("config/herds/h.toml"),
                "repo = \"/src/repo\"\n[lead]\nprompt = \"go\"\nmerge = true\nreview_rounds = 5\n",
            )
            .unwrap();
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
            self.put_at("2026-11-02T09:00:00Z", body);
        }

        fn put_at(&self, at: &str, body: Body) {
            let at = humantime::parse_rfc3339(at).unwrap();
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
                paths: &self.paths(),
                settle_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(1),
                live,
            };
            let result = restart(&self.herdr, &mut ledger, &req, SystemTime::UNIX_EPOCH);
            let mut after = ledger.ledger.clone();
            after.0 = ledger.entries().unwrap();
            (result, after)
        }

        /// Runs a self-restart's waiter for lh-1, polling every millisecond.
        fn wait(&self, give_up: Duration) -> Result<Waited> {
            let member = record("lh-1");
            let mut ledger = Clearing {
                recorded: member.session_id.clone(),
                ledger: MemLedger([("lh-1".to_string(), member)].into(), vec![]),
                herdr: &self.herdr,
                follows: true,
                continued: None,
                clears_before: 0,
            };
            let req = RestartRequest {
                name: "lh-1",
                force: false,
                own_pane: None,
                state: self.state.path(),
                paths: &self.paths(),
                settle_timeout: Duration::from_millis(50),
                poll: Duration::from_millis(1),
                live: &|s| s.to_string(),
            };
            wait_then_restart(
                &self.herdr,
                &mut ledger,
                &req,
                give_up,
                Duration::from_millis(1),
            )
        }

        fn notes(&self) -> Vec<String> {
            crate::report::history(&self.state.path().join("reports"), "lh-1")
                .unwrap()
                .0
                .into_iter()
                .filter_map(|e| match e.body {
                    Body::Note { text } => Some(text),
                    _ => None,
                })
                .collect()
        }

        fn paths(&self) -> Paths {
            let config = self.state.path().join("config");
            Paths {
                herds: config.join("herds"),
                home: "/home/me".into(),
                feed_lock: config.join("feed.lock"),
                paused: config.join("paused"),
            }
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
        assert_eq!(
            (
                ledger.0["lh-1"].pane_id.as_str(),
                ledger.0["lh-1"].tab_id.as_deref()
            ),
            ("w9:p9", Some("w9:t9"))
        );
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
                    w.put(Body::Start { resumed: false });
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
        let cases: [(Option<&str>, Edit, &str); 4] = [
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
            (
                Some("w1:p1"),
                &|r| {
                    r.env_pane_id = Some("w1:p1".into());
                    r.pane_id = "w3:p2".into();
                },
                "lh-1 can't restart itself",
            ),
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

    /// The member's Stop hook asks for a handoff in `session`, past its limit.
    fn asked(w: &World, session: &str) -> String {
        let transcript = w.state.path().join("t.jsonl");
        fs::write(
            &transcript,
            r#"{"type":"assistant","isSidechain":false,"message":{"usage":{"input_tokens":1,"cache_read_input_tokens":200}}}"#,
        )
        .unwrap();
        let stop = format!(
            r#"{{"hook_event_name":"Stop","session_id":"{session}","transcript_path":"{}"}}"#,
            transcript.display()
        );
        crate::context::handoff_block(
            &record("lh-1"),
            stop.as_bytes(),
            Ok(100),
            w.state.path(),
            &|_| Ok(()),
        )
        .reason
        .unwrap()
        .expect("asked")
    }

    fn handoff_written_at(w: &World, at: &str) {
        fs::File::options()
            .write(true)
            .open(w.state.path().join("handoffs/lh-1.md"))
            .unwrap()
            .set_modified(humantime::parse_rfc3339(at).unwrap())
            .unwrap();
    }

    #[test]
    fn a_handoff_written_before_the_context_ask_is_still_there_to_restart_from() {
        let w = World::new();
        w.put_at("2026-11-02T08:00:00Z", Body::Start { resumed: false });
        handoff_written_at(&w, "2026-11-02T08:30:00Z");
        asked(&w, "old");
        w.handed_off();

        let (result, _) = w.restart(false, true);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fs::read_to_string(w.handoff()).unwrap(), "## State\n");
    }

    #[test]
    fn a_handoff_written_before_a_revive_is_still_there_to_restart_from() {
        let w = World::new();
        w.put_at("2026-11-02T08:00:00Z", Body::Start { resumed: false });
        handoff_written_at(&w, "2026-11-02T08:30:00Z");
        w.put_at("2026-11-02T08:45:00Z", Body::Start { resumed: true });
        asked(&w, "old");
        w.handed_off();

        let (result, _) = w.restart(false, true);

        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn after_a_restart_that_timed_out_the_next_ask_moves_the_consumed_handoff() {
        let w = World::new();
        w.put_at("2026-11-02T08:00:00Z", Body::Start { resumed: false });
        handoff_written_at(&w, "2026-11-02T08:30:00Z");
        w.handed_off();
        let (timed_out, _) = w.restart(false, false);
        assert!(timed_out.is_err());

        let reason = asked(&w, "new");
        w.put_at(
            "2026-11-02T10:00:00Z",
            Body::Handoff {
                text: "again".into(),
            },
        );
        w.stop(0, None);
        let (result, _) = w.restart(false, true);

        assert!(reason.contains("write a new one"), "{reason}");
        assert!(format!("{:#}", result.unwrap_err()).starts_with("lh-1 has no handoff at "),);
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
                r.role = lead.then_some(crate::ledger::Role::Lead);
                r.herd = Some("h".into());
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

    #[test]
    fn a_leads_resume_prompt_carries_its_resolved_switches_and_review_round_limit() {
        let w = World::new();
        w.handed_off();
        fs::write(
            w.paths().herds.with_file_name("config.toml"),
            "[lead_defaults]\nnew_cards = true\n",
        )
        .unwrap();
        let (result, _) = w.restart_as(false, true, None, &|r| {
            r.role = Some(crate::ledger::Role::Lead);
            r.herd = Some("h".into());
        });
        result.unwrap();
        let resume = &w.herdr.prompts()[1].1;
        assert!(
            resume.contains(
                "Your switches: merge on, install off, rebase off, new_cards on, \
                 design_escalation off, blocked_dialogs off. Your review-round limit: 5."
            ),
            "{resume}"
        );
    }

    #[test]
    fn a_lead_whose_herd_file_cant_be_read_is_not_cleared() {
        let w = World::new();
        w.handed_off();
        let (result, _) = w.restart_as(false, true, None, &|r| {
            r.role = Some(crate::ledger::Role::Lead);
            r.herd = Some("gone".into());
        });
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("lh-1's switches"), "{err}");
        assert!(w.herdr.prompts().is_empty());
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

    fn schedule(w: &World, own_pane: Option<&str>) -> Result<Scheduled> {
        let ledger = MemLedger([("lh-1".to_string(), record("lh-1"))].into(), vec![]);
        schedule_self(&w.herdr, &ledger, own_pane, w.state.path())
    }

    #[test]
    fn a_member_that_reported_its_handoff_schedules_its_own_restart() {
        let w = World::new();
        w.put(Body::Handoff {
            text: "at step 3".into(),
        });
        let scheduled = schedule(&w, Some("w1:p1")).unwrap();
        assert_eq!(scheduled.name, "lh-1");
        assert!(
            scheduled
                .message
                .starts_with("lh-1 restarts in this pane once"),
            "{}",
            scheduled.message
        );
    }

    #[test]
    fn self_restart_is_refused_with_nothing_scheduled() {
        type Setup<'a> = &'a dyn Fn(&World);
        let cases: [(Option<&str>, Setup, &str); 5] = [
            (
                None,
                &|w| w.put(Body::Handoff { text: "x".into() }),
                "loopherd restart --self runs only in a member's own pane",
            ),
            (
                Some("w7:p7"),
                &|w| w.put(Body::Handoff { text: "x".into() }),
                "no member runs in pane w7:p7",
            ),
            (
                Some("w1:p1"),
                &|w| {
                    w.put(Body::Handoff { text: "x".into() });
                    fs::remove_file(w.state.path().join("handoffs/lh-1.md")).unwrap();
                },
                "lh-1 has no handoff at ",
            ),
            (
                Some("w1:p1"),
                &|_| {},
                "lh-1 hasn't reported --kind handoff since it started",
            ),
            (
                Some("w1:p1"),
                &|w| {
                    w.put(Body::Handoff { text: "x".into() });
                    w.put(Body::Start { resumed: false });
                },
                "lh-1 hasn't reported --kind handoff since it started",
            ),
        ];
        for (own_pane, setup, message) in cases {
            let w = World::new();
            setup(&w);
            let err = format!("{:#}", schedule(&w, own_pane).err().expect(message));
            assert!(err.starts_with(message), "{err}");
            w.put(Body::Handoff { text: "x".into() });
            if fs::write(w.state.path().join("handoffs/lh-1.md"), "## State\n").is_ok() {
                assert!(
                    schedule(&w, Some("w1:p1")).is_ok(),
                    "{message}: a waiter was left behind"
                );
            }
        }
    }

    #[test]
    fn a_second_self_restart_is_refused_while_the_first_waits() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let first = schedule(&w, Some("w1:p1")).unwrap();
        let err = format!("{:#}", schedule(&w, Some("w1:p1")).err().unwrap());
        assert_eq!(err, "lh-1 already has a restart waiting for its stop");
        drop(first);
        assert!(schedule(&w, Some("w1:p1")).is_ok());
    }

    #[test]
    fn a_waiting_self_restart_restarts_once_the_stop_after_its_handoff_is_finished() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                assert!(w.herdr.prompts().is_empty(), "restarted before its stop");
                w.stop(0, None);
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::Restarted));
        let prompts = w.herdr.prompts();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].1, "/clear");
        assert!(w.notes().is_empty());
    }

    #[test]
    fn a_self_restart_waits_for_a_stop_with_nothing_in_flight_and_herdr_idle() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        w.stop(1, None);
        w.herdr.set_status("lh-1", AgentStatus::Working);
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                w.stop(0, None);
                std::thread::sleep(Duration::from_millis(20));
                assert!(w.herdr.prompts().is_empty(), "restarted while working");
                w.herdr.set_status("lh-1", AgentStatus::Idle);
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::Restarted));
    }

    #[test]
    fn a_self_restart_that_times_out_leaves_the_handoff_and_a_note_naming_why() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        w.stop(1, None);
        let waited = w.wait(Duration::from_millis(30)).unwrap();
        assert!(matches!(waited, Waited::GaveUp));
        assert!(w.herdr.prompts().is_empty());
        assert!(w.state.path().join("handoffs/lh-1.md").is_file());
        assert_eq!(
            w.notes(),
            [
                "self-restart gave up after 30ms: lh-1 isn't finished: its last stop reads \
              `paused: 1 shell`. Its handoff is still in place; run loopherd restart lh-1 once it's finished."
            ]
        );
    }

    #[test]
    fn a_self_restart_overtaken_by_another_restart_exits_without_clearing() {
        let w = World::new();
        w.put(Body::Handoff { text: "x".into() });
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                w.put(Body::Start { resumed: false });
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::RestartedElsewhere));
        assert!(w.herdr.prompts().is_empty());
        assert!(w.notes().is_empty());
    }

    #[test]
    fn a_restart_refuses_while_another_restart_of_the_same_member_runs() {
        let w = World::new();
        w.handed_off();
        let running = take(w.state.path(), "lh-1", "restart").unwrap().unwrap();
        let (result, _) = w.restart(true, true);
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "a restart of lh-1 is already running"
        );
        assert!(w.herdr.prompts().is_empty());
        let waited = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                drop(running);
            });
            w.wait(Duration::from_secs(5))
        });
        assert!(matches!(waited.unwrap(), Waited::Restarted));
        assert_eq!(w.herdr.prompts().len(), 2);
    }
}
