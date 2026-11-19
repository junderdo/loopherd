//! The shepherd restarting itself after its handoff, with `[shepherd]
//! self_restart` on: `restart --self` from its pane arms a detached waiter,
//! which sends `/clear` and then `go` into that pane once the turn has ended
//! and the human has left it alone for [`WARNING`]. Any prompt from the human
//! there cancels it; a restart that can't happen safely gives up and says so.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};

use crate::context;
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Process;
use crate::lock::Lock;
use crate::pane_label;
use crate::restart;
use crate::shepherd_resume::{self, Attempt, AttemptState, Place, Recorded, StopAt};
use crate::subagent_view::{TEXT_REACHES_SUBAGENT, input_box_empty, shows_subagent_view};
use crate::watch::Clock;

/// How long the pane must go without a prompt from the human.
pub const WARNING: Duration = Duration::from_secs(5 * 60);
/// How long the waiter waits for its conditions before giving up.
pub const TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How soon after a self-restart another is refused.
pub const GUARD: Duration = Duration::from_secs(30 * 60);
/// How long after `/clear` the resume has to mark the record used.
pub const RESUME_WAIT: Duration = Duration::from_secs(30);
pub const POLL: Duration = Duration::from_secs(2);
/// A longer gap between polls, as when the machine slept, starts the
/// human's [`WARNING`] minutes again: nothing was seen during it.
const GAP: Duration = Duration::from_secs(30);
/// How long arming waits for a cancelled waiter to let go of its claim.
const CLAIM_WAIT: Duration = Duration::from_secs(5);

const MANUAL: &str = "your handoff stays for the human's `/clear` and `go`";

/// What `restart --self` needs from config and Claude's settings.
pub struct Switches {
    pub enabled: bool,
    pub prompt_hook: bool,
}

/// A pending self-restart, claimed until the waiter holding `waiter` exits.
pub struct Armed {
    pub waiter: Lock,
    pub message: String,
}

/// Arms a self-restart for the shepherd at `place`, the caller's pane,
/// herdr and Claude, which must match the record of its handoff. Refuses
/// when the switch is off, the UserPromptSubmit hook that cancels it is
/// missing, no handoff was written since the record, or one already ran
/// within [`GUARD`]; that last also goes to `notify`.
pub fn arm(
    state: &Path,
    place: &Place,
    switches: &Switches,
    clock: &mut dyn Clock,
    notify: &mut dyn FnMut(&str),
) -> Result<Armed> {
    if !switches.enabled {
        bail!("[shepherd] self_restart is off in config.toml; {MANUAL}");
    }
    if !switches.prompt_hook {
        bail!(
            "loopherd's UserPromptSubmit hook isn't installed, so a prompt couldn't stop a self-restart \
             (fix: loopherd install-hook); {MANUAL}"
        );
    }
    let waiter = claim(state, clock)?;
    let now = clock.now();
    let armed =
        shepherd_resume::update(state, place, |r| arm_record(r, now))?.ok_or_else(|| {
            anyhow!(
                "no handoff of yours is recorded for pane {} and this Claude; {MANUAL}",
                place.pane
            )
        })?;
    match armed {
        Ok(()) => Ok(Armed {
            waiter,
            message: format!(
                "Armed: you restart yourself in pane {} once this turn ends and the human sends no \
                 prompt here for {} minutes; any prompt here stops it. It gives up after {} minutes, \
                 saying why. Warn the human in your last message, then end your turn.",
                place.pane,
                WARNING.as_secs() / 60,
                TIMEOUT.as_secs() / 60,
            ),
        }),
        Err(Refused::Plain(why)) => bail!("{why}; {MANUAL}"),
        Err(Refused::Guard(why)) => {
            notify(&format!("self-restart refused: {why}"));
            bail!("{why}; {MANUAL}")
        }
    }
}

enum Refused {
    Plain(String),
    /// Also sent as a notification.
    Guard(String),
}

fn arm_record(r: &mut Recorded, now: SystemTime) -> Result<(), Refused> {
    if r.used {
        return Err(Refused::Plain(
            "your recorded handoff was already resumed".into(),
        ));
    }
    if !r.handoff_written {
        return Err(Refused::Plain(
            "no handoff was written since your handoff line; write it first".into(),
        ));
    }
    let last = r.restart.last_self_restart.as_deref().and_then(parse_time);
    if let Some(last) = last.filter(|last| now < *last + GUARD) {
        return Err(Refused::Guard(format!(
            "you last restarted yourself at {}, under {} minutes ago",
            humantime::format_rfc3339_seconds(last),
            GUARD.as_secs() / 60
        )));
    }
    r.restart.self_restart = Some(Attempt {
        armed: time(now),
        state: AttemptState::Pending,
        why: None,
    });
    Ok(())
}

/// The shepherd's waiter claim, waiting briefly for a cancelled waiter to exit.
fn claim(state: &Path, clock: &mut dyn Clock) -> Result<Lock> {
    let give_up = clock.now() + CLAIM_WAIT;
    loop {
        if let Some(lock) = restart::take(state, context::SHEPHERD, "waiter")? {
            return Ok(lock);
        }
        if clock.now() >= give_up {
            bail!("a self-restart of yours is already waiting");
        }
        clock.sleep(Duration::from_millis(250));
    }
}

/// What the waiter reads and acts through.
pub struct Waiter<'a> {
    pub herdr: &'a dyn Herdr,
    pub state: &'a Path,
    /// Where pane labels are marked, so the human's next prompt clears one.
    pub mailboxes: &'a Path,
    pub place: &'a Place,
    pub alive: &'a dyn Fn(&Process) -> bool,
    /// Reads `[shepherd] self_restart` afresh, so turning it off stops the waiter.
    pub enabled: &'a dyn Fn() -> Result<bool>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Waited {
    /// Sent `/clear`, saw the resume, then sent `go`.
    Restarted,
    /// Sent `/clear`, but no resume came, so sent the paste line.
    Fallback,
    /// A prompt cancelled it, or the human resumed by hand.
    Stopped,
    GaveUp(String),
}

enum Gate {
    Go,
    Wait(String),
    Stop,
    Cancel(String),
    GiveUp(String),
}

use Gate::GiveUp;

/// Polls every [`POLL`] until the shepherd's stop since arming reads
/// finished with nothing in flight, herdr shows its pane idle or done, the
/// human sent no prompt there for [`WARNING`] of polls it saw, and its
/// input box is empty; then restarts it, unless the machine slept while it
/// checked. Stops when the switch is turned
/// off. Gives up after [`TIMEOUT`], when its pane or Claude is gone, or on
/// any failure: the reason goes in the record, to `notify`, and on the
/// pane's label.
pub fn wait(w: &Waiter, clock: &mut dyn Clock, notify: &mut dyn FnMut(&str)) -> Result<Waited> {
    let deadline = clock.now() + TIMEOUT;
    let mut quiet_from = SystemTime::UNIX_EPOCH;
    let mut polled = clock.now();
    loop {
        let now = clock.now();
        if paused(polled, now) {
            quiet_from = now;
        }
        polled = now;
        let gate = check(w, now, quiet_from).unwrap_or_else(|e| GiveUp(format!("{e:#}")));
        // The machine can sleep during the check's herdr calls, so a pass is
        // judged again on the time it ended at.
        let checked = clock.now();
        if matches!(gate, Gate::Go) && paused(now, checked) {
            continue;
        }
        match gate {
            Gate::Stop => return Ok(Waited::Stopped),
            Gate::Cancel(why) => {
                set_state(w, AttemptState::Cancelled, Some(why))?;
                return Ok(Waited::Stopped);
            }
            Gate::GiveUp(why) => return give_up(w, &why, notify),
            Gate::Go | Gate::Wait(_) if checked >= deadline => {
                let why = match gate {
                    Gate::Wait(why) => why,
                    _ => "its conditions held only after that".into(),
                };
                let why = format!("waited {} minutes: {why}", TIMEOUT.as_secs() / 60);
                return give_up(w, &why, notify);
            }
            Gate::Go => {
                return match restart_now(w, clock, notify) {
                    Ok(waited) => Ok(waited),
                    Err(e) => give_up(w, &format!("{e:#}"), notify),
                };
            }
            Gate::Wait(_) => clock.sleep(POLL),
        }
    }
}

/// Whether more than [`GAP`] passed unseen between two reads of the clock.
fn paused(from: SystemTime, to: SystemTime) -> bool {
    to.duration_since(from).map_or(true, |gap| gap > GAP)
}

fn check(w: &Waiter, now: SystemTime, quiet_from: SystemTime) -> Result<Gate> {
    let Some((used, restart)) =
        shepherd_resume::update(w.state, w.place, |r| (r.used, r.restart.clone()))?
    else {
        return Ok(GiveUp("its handoff record was replaced".into()));
    };
    let Some(attempt) = restart
        .self_restart
        .filter(|a| a.state == AttemptState::Pending)
    else {
        return Ok(Gate::Stop);
    };
    if used {
        return Ok(Gate::Stop);
    }
    if !(w.enabled)()? {
        return Ok(Gate::Cancel(
            "[shepherd] self_restart was turned off".into(),
        ));
    }
    if !(w.alive)(&w.place.process) {
        return Ok(GiveUp(format!(
            "its Claude (pid {}) exited",
            w.place.process.pid
        )));
    }
    let armed = parse_time(&attempt.armed).ok_or_else(|| anyhow!("unreadable arm time"))?;
    let stop = restart
        .stop
        .filter(|s| parse_time(&s.at).is_some_and(|at| at >= armed));
    let Some(StopAt { stop, .. }) = stop else {
        return Ok(Gate::Wait("its turn hasn't stopped since it armed".into()));
    };
    if stop.error.is_some() || stop.pending().is_some() {
        return Ok(Gate::Wait(format!(
            "its last stop reads `{}`",
            stop.describe()
        )));
    }
    let pane = &w.place.pane;
    let Some(agent) = w.herdr.agents()?.into_iter().find(|a| a.pane_id == *pane) else {
        return Ok(GiveUp(format!("no agent runs in pane {pane}")));
    };
    if !matches!(agent.status, AgentStatus::Idle | AgentStatus::Done) {
        return Ok(Gate::Wait(format!(
            "herdr shows it {}",
            agent.status.as_str()
        )));
    }
    // Any prompt since arming cancelled it, so these are minutes without one.
    let quiet_until = armed.max(quiet_from) + WARNING;
    if now < quiet_until {
        let paused = match quiet_from > armed {
            true => ", counted again after a pause in its checks",
            false => "",
        };
        return Ok(Gate::Wait(format!(
            "the human's {} minutes{paused} run until {}",
            WARNING.as_secs() / 60,
            humantime::format_rfc3339_seconds(quiet_until)
        )));
    }
    let screen = w.herdr.visible_text(pane)?;
    if shows_subagent_view(&screen) {
        return Ok(Gate::Wait(format!(
            "its pane shows a subagent's view, {TEXT_REACHES_SUBAGENT}"
        )));
    }
    if !input_box_empty(&screen) {
        return Ok(Gate::Wait("its input box isn't empty".into()));
    }
    Ok(Gate::Go)
}

/// Marks the restart under way, unless a prompt cancelled it since the
/// check, then sends `/clear` and, once the resume marks the record used,
/// `go`; without the resume, the paste line naming the handoff.
fn restart_now(w: &Waiter, clock: &mut dyn Clock, notify: &mut dyn FnMut(&str)) -> Result<Waited> {
    let started = shepherd_resume::update(w.state, w.place, |r| {
        let Some(attempt) = r.restart.self_restart.as_mut() else {
            return false;
        };
        if attempt.state != AttemptState::Pending || r.used {
            return false;
        }
        attempt.state = AttemptState::Restarting;
        true
    })?;
    if started != Some(true) {
        return Ok(Waited::Stopped);
    }
    let pane = &w.place.pane;
    w.herdr.prompt(pane, "/clear")?;
    let cleared = time(clock.now());
    shepherd_resume::update(w.state, w.place, |r| {
        r.restart.last_self_restart = Some(cleared)
    })?;
    let deadline = clock.now() + RESUME_WAIT;
    let resumed = loop {
        let used = shepherd_resume::update(w.state, w.place, |r| r.used)?;
        if used == Some(true) {
            break true;
        }
        if clock.now() >= deadline {
            break false;
        }
        clock.sleep(Duration::from_secs(1));
    };
    let handoff = context::handoff_path(w.state, context::SHEPHERD);
    let paste = format!(
        "continue as the loopherd shepherd from {}",
        handoff.display()
    );
    // The human may have started typing in the cleared session; a send would join it.
    if !input_box_empty(&w.herdr.visible_text(pane)?) {
        if !resumed {
            let why = format!(
                "the human was typing after the /clear and no resume followed it, so it sent \
                 nothing; paste `{paste}`"
            );
            return give_up(w, &why, notify);
        }
        let why = "the human was typing after the /clear, so it sent nothing more";
        set_state(w, AttemptState::Restarted, Some(why.into()))?;
        return Ok(Waited::Restarted);
    }
    let (line, waited, why) = match resumed {
        true => ("go", Waited::Restarted, None),
        false => (
            paste.as_str(),
            Waited::Fallback,
            Some("no resume followed the /clear, so it sent the paste line".to_string()),
        ),
    };
    w.herdr.prompt(pane, line)?;
    set_state(w, AttemptState::Restarted, why)?;
    Ok(waited)
}

/// Records why the self-restart gave up, sends it to `notify` and puts it
/// on the pane's label.
fn give_up(w: &Waiter, why: &str, notify: &mut dyn FnMut(&str)) -> Result<Waited> {
    let recorded = set_state(w, AttemptState::GaveUp, Some(why.to_string()));
    notify(&format!("self-restart gave up: {why}"));
    let label = format!("shepherd: self-restart gave up: {why}");
    let labelled = pane_label::label_until_prompted(w.herdr, w.mailboxes, &w.place.pane, &label);
    recorded.and(labelled)?;
    Ok(Waited::GaveUp(why.to_string()))
}

/// Ends a waiter that was refused the claim `restart --self` hands over.
/// Gives up as [`give_up`] does, unless a waiter holds that claim: its
/// pending restart is left alone, and only `notify` is told.
pub fn refused(w: &Waiter, why: &str, notify: &mut dyn FnMut(&str)) -> Result<()> {
    if restart::take(w.state, context::SHEPHERD, "waiter")?.is_some() {
        give_up(w, why, notify)?;
    } else {
        notify(&format!(
            "a self-restart waiter was refused: {why}; the one already waiting carries on"
        ));
    }
    Ok(())
}

fn set_state(w: &Waiter, state: AttemptState, why: Option<String>) -> Result<()> {
    shepherd_resume::update(w.state, w.place, |r| {
        if let Some(attempt) = r.restart.self_restart.as_mut() {
            attempt.state = state;
            attempt.why = why;
        }
    })?;
    Ok(())
}

/// For a UserPromptSubmit at `place`: cancels a pending self-restart,
/// returning the context that tells the shepherd so. A pending one whose
/// waiter died gives up, telling `notify` and the shepherd. A prompt Claude
/// made from a finished background task changes nothing.
pub fn prompted(
    state: &Path,
    place: &Place,
    input: &[u8],
    notify: &mut dyn FnMut(&str),
) -> Result<Option<String>> {
    let input: Option<serde_json::Value> = serde_json::from_slice(input).ok();
    let prompt = input.as_ref().and_then(|i| i["prompt"].as_str());
    if prompt.is_some_and(pane_label::is_task_notification) {
        return Ok(None);
    }
    // A free claim means no waiter runs: it never started, or died.
    let waiter_runs = restart::take(state, context::SHEPHERD, "waiter")?.is_none();
    let ended = shepherd_resume::update(state, place, |r| {
        let attempt = r.restart.self_restart.as_mut();
        let attempt = attempt.filter(|a| a.state == AttemptState::Pending && !r.used)?;
        let (state, why, told) = match waiter_runs {
            true => (
                AttemptState::Cancelled,
                "a prompt came in its pane",
                CANCELLED,
            ),
            false => (AttemptState::GaveUp, WAITER_GONE, WAITER_DIED),
        };
        attempt.state = state;
        attempt.why = Some(why.into());
        Some((state, told))
    })?;
    let Some((state, told)) = ended.flatten() else {
        return Ok(None);
    };
    if state == AttemptState::GaveUp {
        notify(&format!("self-restart gave up: {WAITER_GONE}"));
    }
    Ok(Some(format!("{told} {NEXT}")))
}

const WAITER_GONE: &str = "its waiter wasn't running";
const CANCELLED: &str = "Your self-restart is cancelled: this prompt came while it waited.";
const WAITER_DIED: &str =
    "Your self-restart had given up: its waiter wasn't running, so nothing restarted you.";
const NEXT: &str = "Your handoff is still written. Once you've answered, tell the human to run \
    `/clear` and send `go` when they're ready, or ask whether to re-arm it; on a yes, run \
    `loopherd restart --self` again, which waits another 5 minutes without a prompt here.";

fn time(at: SystemTime) -> String {
    humantime::format_rfc3339_nanos(at).to_string()
}

fn parse_time(text: &str) -> Option<SystemTime> {
    humantime::parse_rfc3339(text).ok()
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::rc::Rc;

    use serde_json::{Value, json};

    use super::*;
    use crate::herdr::fake::{FAKE_SOCKET, FakeHerdr};
    use crate::subagent_view::screens::MAIN_VIEW;

    const PANE: &str = "w1:p1";
    const RECORDED: &str = "2026-11-19T10:00:00Z";
    const HANDOFF: &str = "2026-11-19T10:01:00Z";
    const ARMED: &str = "2026-11-19T10:02:00Z";

    fn at(time: &str) -> SystemTime {
        humantime::parse_rfc3339(time).unwrap()
    }

    fn place() -> Place {
        Place {
            pane: PANE.into(),
            socket: FAKE_SOCKET.into(),
            process: Process {
                pid: 30,
                start: 300,
            },
        }
    }

    /// A clock at `start` whose every sleep first runs `step` with the time it
    /// wakes at; the sleep numbered by `gap` lasts its duration longer, and
    /// `suspended` is time the machine slept outside any sleep.
    struct Steps<'a> {
        now: SystemTime,
        suspended: Rc<Cell<Duration>>,
        sleeps: u32,
        gap: Option<(u32, Duration)>,
        step: Box<dyn FnMut(SystemTime) + 'a>,
    }

    impl Clock for Steps<'_> {
        fn elapsed(&self) -> Duration {
            unreachable!()
        }
        fn sleep(&mut self, duration: Duration) {
            self.sleeps += 1;
            assert!(self.sleeps < 2000, "never woke");
            self.now += duration;
            if let Some((_, gap)) = self.gap.filter(|(at, _)| *at == self.sleeps) {
                self.now += gap;
            }
            let woke = self.now();
            (self.step)(woke);
        }
        fn now(&self) -> SystemTime {
            self.now + self.suspended.get()
        }
    }

    fn clock_at<'a>(start: &str, step: impl FnMut(SystemTime) + 'a) -> Steps<'a> {
        Steps {
            now: at(start),
            suspended: Rc::default(),
            sleeps: 0,
            gap: None,
            step: Box::new(step),
        }
    }

    struct World {
        state: tempfile::TempDir,
        herdr: FakeHerdr,
        alive: Cell<bool>,
        enabled: Cell<bool>,
        /// How long the machine sleeps during the waiter's next check.
        suspend_in_check: Cell<Option<Duration>>,
        suspended: Rc<Cell<Duration>>,
        notified: RefCell<Vec<String>>,
        /// The claims of the waiters armed, held as a running waiter holds its own.
        claims: RefCell<Vec<Lock>>,
    }

    impl World {
        /// A shepherd that wrote its handoff after its handoff line, idle in
        /// [`PANE`] with an empty input box, with the switch on and the hook in.
        fn new() -> World {
            let state = tempfile::tempdir().unwrap();
            shepherd_resume::record(state.path(), &place(), None, "s-old", at(RECORDED)).unwrap();
            let handoff = context::handoff_path(state.path(), context::SHEPHERD);
            fs::create_dir_all(handoff.parent().unwrap()).unwrap();
            fs::write(&handoff, "## Next steps\n").unwrap();
            fs::File::options()
                .write(true)
                .open(&handoff)
                .unwrap()
                .set_modified(at(HANDOFF))
                .unwrap();
            let herdr = FakeHerdr::new(&[("w1", "dev")]).with_live_agent(
                Some("shepherd"),
                Some("s-old"),
                PANE,
            );
            herdr.set_screen(PANE, MAIN_VIEW);
            World {
                state,
                herdr,
                alive: Cell::new(true),
                enabled: Cell::new(true),
                suspend_in_check: Cell::default(),
                suspended: Rc::default(),
                notified: RefCell::default(),
                claims: RefCell::default(),
            }
        }

        fn path(&self) -> &Path {
            self.state.path()
        }

        fn arm_at(&self, now: &str, switches: Switches) -> Result<Armed> {
            let mut notify = |text: &str| self.notified.borrow_mut().push(text.into());
            arm(
                self.path(),
                &place(),
                &switches,
                &mut clock_at(now, |_| {}),
                &mut notify,
            )
        }

        fn arm(&self) -> String {
            let armed = self.arm_at(ARMED, on()).unwrap();
            self.claims.borrow_mut().push(armed.waiter);
            armed.message
        }

        fn stop(&self, now: SystemTime, shells: usize) {
            let tasks: Vec<Value> = (0..shells).map(|_| json!({"type": "shell"})).collect();
            let input =
                json!({"hook_event_name": "Stop", "background_tasks": tasks, "session_crons": []});
            crate::stop_hook::shepherd_stopped(
                self.path(),
                &place(),
                input.to_string().as_bytes(),
                now,
            )
            .unwrap();
        }

        fn prompt(&self, prompt: &str) -> Option<String> {
            let input = json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt});
            let mut notify = |text: &str| self.notified.borrow_mut().push(text.into());
            prompted(
                self.path(),
                &place(),
                input.to_string().as_bytes(),
                &mut notify,
            )
            .unwrap()
        }

        /// What the SessionStart hook does on the `/clear` the waiter sends.
        fn resume_on_clear(&self) {
            let cleared = self
                .herdr
                .prompts()
                .iter()
                .any(|(_, text)| text == "/clear");
            if cleared {
                let input = br#"{"source":"clear","session_id":"s-new"}"#;
                shepherd_resume::session_started(self.path(), &place(), input, &|s| s.into())
                    .unwrap();
            }
        }

        /// Runs the waiter from [`ARMED`], calling `step` at each wake.
        fn wait(&self, step: impl FnMut(SystemTime)) -> Waited {
            self.wait_on(clock_at(ARMED, step))
        }

        fn wait_on(&self, mut clock: Steps) -> Waited {
            clock.suspended = self.suspended.clone();
            self.with_waiter(|waiter, notify| wait(waiter, &mut clock, notify).unwrap())
        }

        fn with_waiter<T>(&self, run: impl FnOnce(&Waiter, &mut dyn FnMut(&str)) -> T) -> T {
            let alive = |_: &Process| self.alive.get();
            let enabled = || {
                if let Some(slept) = self.suspend_in_check.take() {
                    self.suspended.set(self.suspended.get() + slept);
                }
                Ok(self.enabled.get())
            };
            let mailboxes = self.path().join("reports");
            let waiter = Waiter {
                herdr: &self.herdr,
                state: self.path(),
                mailboxes: &mailboxes,
                place: &place(),
                alive: &alive,
                enabled: &enabled,
            };
            let mut notify = |text: &str| self.notified.borrow_mut().push(text.into());
            run(&waiter, &mut notify)
        }

        fn saved(&self) -> Value {
            serde_json::from_str(&fs::read_to_string(self.path().join("shepherd.json")).unwrap())
                .unwrap()
        }

        fn sent(&self) -> Vec<String> {
            self.herdr.prompts().into_iter().map(|(_, t)| t).collect()
        }
    }

    fn on() -> Switches {
        Switches {
            enabled: true,
            prompt_hook: true,
        }
    }

    fn minutes(n: u64) -> Duration {
        Duration::from_secs(n * 60)
    }

    #[test]
    fn an_armed_shepherd_restarts_with_clear_then_go_once_its_turn_ends_and_the_pane_is_quiet() {
        let w = World::new();
        let message = w.arm();
        assert!(message.contains("any prompt here stops it"), "{message}");
        assert_eq!(w.saved()["self_restart"]["state"], "pending");
        w.stop(at(ARMED) + Duration::from_secs(5), 0);

        let mut sent_at = None;
        let waited = w.wait(|now| {
            if sent_at.is_none() && !w.herdr.prompts().is_empty() {
                sent_at = Some(now);
            }
            w.resume_on_clear();
        });

        assert_eq!(waited, Waited::Restarted);
        assert_eq!(w.sent(), ["/clear", "go"]);
        assert!(sent_at.unwrap() >= at(ARMED) + WARNING);
        let saved = w.saved();
        assert_eq!(saved["self_restart"]["state"], "restarted");
        assert_eq!(saved["used"], true);
        assert!(saved["last_self_restart"].is_string(), "{saved}");
        assert!(w.path().join("handoffs/shepherd.prev.md").is_file());
        assert!(w.notified.borrow().is_empty());
    }

    #[test]
    fn the_resumed_session_is_told_it_restarted_itself() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let told = RefCell::new(None);
        w.wait(|_| {
            let cleared = w.herdr.prompts().iter().any(|(_, t)| t == "/clear");
            if cleared && told.borrow().is_none() {
                let input = br#"{"source":"clear","session_id":"s-new"}"#;
                *told.borrow_mut() =
                    shepherd_resume::session_started(w.path(), &place(), input, &|s| s.into())
                        .unwrap();
            }
        });
        let told = told.into_inner().unwrap();
        assert!(told.contains("You restarted yourself"), "{told}");
    }

    #[test]
    fn with_no_resume_after_the_clear_it_sends_the_paste_line() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);

        assert_eq!(w.wait(|_| {}), Waited::Fallback);

        let handoff = w.path().join("handoffs/shepherd.md");
        let line = format!(
            "continue as the loopherd shepherd from {}",
            handoff.display()
        );
        assert_eq!(w.sent(), ["/clear".to_string(), line]);
        let saved = w.saved();
        assert_eq!(saved["self_restart"]["state"], "restarted");
        assert!(
            saved["self_restart"]["why"]
                .as_str()
                .unwrap()
                .contains("paste line")
        );
    }

    /// Runs the waiter while `hold` keeps one condition false until `until`,
    /// returning when `/clear` went out.
    fn cleared_at(w: &World, mut hold: impl FnMut(SystemTime, &World)) -> SystemTime {
        let mut sent_at = None;
        w.wait(|now| {
            hold(now, w);
            if sent_at.is_none() && !w.herdr.prompts().is_empty() {
                sent_at = Some(now);
            }
            w.resume_on_clear();
        });
        sent_at.expect("never cleared")
    }

    #[test]
    fn it_waits_for_a_finished_stop_since_it_armed() {
        let w = World::new();
        w.stop(at(ARMED) - Duration::from_secs(30), 0);
        w.arm();
        let late = at(ARMED) + minutes(9);

        let sent = cleared_at(&w, |now, w| {
            if now == late {
                w.stop(now, 0);
            }
        });

        assert!(sent >= late, "{sent:?}");
    }

    #[test]
    fn it_waits_out_a_stop_with_work_in_flight() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 1);
        let late = at(ARMED) + minutes(8);

        let sent = cleared_at(&w, |now, w| {
            if now == late {
                w.stop(now, 0);
            }
        });

        assert!(sent >= late, "{sent:?}");
    }

    #[test]
    fn it_waits_while_herdr_shows_the_pane_working() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        w.herdr.set_status("shepherd", AgentStatus::Working);
        let late = at(ARMED) + minutes(7);

        let sent = cleared_at(&w, |now, w| {
            if now == late {
                w.herdr.set_status("shepherd", AgentStatus::Done);
            }
        });

        assert!(sent >= late, "{sent:?}");
    }

    #[test]
    fn it_waits_while_text_is_typed_in_the_input_box() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        w.herdr
            .set_screen(PANE, &MAIN_VIEW.replace("❯ \n", "❯ half a thou\n"));
        let late = at(ARMED) + minutes(11);

        let sent = cleared_at(&w, |now, w| {
            if now == late {
                w.herdr.set_screen(PANE, MAIN_VIEW);
            }
        });

        assert!(sent >= late, "{sent:?}");
    }

    #[test]
    fn a_prompt_while_it_waits_cancels_it_and_tells_the_shepherd() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let typed = at(ARMED) + minutes(2);
        let told = RefCell::new(None);

        let waited = w.wait(|now| {
            if now == typed {
                *told.borrow_mut() = w.prompt("wait, one more thing");
            }
        });

        assert_eq!(waited, Waited::Stopped);
        assert!(w.sent().is_empty());
        let told = told.into_inner().unwrap();
        for part in [
            "cancelled",
            "`/clear`",
            "`go`",
            "re-arm",
            "loopherd restart --self",
        ] {
            assert!(told.contains(part), "{part}: {told}");
        }
        let saved = w.saved();
        assert_eq!(saved["self_restart"]["state"], "cancelled");
        assert!(saved.get("last_self_restart").is_none(), "{saved}");
    }

    #[test]
    fn a_task_notification_doesnt_cancel_it() {
        let w = World::new();
        w.arm();
        let notification = "<task-notification>watch exited</task-notification>";
        assert_eq!(w.prompt(notification), None);
        assert_eq!(w.saved()["self_restart"]["state"], "pending");
    }

    #[test]
    fn a_cancelled_restart_can_be_re_armed_and_doesnt_count_toward_the_guard() {
        let w = World::new();
        w.arm();
        w.prompt("hold on");
        w.claims.borrow_mut().clear();

        let again = "2026-11-19T10:04:00Z";
        w.arm_at(again, on()).unwrap();

        assert_eq!(w.saved()["self_restart"]["state"], "pending");
        assert_eq!(
            w.saved()["self_restart"]["armed"],
            "2026-11-19T10:04:00.000000000Z"
        );
        assert!(w.notified.borrow().is_empty());
    }

    #[test]
    fn a_second_arm_is_refused_while_a_waiter_holds_the_claim() {
        let w = World::new();
        w.arm();
        let e = w.arm_at(ARMED, on()).err().unwrap();
        assert!(format!("{e:#}").contains("already waiting"), "{e:#}");
    }

    #[test]
    fn arming_is_refused_with_the_reason_and_nothing_pending() {
        let off = Switches {
            enabled: false,
            prompt_hook: true,
        };
        let no_hook = Switches {
            enabled: true,
            prompt_hook: false,
        };
        for (switches, why) in [
            (off, "self_restart is off"),
            (no_hook, "UserPromptSubmit hook isn't installed"),
        ] {
            let w = World::new();
            let e = w.arm_at(ARMED, switches).err().unwrap();
            let e = format!("{e:#}");
            assert!(e.contains(why), "{e}");
            assert!(e.contains("`/clear` and `go`"), "{e}");
            assert!(w.saved().get("self_restart").is_none());
        }

        let w = World::new();
        fs::remove_file(w.path().join("handoffs/shepherd.md")).unwrap();
        let e = w.arm_at(ARMED, on()).err().unwrap();
        assert!(format!("{e:#}").contains("no handoff was written"), "{e:#}");
        assert!(w.saved().get("self_restart").is_none());

        let w = World::new();
        let elsewhere = Place {
            pane: "w1:p2".into(),
            ..place()
        };
        let mut notify = |_: &str| {};
        let e = arm(
            w.path(),
            &elsewhere,
            &on(),
            &mut clock_at(ARMED, |_| {}),
            &mut notify,
        )
        .err()
        .unwrap();
        assert!(format!("{e:#}").contains("no handoff of yours"), "{e:#}");
    }

    #[test]
    fn arming_within_30_minutes_of_the_last_self_restart_is_refused_and_notified() {
        let w = World::new();
        shepherd_resume::update(w.path(), &place(), |r| {
            r.restart.last_self_restart = Some("2026-11-19T09:40:00Z".into())
        })
        .unwrap();

        let e = w.arm_at(ARMED, on()).err().unwrap();

        assert!(format!("{e:#}").contains("09:40:00"), "{e:#}");
        assert_eq!(w.notified.borrow().len(), 1);
        assert!(w.notified.borrow()[0].starts_with("self-restart refused: "));
        assert!(w.saved().get("self_restart").is_none());

        w.arm_at("2026-11-19T10:10:01Z", on()).unwrap();
    }

    #[test]
    fn the_last_self_restart_carries_into_the_next_handoff_record() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        w.wait(|_| w.resume_on_clear());
        let last = w.saved()["last_self_restart"].clone();

        shepherd_resume::record(
            w.path(),
            &place(),
            None,
            "s-new",
            at("2026-11-19T10:20:00Z"),
        )
        .unwrap();

        assert_eq!(w.saved()["last_self_restart"], last);
        assert!(w.saved().get("self_restart").is_none());
    }

    fn assert_gave_up(w: &World, waited: Waited, why: &str) {
        let Waited::GaveUp(said) = waited else {
            panic!("{waited:?}");
        };
        assert!(said.contains(why), "{said}");
        assert!(w.sent().is_empty(), "{:?}", w.sent());
        let saved = w.saved();
        assert_eq!(saved["self_restart"]["state"], "gave_up");
        assert_eq!(saved["self_restart"]["why"], said.as_str());
        assert_eq!(
            *w.notified.borrow(),
            [format!("self-restart gave up: {said}")]
        );
        assert_eq!(
            w.herdr.label(PANE),
            Some(format!("shepherd: self-restart gave up: {said}"))
        );
        assert!(w.path().join("handoffs/shepherd.md").is_file());
    }

    #[test]
    fn it_gives_up_after_30_minutes_naming_what_it_waited_on() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 1);

        let waited = w.wait(|_| {});

        assert_gave_up(
            &w,
            waited,
            "waited 30 minutes: its last stop reads `paused: 1 shell`",
        );
    }

    #[test]
    fn it_gives_up_when_the_pane_or_its_claude_is_gone() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let waited = w.wait(|_| w.herdr.remove_agent("shepherd"));
        assert_gave_up(&w, waited, "no agent runs in pane w1:p1");

        let w = World::new();
        w.arm();
        let waited = w.wait(|_| w.alive.set(false));
        assert_gave_up(&w, waited, "its Claude (pid 30) exited");
    }

    #[test]
    fn it_gives_up_when_sending_fails() {
        let mut w = World::new();
        w.herdr.fail_prompt = true;
        w.arm();
        w.stop(at(ARMED), 0);

        let waited = w.wait(|_| {});

        let Waited::GaveUp(why) = waited else {
            panic!("{waited:?}");
        };
        assert!(why.contains("prompt refused"), "{why}");
        assert_eq!(w.saved()["self_restart"]["state"], "gave_up");
        assert!(w.saved().get("last_self_restart").is_none());
        w.claims.borrow_mut().clear();
        w.arm_at("2026-11-19T10:10:00Z", on()).unwrap();
    }

    #[test]
    fn a_label_it_gave_up_with_clears_on_the_humans_next_prompt() {
        let w = World::new();
        w.arm();
        w.wait(|_| w.alive.set(false));
        assert!(w.herdr.label(PANE).is_some());

        let input = br#"{"prompt":"ok, I'll do it"}"#;
        pane_label::prompted(&w.herdr, &w.path().join("reports"), PANE, input).unwrap();

        assert_eq!(w.herdr.label(PANE), None);
    }

    #[test]
    fn a_resume_by_hand_while_it_waits_ends_it_quietly() {
        let w = World::new();
        w.arm();
        let waited = w.wait(|_| {
            let input = br#"{"source":"clear","session_id":"s-new"}"#;
            shepherd_resume::session_started(w.path(), &place(), input, &|s| s.into()).unwrap();
        });
        assert_eq!(waited, Waited::Stopped);
        assert!(w.sent().is_empty());
        assert!(w.notified.borrow().is_empty());
    }

    #[test]
    fn it_waits_while_the_pane_shows_a_subagents_view() {
        use crate::subagent_view::screens::SUBAGENT_PICKER;
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        w.herdr.set_screen(PANE, SUBAGENT_PICKER);
        let late = at(ARMED) + minutes(12);

        let sent = cleared_at(&w, |now, w| {
            if now == late {
                w.herdr.set_screen(PANE, MAIN_VIEW);
            }
        });

        assert!(sent >= late, "{sent:?}");
    }

    #[test]
    fn it_sends_no_go_into_text_the_human_typed_after_the_clear() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        w.herdr
            .set_screen_on_prompt(PANE, &MAIN_VIEW.replace("❯ \n", "❯ what happ\n"));

        let waited = w.wait(|_| w.resume_on_clear());

        assert_eq!(waited, Waited::Restarted);
        assert_eq!(w.sent(), ["/clear"]);
        let why = w.saved()["self_restart"]["why"].clone();
        assert!(why.as_str().unwrap().contains("typing"), "{why}");
    }

    #[test]
    fn a_prompt_with_no_waiter_running_gives_up_and_says_so() {
        let w = World::new();
        w.arm();
        w.claims.borrow_mut().clear();

        let told = w.prompt("hello").unwrap();

        assert!(told.contains("waiter wasn't running"), "{told}");
        assert!(told.contains("loopherd restart --self"), "{told}");
        let saved = w.saved();
        assert_eq!(saved["self_restart"]["state"], "gave_up");
        assert_eq!(saved["self_restart"]["why"], "its waiter wasn't running");
        assert_eq!(
            *w.notified.borrow(),
            ["self-restart gave up: its waiter wasn't running"]
        );
    }

    #[test]
    fn a_waiter_that_wakes_past_its_30_minutes_gives_up_instead_of_clearing() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let slept = Steps {
            gap: Some((1, Duration::from_secs(8 * 60 * 60))),
            ..clock_at(ARMED, |_| {})
        };

        let waited = w.wait_on(slept);

        assert_gave_up(&w, waited, "waited 30 minutes: the human's 5 minutes");
    }

    #[test]
    fn a_pause_in_its_checks_starts_the_humans_5_minutes_again() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let mut sent_at = None;
        let paused = Steps {
            gap: Some((1, minutes(10))),
            ..clock_at(ARMED, |now| {
                if sent_at.is_none() && !w.herdr.prompts().is_empty() {
                    sent_at = Some(now);
                }
                w.resume_on_clear();
            })
        };

        assert_eq!(w.wait_on(paused), Waited::Restarted);

        let woke = at(ARMED) + POLL + minutes(10);
        assert!(sent_at.unwrap() >= woke + WARNING, "{sent_at:?}");
    }

    #[test]
    fn a_suspend_during_the_check_that_passes_starts_the_humans_5_minutes_again() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let holds = at(ARMED) + WARNING;
        let mut sent_at = None;

        let waited = w.wait(|now| {
            if now == holds {
                w.suspend_in_check.set(Some(minutes(10)));
            }
            if sent_at.is_none() && !w.herdr.prompts().is_empty() {
                sent_at = Some(now);
            }
            w.resume_on_clear();
        });

        assert_eq!(waited, Waited::Restarted);
        let woke = holds + minutes(10);
        assert!(sent_at.unwrap() >= woke + WARNING, "{sent_at:?}");
    }

    #[test]
    fn a_suspend_past_its_30_minutes_during_the_check_that_passes_gives_up() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let holds = at(ARMED) + WARNING;

        let waited = w.wait(|now| {
            if now == holds {
                w.suspend_in_check
                    .set(Some(Duration::from_secs(8 * 60 * 60)));
            }
        });

        assert_gave_up(&w, waited, "waited 30 minutes: the human's 5 minutes");
    }

    #[test]
    fn a_check_that_passes_only_as_its_30_minutes_run_out_gives_up() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 1);
        let last_poll = at(ARMED) + TIMEOUT - POLL;

        let waited = w.wait(|now| {
            if now == last_poll {
                w.stop(now, 0);
                w.suspend_in_check.set(Some(Duration::from_secs(20)));
            }
        });

        assert_gave_up(
            &w,
            waited,
            "waited 30 minutes: its conditions held only after that",
        );
    }

    #[test]
    fn a_wait_refused_its_claim_leaves_a_running_waiters_restart_pending() {
        let w = World::new();
        w.arm();

        w.with_waiter(|waiter, notify| refused(waiter, "no claim", notify).unwrap());

        assert_eq!(w.saved()["self_restart"]["state"], "pending");
        assert_eq!(w.herdr.label(PANE), None);
        assert_eq!(
            *w.notified.borrow(),
            ["a self-restart waiter was refused: no claim; the one already waiting carries on"]
        );
    }

    #[test]
    fn a_wait_refused_its_claim_gives_up_when_no_waiter_runs() {
        let w = World::new();
        w.arm();
        w.claims.borrow_mut().clear();

        w.with_waiter(|waiter, notify| refused(waiter, "no claim", notify).unwrap());

        assert_gave_up(&w, Waited::GaveUp("no claim".into()), "no claim");
    }

    #[test]
    fn turning_the_switch_off_stops_a_waiting_restart() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        let off = at(ARMED) + minutes(1);

        let waited = w.wait(|now| {
            if now >= off {
                w.enabled.set(false);
            }
        });

        assert_eq!(waited, Waited::Stopped);
        assert!(w.sent().is_empty());
        let saved = w.saved();
        assert_eq!(saved["self_restart"]["state"], "cancelled");
        assert_eq!(
            saved["self_restart"]["why"],
            "[shepherd] self_restart was turned off"
        );
        assert!(w.notified.borrow().is_empty());
    }

    #[test]
    fn typing_after_a_clear_no_resume_followed_gives_up_naming_the_paste_line() {
        let w = World::new();
        w.arm();
        w.stop(at(ARMED), 0);
        w.herdr
            .set_screen_on_prompt(PANE, &MAIN_VIEW.replace("❯ \n", "❯ what happ\n"));

        let waited = w.wait(|_| {});

        let Waited::GaveUp(why) = waited else {
            panic!("{waited:?}");
        };
        assert!(
            why.contains("continue as the loopherd shepherd from"),
            "{why}"
        );
        assert_eq!(w.sent(), ["/clear"]);
        assert_eq!(w.saved()["self_restart"]["state"], "gave_up");
        assert!(w.saved()["last_self_restart"].is_string());
        assert_eq!(
            *w.notified.borrow(),
            [format!("self-restart gave up: {why}")]
        );
    }
}
