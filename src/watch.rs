//! `loopherd watch`: block until a member needs the shepherd. Stops come
//! from the Stop hook's mailbox entries; herdr's `blocked` and `gone` from
//! entries the herdr plugin's event hook or this watch's poll records.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};

use crate::context::ShepherdHandoff;
use crate::feed::{claimable, claimable_count, run_check, run_in_repo, run_source};
use crate::github::Merges;
use crate::herd::{self, MemberCheck, MemberChecks, Paths, holds_slot, lead_checks};
use crate::herd_view::{Member, Options, View};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::{AfterMerge, AgentRecord, Ledger, Process};
use crate::lock::Lock;
use crate::merge_alert::MergeAlerts;
use crate::notify::{self, Waiting};
use crate::pane_event::{record_blocked, record_gone_after, retracted};
use crate::pause;
use crate::process::{Procs, live};
use crate::report::{
    Body, Filter, Mailbox, Pending, Stop, member_of_own_pane, peek_reports, unread_wakes,
};
use crate::status;

/// The kinds of line a watch prints after a member's name, besides report kinds.
pub mod line {
    pub const PAUSED: &str = "paused";
    pub const FINISHED: &str = "finished";
    pub const FAILED: &str = "failed";
    pub const MERGED: &str = "merged";
    pub const CLAIMABLE: &str = "claimable";
    pub const INTERRUPTED: &str = "interrupted";
    pub const BLOCKED: &str = "blocked";
    pub const GONE: &str = "gone";
    /// After a herd's name, not a member's.
    pub const FAILING: &str = "failing";
    pub const RECOVERED: &str = "recovered";
    /// After a herd's name.
    pub const RUNNING_LOW: &str = "running low";
    /// After a herd's name, or `all` on the shepherd's watch.
    pub const DRAINED: &str = "drained";
    /// After a herd's name.
    pub const OUT_OF_WORK: &str = "out of work";
    #[cfg(test)]
    pub const ALL: [&str; 13] = [
        PAUSED,
        FINISHED,
        FAILED,
        MERGED,
        CLAIMABLE,
        INTERRUPTED,
        BLOCKED,
        GONE,
        FAILING,
        RECOVERED,
        RUNNING_LOW,
        DRAINED,
        OUT_OF_WORK,
    ];
}

pub trait Clock {
    /// Time since the watch started.
    fn elapsed(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
    /// The wall-clock time.
    fn now(&self) -> SystemTime;
}

pub struct RealClock(Instant);

impl RealClock {
    pub fn start() -> RealClock {
        RealClock(Instant::now())
    }
}

impl Clock for RealClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Takes on a lead's identity for its watch, given the lead's name.
pub type AsLead<'a> = dyn Fn(&str) -> Result<()> + 'a;

pub struct Request<'a> {
    /// Only members of these herds; empty is every member.
    pub herds: &'a [String],
    pub timeout: Option<Duration>,
    pub poll: Duration,
    /// The caller's pane and herdr socket, which make it a lead's watch
    /// when they match a lead as `loopherd report` matches.
    pub own_pane: Option<&'a str>,
    /// The caller and its ancestors, which carry a moved member's old pane id.
    pub own_lineage: &'a [Process],
    pub own_socket: Option<&'a Path>,
    /// The lead whose sandboxed Claude is an ancestor of this watch, which
    /// may then be only that lead's own.
    pub caller: Option<&'a str>,
    /// Run with the lead's name once this is known to be a lead's watch,
    /// before its checks and before any thread starts; an error stops the watch.
    pub as_lead: Option<&'a AsLead<'a>>,
    /// Where a lead's watch finds its herd file for its merged and
    /// claimable checks; `None` runs no checks.
    pub herd_files: Option<&'a Paths>,
    /// How long each check's command may run. The shepherd's watch reads it
    /// only for its unled herds' checks, and warns rather than fails on it.
    pub check_timeout: fn() -> Result<Duration>,
    /// Measures the caller's Claude for the shepherd's own handoff line; run
    /// as the watch exits, only when it is the shepherd's.
    pub shepherd_handoff: Option<&'a dyn Fn() -> Option<ShepherdHandoff>>,
    /// Tells a lead's watch whether the feed running a worker's after_merge
    /// still runs.
    pub procs: &'a dyn Procs,
    /// Given a lead's own questions and blocks as its watch first sees them,
    /// which wake only the shepherd's watch.
    pub notify_own: Option<NotifyOwn<'a>>,
    /// Asked by a lead's watch, through `notify_own`, for merges the human
    /// should hear of (see [`herd::merge_alerts`]).
    pub merges: Option<&'a dyn Merges>,
    /// Labels the caller's pane with what wakes the shepherd's watch waiting
    /// on the human, until the human next prompts there.
    pub pane_label: bool,
}

pub type NotifyOwn<'a> = &'a dyn Fn(&[Waiting]);

/// How often a lead's watch runs its herd's checks, and the shepherd's its unled herds'.
const CHECK_EVERY: Duration = Duration::from_secs(60);

/// What woke the watch, one rendered line each. Nothing is marked read until
/// [`Wake::mark_read`], so a wake lost before it's printed comes back.
#[must_use = "what woke it stays unread until mark_read"]
pub struct Wake {
    pub lines: Vec<String>,
    /// What among `lines` waits on the human.
    pub waiting: Vec<Waiting>,
    pending: Vec<Pending>,
    fired: Option<Fired>,
    handoff: Option<ShepherdHandoff>,
    /// Held until marked read, so an overlapping watch can't print it too.
    _only: Lock,
}

impl Wake {
    pub fn mark_read(self, warn: &mut dyn FnMut(String)) -> Result<()> {
        self.pending.into_iter().try_for_each(Pending::save)?;
        self.fired.map_or(Ok(()), Fired::save)?;
        self.handoff.map_or(Ok(()), |h| h.mark(warn))
    }
}

/// Polls until a member needs the shepherd and returns what woke it; `None`
/// when `req.timeout` passes first. Calls `warn` as herdr becomes unreadable
/// and for each bad mailbox line. Refuses to run beside another watch on
/// `dir`.
pub fn watch(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    dir: &Path,
    req: &Request,
    clock: &mut dyn Clock,
    warn: &mut dyn FnMut(&str),
) -> Result<Option<Wake>> {
    let reader = reader(ledger, req, warn)?;
    if let (Reader::Lead { name, .. }, Some(as_lead)) = (&reader, req.as_lead) {
        as_lead(name)?;
    }
    let only = only_watch(dir, &reader)?;
    let mut checks = match (&reader, req.herd_files) {
        (
            Reader::Lead {
                name,
                herd,
                created,
            },
            Some(paths),
        ) => Some(Checks {
            herd: herd.clone(),
            alerts: MergeAlerts::new(herd, name, created, dir),
            paths,
            procs: req.procs,
            timeout: (req.check_timeout)()?,
            fired: Fired::load(dir, name, created)?,
            next: Duration::ZERO,
            merges_due: false,
            problem: None,
            unreadable_pause: None,
        }),
        _ => None,
    };
    let mut unled = match (&reader, req.herd_files) {
        (Reader::Shepherd, Some(paths)) => Some(Unled {
            paths,
            dir,
            herds: req.herds,
            check_timeout: req.check_timeout,
            fired: Fired::shepherds(dir)?,
            next: Duration::ZERO,
            bad_timeout: None,
            failing: HashSet::new(),
            unlisted: None,
        }),
        _ => None,
    };
    // Each member's last known status; an unreadable poll keeps it.
    let mut last: HashMap<String, AgentStatus> = HashMap::new();
    let mut gone = Gone::default();
    // Unlike `gone`, kept while herdr is unreadable: a member seen live and
    // now gone counts as in flight, though a restart may keep `gone` quiet.
    let mut seen_live: HashSet<String> = HashSet::new();
    let mut code = 0;
    let mut own_questions_seen = HashSet::new();
    // An unreadable mailbox warns every poll until it's readable; say it once.
    let mut warned: HashSet<String> = HashSet::new();
    // A gone is recorded again every poll until it's written; say each failure once.
    let mut unrecorded: HashSet<String> = HashSet::new();
    loop {
        let before = last.clone();
        let marks = notify::BlockMarks::read(dir)?;
        let read = status::read_statuses(
            herdr,
            ledger,
            Options {
                all: false,
                strays: true,
            },
        )?;
        if read.code != code {
            if let Some(problem) = &read.problem {
                warn(problem);
            }
            code = read.code;
        }
        let agents_read = read.code == 0;
        let occupied: HashSet<String> = read
            .view
            .strays
            .agents
            .iter()
            .flatten()
            .map(|stray| stray.pane.clone())
            .collect();
        let (members, own) = in_scope(read.view, req, &reader)?;
        let names: Vec<String> = members.iter().map(|m| m.name.clone()).collect();
        // The human is in an attended member's pane: only its `gone` wakes the watch.
        let attended: HashSet<&str> = members
            .iter()
            .filter(|m| m.attended)
            .map(|m| m.name.as_str())
            .collect();
        let reported: Vec<String> = names
            .iter()
            .filter(|n| !attended.contains(n.as_str()))
            .cloned()
            .collect();
        let mut lines = vec![];
        let mut waiting = vec![];
        let mut pending = vec![];
        let mut wakes = vec![];
        if !agents_read {
            gone = Gone::default();
        }
        for member in &members {
            if member.gone {
                if let Some(seen_live) = gone.confirm(member, &occupied)
                    && let Err(e) =
                        record_gone_after(dir, &member.name, SystemTime::now(), seen_live)
                {
                    let warning = format!("warning: record {} gone: {e:#}", member.name);
                    if unrecorded.insert(warning.clone()) {
                        warn(&warning);
                    }
                }
                continue;
            }
            gone.seen(member, agents_read);
            if agents_read {
                seen_live.insert(member.name.clone());
            }
            let was = last.get(&member.name).copied();
            // Only on change: one blocked at start would re-wake every re-armed watch.
            if member.status == AgentStatus::Blocked
                && was.is_some_and(|w| w != AgentStatus::Blocked)
                && let Err(e) = record_blocked(dir, &member.name, SystemTime::now())
            {
                let warning = format!("warning: record {} blocked: {e:#}", member.name);
                if unrecorded.insert(warning.clone()) {
                    warn(&warning);
                }
            }
            if member.status != AgentStatus::Unknown {
                last.insert(member.name.clone(), member.status);
            }
        }
        // An empty filter means every mailbox.
        if !names.is_empty() {
            // Stops first: a stop is appended after its report, so any stop
            // read here has its report visible to the read after it.
            let wakes_read;
            (wakes, wakes_read) = unread_wakes(dir, &names)?;
            pending.push(wakes_read);
        }
        if !reported.is_empty() {
            let (read, reports_read) = peek_reports(
                ledger,
                dir,
                &Filter {
                    names: &reported,
                    herd: None,
                    all: false,
                    since: None,
                    tail: None,
                },
            )?;
            for w in read.warnings.iter().filter(|w| !warned.contains(*w)) {
                warn(&format!("warning: {w}"));
            }
            warned = read.warnings.into_iter().collect();
            lines.extend(read.lines);
            waiting.extend(read.entries.into_iter().filter_map(|(member, entry)| {
                let Body::Question { text } = entry.body else {
                    return None;
                };
                Some(Waiting {
                    herd: None,
                    member,
                    kind: notify::Kind::Question,
                    text,
                    at: Some(entry.at),
                })
            }));
            pending.push(reports_read);
        }
        let stopped = wakes.iter().filter(|m| !attended.contains(m.name.as_str()));
        lines.extend(stopped.flat_map(stop_wakes));
        let mut fired = None;
        if let Some(checks) = checks.as_mut().filter(|c| clock.elapsed() >= c.next) {
            let events = checks.run(ledger, warn)?;
            checks.merges_due = true;
            checks.next = clock.elapsed() + CHECK_EVERY;
            if !events.is_empty() {
                lines.extend(events.iter().map(|e| e.line.clone()));
                fired = Some(checks.fired.with(&events));
            }
        }
        for mailbox in &wakes {
            let quiet = attended.contains(mailbox.name.as_str());
            herdr_wakes(mailbox, quiet, &mut lines, &mut waiting);
        }
        // Not on a poll already waking it, so its commands don't hold those lines back.
        if lines.is_empty()
            && let Some(unled) = unled.as_mut().filter(|u| clock.elapsed() >= u.next)
        {
            let events = unled.run(ledger, warn)?;
            unled.next = clock.elapsed() + CHECK_EVERY;
            if !events.is_empty() {
                lines.extend(events.iter().map(|e| e.line.clone()));
                fired = Some(unled.fired.with(&events));
            }
        }
        if let (Some(notify_own), Some(own), Reader::Lead { herd, .. }) =
            (req.notify_own, &own, &reader)
        {
            let mut events = own_questions(ledger, dir, &own.name, &mut own_questions_seen)?;
            let was = last.get(&own.name).copied();
            if own.status == AgentStatus::Blocked && was.is_some_and(|w| w != AgentStatus::Blocked)
            {
                events.push(Waiting {
                    herd: None,
                    member: own.name.clone(),
                    kind: notify::Kind::Blocked,
                    text: String::new(),
                    at: None,
                });
            }
            if own.status != AgentStatus::Unknown {
                last.insert(own.name.clone(), own.status);
            }
            if !events.is_empty() {
                events.iter_mut().for_each(|e| e.herd = Some(herd.clone()));
                notify_own(&events);
            }
        }
        for member in members.iter().chain(&own) {
            if !matches!(member.status, AgentStatus::Blocked | AgentStatus::Unknown) {
                marks.unblocked(dir, &member.name)?;
            }
        }
        if lines.is_empty() {
            let flight = Flight {
                dir,
                seen_live: &seen_live,
                before: &before,
            };
            lines.extend(drained(ledger, &flight, req, &reader, &members)?);
        }
        if let (Some(notify_own), Some(merges), Some(checks)) = (
            req.notify_own,
            req.merges,
            checks.as_mut().filter(|c| c.merges_due),
        ) {
            checks.merges_due = false;
            let waking = !lines.is_empty();
            let merged = (checks.alerts).poll(checks.paths, merges, clock.now(), waking, warn);
            if !merged.is_empty() {
                notify_own(&merged);
            }
        }
        let left = req.timeout.map(|t| t.saturating_sub(clock.elapsed()));
        let exiting = !lines.is_empty() || left == Some(Duration::ZERO);
        let handoff = exiting
            .then(|| shepherd_handoff(ledger, req, &reader))
            .flatten();
        lines.extend(handoff.iter().map(|h| h.line.clone()));
        if !lines.is_empty() {
            if !waiting.is_empty() {
                let entries = ledger.entries()?;
                for event in &mut waiting {
                    event.herd = entries.get(&event.member).and_then(|e| e.herd.clone());
                }
            }
            // A lead relays its workers' questions as its own reports, which label its pane.
            if let (true, Reader::Shepherd, Some(pane), Some(first)) =
                (req.pane_label, &reader, req.own_pane, waiting.first())
                && member_of_own_pane(&ledger.entries()?, pane, req.own_lineage, req.own_socket)
                    .is_none()
                && let Err(e) = crate::pane_label::set(herdr, dir, pane, first)
            {
                warn(&format!("warning: label pane {pane}: {e:#}"));
            }
            return Ok(Some(Wake {
                lines,
                waiting,
                pending,
                fired,
                handoff,
                _only: only,
            }));
        }
        // Nothing to print, so nothing to lose.
        pending.into_iter().try_for_each(Pending::save)?;
        if left == Some(Duration::ZERO) {
            return Ok(None);
        }
        clock.sleep(left.map_or(req.poll, |left| left.min(req.poll)));
    }
}

/// The line ending a watch over herds paused with nothing left in flight:
/// a lead's herd has no open worker; the shepherd reads at least one herd,
/// each drained, and no member it reads has work in flight. Pauses are read
/// now, so a resume since the members were read counts.
fn drained(
    ledger: &dyn Ledger,
    flight: &Flight,
    req: &Request,
    reader: &Reader,
    members: &[Member],
) -> Result<Option<String>> {
    let Some(paths) = req.herd_files else {
        return Ok(None);
    };
    let paused = |herd: &str| matches!(pause::read(&paths.paused, herd), pause::Reading::Paused(_));
    if let Reader::Lead { herd, .. } = reader {
        return Ok(
            (members.is_empty() && paused(herd)).then(|| format!("{herd} {}", line::DRAINED))
        );
    }
    let entries = ledger.entries()?;
    let open_worker = |herd: &str| {
        entries
            .values()
            .any(|e| !e.retired && !e.is_lead() && e.herd.as_deref() == Some(herd))
    };
    let mut herds = 0;
    for member in members {
        match entries.get(&member.name).and_then(|e| e.herd.as_deref()) {
            None => {}
            Some(herd) if member.lead && paused(herd) && !open_worker(herd) => herds += 1,
            Some(_) => return Ok(None),
        }
    }
    // Mailboxes last: most polls end above without reading them.
    let drained = herds > 0 && !members.iter().any(|m| flight.carries(m));
    Ok(drained.then(|| format!("all {}", line::DRAINED)))
}

/// What a watch has seen that says whether a member may still work.
struct Flight<'a> {
    dir: &'a Path,
    seen_live: &'a HashSet<String>,
    /// Statuses as of the poll before, since herdr can show a member idle
    /// before its Stop hook has written the stop.
    before: &'a HashMap<String, AgentStatus>,
}

impl Flight<'_> {
    /// Whether `member` may still work: gone after this watch saw it live,
    /// busy, blocked or unknown on this poll or the
    /// last, or its last stop since its start failed or left work in
    /// flight. An unreadable mailbox counts as in flight.
    fn carries(&self, member: &Member) -> bool {
        let busy = |status| {
            matches!(
                status,
                AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Unknown
            )
        };
        if member.gone {
            return self.seen_live.contains(&member.name);
        }
        if busy(member.status) || self.before.get(&member.name).copied().is_some_and(busy) {
            return true;
        }
        let Ok((entries, _)) = crate::report::history(self.dir, &member.name) else {
            return true;
        };
        let mut pending = false;
        for entry in entries {
            match entry.body {
                Body::Start { .. } => pending = false,
                Body::Stop(stop) => pending = stop.error.is_some() || stop.pending().is_some(),
                _ => {}
            }
        }
        pending
    }
}

/// A lead's merged and claimable checks, run every [`CHECK_EVERY`].
struct Checks<'a> {
    herd: String,
    alerts: MergeAlerts,
    paths: &'a Paths,
    procs: &'a dyn Procs,
    timeout: Duration,
    fired: Fired,
    /// When the next run is due, as time since the watch started.
    next: Duration,
    /// Whether a run asked for a merge lookup not yet made.
    merges_due: bool,
    /// Why the herd file last failed to load, warned once.
    problem: Option<String>,
    /// Why the herd's pause file was last unreadable, warned once.
    unreadable_pause: Option<String>,
}

impl Checks<'_> {
    /// Each event not fired before. A check, the source or the backlog failing starts a
    /// streak, one `failing` event, and its next pass a `recovered` one; one
    /// that runs out of the round's [`CHECK_EVERY`] is no event. A paused
    /// herd has no claimable events (D23).
    fn run(&mut self, ledger: &dyn Ledger, warn: &mut dyn FnMut(&str)) -> Result<Vec<Event>> {
        let loaded = lead_checks(self.paths, &self.herd);
        let problem = loaded
            .as_ref()
            .err()
            .map(|e| format!("{}'s checks: {e:#}", self.herd));
        warn_once(&mut self.problem, problem, warn);
        let Ok((checks, queue)) = loaded else {
            return Ok(vec![]);
        };
        let budget = Budget {
            deadline: Instant::now() + CHECK_EVERY,
            each: self.timeout,
        };
        let workers = ledger.entries()?.into_values().filter(|e| {
            !e.retired && !e.may_be_lead() && e.herd.as_deref() == Some(self.herd.as_str())
        });
        let after_merge = queue.as_ref().is_some_and(|q| !q.after_merge.is_empty());
        let mut events = vec![];
        let mut failing = vec![];
        let mut passing = vec![];
        let mut freed = 0;
        let mut low = None;
        let mut out = None;
        for worker in workers {
            let done = run_member_check(&checks.done, &checks, &worker, &budget);
            let merged = run_member_check(&checks.merged, &checks, &worker, &budget);
            for (kind, check) in [("done", &done), ("merged", &merged)] {
                let what = format!("{} {kind}", worker.name);
                match check {
                    Some(Ok(_)) => passing.push(what),
                    Some(Err(Unchecked::Failed(why))) => failing.push((what, why.clone())),
                    Some(Err(Unchecked::OutOfTime)) | None => {}
                }
            }
            let due = after_merge && worker.after_merge.is_none();
            // Feed keeps, done or not, a member whose after_merge is due or unnoted.
            let kept = match worker.after_merge.as_ref() {
                Some(&AfterMerge::Pending { owner }) => {
                    if live(self.procs, Some(owner)).is_none()
                        && still_pending(ledger, &worker.name, owner)?
                    {
                        events.push(Event::new(format!("{} {}", worker.name, line::INTERRUPTED)));
                    }
                    true
                }
                Some(AfterMerge::Noted) => false,
                Some(AfterMerge::Unknown(_)) => true,
                None => due && matches!(merged, Some(Ok(true) | Err(_))),
            };
            freed += usize::from(holds_slot(&worker) && done == Some(Ok(true)) && !kept);
            // An attended member's stops wake nothing, so its merge must, done or not.
            let announced = due || worker.attended.is_some();
            if merged == Some(Ok(true)) && (announced || matches!(done, None | Some(Ok(false)))) {
                events.push(Event::new(format!("{} {}", worker.name, line::MERGED)));
            }
        }
        // Read last, so a pause during the member checks counts.
        let reading = pause::read(&self.paths.paused, &self.herd);
        let warning = reading.warning(&self.herd);
        warn_once(&mut self.unreadable_pause, warning, warn);
        let paused = reading.pause().is_some();
        // After the pause, so a resume during the member checks counts now.
        self.fired.reload()?;
        if let (false, Some(queue), Some(timeout)) = (paused, queue, budget.left()) {
            match run_source(&queue, timeout) {
                Ok((tasks, _)) => {
                    passing.push("source".into());
                    let names = claimable(ledger, self.paths, &queue, &self.herd, freed, &tasks)?;
                    let claimable =
                        claimable_count(ledger, self.paths, &queue, &self.herd, &tasks)?;
                    // Read again: a feed may have opened a worker while the source ran.
                    let entries = ledger.entries()?;
                    let no_worker = !open(&entries, &self.herd, |e| !e.is_lead());
                    if queue.backlog.is_none() {
                        low = self.running_low(&entries, claimable, 0, queue.max)?;
                        out = (self.fired).out_of_work(
                            &self.herd,
                            &entries,
                            claimable == 0 && no_worker,
                        )?;
                    }
                    if let Some(backlog) = &queue.backlog
                        && let Some(timeout) = budget.left()
                    {
                        match run_in_repo(backlog, &queue.repo, timeout) {
                            Ok((unready, _)) => {
                                passing.push("backlog".into());
                                let unready = unready.lines().filter(|l| !l.trim().is_empty());
                                let unready = unready.count();
                                low = self.running_low(&entries, claimable, unready, queue.max)?;
                                out = self.fired.out_of_work(
                                    &self.herd,
                                    &entries,
                                    claimable == 0 && unready == 0 && no_worker,
                                )?;
                            }
                            Err(_) if budget.cut(timeout) => {}
                            Err(e) => {
                                failing.push(("backlog".into(), why(backlog, &format!("{e:#}"))))
                            }
                        }
                    }
                    // A running feed may be spawning these before it records them.
                    if !feeding(self.paths)? {
                        events.extend(
                            names
                                .into_iter()
                                .map(|name| Event::new(format!("{name} {}", line::CLAIMABLE))),
                        );
                    }
                }
                Err(_) if budget.cut(timeout) => {}
                Err(e) => failing.push(("source".into(), why(&queue.source, &format!("{e:#}")))),
            }
        }
        let herd = &self.herd;
        let failed = failing.into_iter().map(|(what, why)| Event {
            key: format!("{herd} {} {what}", line::FAILING),
            line: format!("{herd} {}  {what} {why}", line::FAILING),
            covers: None,
        });
        let recovered = passing
            .into_iter()
            .filter(|what| {
                let key = format!("{herd} {} {what}", line::FAILING);
                self.fired.keys.contains(&key)
            })
            .map(|what| Event {
                key: format!("{herd} {} {what}", line::RECOVERED),
                line: format!("{herd} {}  {what}", line::RECOVERED),
                covers: None,
            });
        Ok(failed
            .chain(recovered)
            .chain(low)
            .chain(out)
            .chain(events)
            .filter(|e| !self.fired.keys.contains(&e.key))
            .collect())
    }
}

impl Checks<'_> {
    /// The `running low` event while claimable is at or below `max` with
    /// unready tasks waiting. Its key leaves out the counts, so it prints once
    /// per streak; a round that isn't low ends the streak. A herd with no work
    /// and no open worker has a streak of its own, keyed like `out of work`,
    /// whose line stands in for the plain one.
    fn running_low(
        &mut self,
        entries: &BTreeMap<String, AgentRecord>,
        claimable: usize,
        unready: usize,
        max: usize,
    ) -> Result<Option<Event>> {
        let herd = &self.herd;
        let key = format!("{herd} {}", line::RUNNING_LOW);
        let counts = format!("{key}  {claimable} claimable, {unready} unready");
        let no_worker = !open(entries, herd, |e| !e.is_lead());
        let starved = self.fired.since_newest_member(
            format!("{key}, {NO_WORKERS}"),
            format!("{counts}, {NO_WORKERS}"),
            herd,
            entries,
            claimable == 0 && unready > 0 && no_worker,
        )?;
        let low = (self.fired).streak(key, counts, claimable <= max && unready > 0)?;
        match (starved, low) {
            (Some(starved), Some(low)) => Ok(Some(Event {
                covers: Some(low.key),
                ..starved
            })),
            (starved, low) => Ok(starved.or(low)),
        }
    }
}

/// The tail of a `running low` line for a herd with no open worker.
const NO_WORKERS: &str = "no workers open";

/// The shepherd's check, every [`CHECK_EVERY`], of each herd it reads that
/// has a herd file and no open lead: whether it is out of work.
struct Unled<'a> {
    paths: &'a Paths,
    /// Where the leads' fired files are.
    dir: &'a Path,
    /// Only these herds; empty is every herd.
    herds: &'a [String],
    check_timeout: fn() -> Result<Duration>,
    fired: Fired,
    next: Duration,
    /// Why the check timeout was last unreadable, warned once.
    bad_timeout: Option<String>,
    /// Herds whose check last failed, warned once a streak.
    failing: HashSet<String>,
    /// Why the herd files last couldn't be listed, warned once.
    unlisted: Option<String>,
}

/// Whether `herd` has an open member that `is`.
fn open(
    entries: &BTreeMap<String, AgentRecord>,
    herd: &str,
    is: impl Fn(&AgentRecord) -> bool,
) -> bool {
    entries
        .values()
        .any(|e| !e.retired && e.herd.as_deref() == Some(herd) && is(e))
}

impl Unled<'_> {
    /// Each `out of work` event not fired before, nor by a retired lead of
    /// its herd. Any herd with a lead or worker open ends its streak, in
    /// scope or not. A paused herd keeps its streak, and one whose herd
    /// file, source or backlog fails or runs out of the round's time is no
    /// event; a failure warns once a streak.
    fn run(&mut self, ledger: &dyn Ledger, warn: &mut dyn FnMut(&str)) -> Result<Vec<Event>> {
        let names = herd::names(&self.paths.herds);
        let problem = (names.as_ref().err()).map(|e| format!("warning: list herd files: {e:#}"));
        warn_once(&mut self.unlisted, problem, warn);
        let Ok(names) = names else {
            return Ok(vec![]);
        };
        self.fired.reload()?;
        let entries = ledger.entries()?;
        let mut idle = vec![];
        for herd in names {
            if open(&entries, &herd, |_| true) {
                self.fired.out_of_work(&herd, &entries, false)?;
            } else if self.herds.is_empty() || self.herds.contains(&herd) {
                idle.push(herd);
            }
        }
        if idle.is_empty() {
            return Ok(vec![]);
        }
        let timeout = (self.check_timeout)();
        let problem = timeout.as_ref().err().map(|e| format!("warning: {e:#}"));
        warn_once(&mut self.bad_timeout, problem, warn);
        let Ok(each) = timeout else {
            return Ok(vec![]);
        };
        let budget = Budget {
            deadline: Instant::now() + CHECK_EVERY,
            each,
        };
        let mut events = vec![];
        for herd in idle {
            if pause::read(&self.paths.paused, &herd).pause().is_some() {
                continue;
            }
            let counted = (|| {
                let Some(queue) = lead_checks(self.paths, &herd)?.1 else {
                    return Ok(None);
                };
                let timeout = budget.left().context("out of time")?;
                let (tasks, _) =
                    run_source(&queue, timeout).with_context(|| why(&queue.source, "failed"))?;
                let claimable = claimable_count(ledger, self.paths, &queue, &herd, &tasks)?;
                let Some(backlog) = &queue.backlog else {
                    return Ok(Some((claimable, 0)));
                };
                let timeout = budget.left().context("out of time")?;
                let (unready, _) = run_in_repo(backlog, &queue.repo, timeout)
                    .with_context(|| why(backlog, "failed"))?;
                let unready = unready.lines().filter(|l| !l.trim().is_empty()).count();
                Ok::<_, anyhow::Error>(Some((claimable, unready)))
            })();
            let (claimable, unready) = match counted {
                Ok(Some(counts)) => counts,
                Ok(None) => continue,
                Err(e) => {
                    if budget.left().is_some() && self.failing.insert(herd.clone()) {
                        warn(&format!("warning: {herd}: {e:#}"));
                    }
                    continue;
                }
            };
            self.failing.remove(&herd);
            // Read again: a feed may have opened a worker while the commands ran.
            let entries = ledger.entries()?;
            let worker = open(&entries, &herd, |e| !e.is_lead());
            let out = claimable == 0 && unready == 0 && !worker;
            let Some(event) = self.fired.out_of_work(&herd, &entries, out)? else {
                continue;
            };
            let mut said = false;
            for lead in
                (entries.values()).filter(|e| e.is_lead() && e.herd.as_deref() == Some(&herd))
            {
                said |= Fired::lead_holds(self.dir, lead, &event.key)?;
            }
            if !said {
                events.push(event);
            }
        }
        Ok(events
            .into_iter()
            .filter(|e| !self.fired.keys.contains(&e.key))
            .collect())
    }
}

/// A line a watch's checks print, and the key in its fired file that stops
/// it printing again.
struct Event {
    key: String,
    line: String,
    /// Another key saved with this one, whose own line this one stands in for.
    covers: Option<String>,
}

impl Event {
    /// One keyed by its own line.
    fn new(line: String) -> Event {
        Event {
            key: line.clone(),
            line,
            covers: None,
        }
    }
}

/// `command` and the last line of `error`, which may start with `command` too.
fn why(command: &str, error: &str) -> String {
    let last = error.trim().lines().last().unwrap_or_default();
    let last = last.strip_prefix(&format!("{command:?} ")).unwrap_or(last);
    format!("{command:?}: {}", printable(last))
}

/// The most of a command's stderr line a watch line shows.
const WHY_MAX: usize = 200;

/// `text` without ANSI escapes or other control characters, cut to [`WHY_MAX`].
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.next_if_eq(&'[').is_some() {
            chars.find(|c| ('@'..='~').contains(c));
        } else if !c.is_control() {
            out.push(c);
        }
    }
    match out.char_indices().nth(WHY_MAX) {
        Some((at, _)) => format!("{}…", &out[..at]),
        None => out,
    }
}

/// Read fresh: the owning feed may have noted it during the member checks.
fn still_pending(ledger: &dyn Ledger, name: &str, owner: Process) -> Result<bool> {
    let entry = ledger.entries()?.remove(name);
    Ok(entry.and_then(|e| e.after_merge) == Some(AfterMerge::Pending { owner }))
}

/// Calls `warn` with `now` unless it was `last` too, and keeps it as `last`.
fn warn_once(last: &mut Option<String>, now: Option<String>, warn: &mut dyn FnMut(&str)) {
    if let Some(now) = now.as_deref().filter(|now| last.as_deref() != Some(now)) {
        warn(now);
    }
    *last = now;
}

/// Bounds one round of checks: each command gets `each`, cut to what's left
/// before `deadline`.
struct Budget {
    deadline: Instant,
    each: Duration,
}

impl Budget {
    fn left(&self) -> Option<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        (!left.is_zero()).then(|| left.min(self.each))
    }

    /// Whether a command given `timeout` and failing now was cut off by
    /// the deadline rather than failing in its own right.
    fn cut(&self, timeout: Duration) -> bool {
        timeout < self.each && self.left().is_none()
    }
}

/// Why a member check has no answer.
#[derive(PartialEq)]
enum Unchecked {
    /// The round's [`CHECK_EVERY`] ran out before or while it ran.
    OutOfTime,
    Failed(String),
}

/// `worker`'s check from `checks` for its template: `None` without one,
/// else whether it passed, or why it has no answer.
fn run_member_check(
    checks: &BTreeMap<String, MemberCheck>,
    herd: &MemberChecks,
    worker: &AgentRecord,
    budget: &Budget,
) -> Option<Result<bool, Unchecked>> {
    let check = checks.get(worker.template.as_ref()?)?;
    let Some(params) = &worker.params else {
        return Some(Err(Unchecked::Failed(why(
            check.text(),
            "no recorded params",
        ))));
    };
    let Some(timeout) = budget.left() else {
        return Some(Err(Unchecked::OutOfTime));
    };
    let command = match check.render(worker, params) {
        Ok(command) => command,
        Err(e) => return Some(Err(Unchecked::Failed(why(check.text(), &e.to_string())))),
    };
    Some(run_check(&command, &herd.repo, timeout).map_err(|e| {
        if budget.cut(timeout) {
            Unchecked::OutOfTime
        } else {
            Unchecked::Failed(why(&command, &e))
        }
    }))
}

/// Whether a feed holds the feed lock, probed without waiting.
fn feeding(paths: &Paths) -> Result<bool> {
    let path = &paths.feed_lock;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let free = Lock::try_take(file).with_context(|| format!("lock {}", path.display()))?;
    Ok(free.is_none())
}

/// The events a watch's checks have printed, one per line of
/// `watch-<lead>.fired` or the shepherd's `watch.fired`, so a re-armed watch
/// doesn't wake on them again. A lead's first line names its spawn, so a
/// re-created lead starts afresh.
/// A [`RESUMED`] line forgets the claimable keys above it, so a card announced
/// before a pause is announced again after resume, and a `recovered` key
/// forgets its `failing` one, so the next failure is announced again. A
/// `<key> ended` line forgets the `running low` or `out of work` streak keyed `<key>`.
struct Fired {
    path: PathBuf,
    header: String,
    keys: HashSet<String>,
    new: Vec<String>,
}

impl Fired {
    /// Called holding the lead's watch lock, so no other watch writes meanwhile.
    fn load(dir: &Path, lead: &str, created: &str) -> Result<Fired> {
        Fired::open(fired_path(dir, lead), format!("lead {created}"))
    }

    /// The shepherd's, called holding its watch lock.
    fn shepherds(dir: &Path) -> Result<Fired> {
        Fired::open(dir.join("watch.fired"), "shepherd".into())
    }

    fn open(path: PathBuf, header: String) -> Result<Fired> {
        let mut fired = Fired {
            path,
            header,
            keys: HashSet::new(),
            new: vec![],
        };
        fired.reload()?;
        Ok(fired)
    }

    /// Re-reads the keys, so a resume since the last read counts.
    fn reload(&mut self) -> Result<()> {
        let path = &self.path;
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        match keys_under(&text, &self.header) {
            Some(keys) => self.keys = keys,
            None => {
                self.keys.clear();
                fs::write(path, format!("{}\n", self.header))
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
        Ok(())
    }

    /// Whether the retired lead `lead`'s fired file holds `key`.
    fn lead_holds(dir: &Path, lead: &AgentRecord, key: &str) -> Result<bool> {
        let path = fired_path(dir, &lead.name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let header = format!("lead {}", lead.created);
        Ok(keys_under(&text, &header).is_some_and(|keys| keys.contains(key)))
    }

    /// The keys to append once `events` print.
    fn with(&self, events: &[Event]) -> Fired {
        Fired {
            path: self.path.clone(),
            header: self.header.clone(),
            keys: HashSet::new(),
            new: (events.iter())
                .flat_map(|e| [Some(&e.key), e.covers.as_ref()])
                .flatten()
                .cloned()
                .collect(),
        }
    }

    /// An event keyed `key` while `on`, printed once per streak since the
    /// caller drops fired keys; while off, ends the streak.
    fn streak(&mut self, key: String, line: String, on: bool) -> Result<Option<Event>> {
        if !on {
            self.end(&key)?;
            return Ok(None);
        }
        Ok(Some(Event {
            key,
            line,
            covers: None,
        }))
    }

    fn end(&mut self, key: &str) -> Result<()> {
        if self.keys.remove(key) {
            append(&self.path, &format!("{key}{ENDED}\n"))?;
        }
        Ok(())
    }

    fn out_of_work(
        &mut self,
        herd: &str,
        entries: &BTreeMap<String, AgentRecord>,
        out: bool,
    ) -> Result<Option<Event>> {
        let line = format!("{herd} {}", line::OUT_OF_WORK);
        self.since_newest_member(line.clone(), line, herd, entries, out)
    }

    /// A streak keyed `base` and the newest of `herd`'s members in `entries`,
    /// so a worker that opened and retired between watches starts a new one.
    fn since_newest_member(
        &mut self,
        base: String,
        line: String,
        herd: &str,
        entries: &BTreeMap<String, AgentRecord>,
        on: bool,
    ) -> Result<Option<Event>> {
        let key = since_newest_member(&base, herd, entries);
        let ended: Vec<String> = (self.keys.iter())
            .filter(|k| **k != key || !on)
            .filter(|k| *k == &base || k.starts_with(&format!("{base} since ")))
            .cloned()
            .collect();
        for old in ended {
            self.end(&old)?;
        }
        Ok(on.then_some(Event {
            key,
            line,
            covers: None,
        }))
    }

    /// Appends, so keys another watch saved meanwhile survive.
    fn save(self) -> Result<()> {
        let text: String = self.new.iter().map(|k| format!("{k}\n")).collect();
        append(&self.path, &text)
    }
}

/// The line `resume` appends to `watch-<lead>.fired`. Every key has a space
/// in it, so no key reads as this.
const RESUMED: &str = "resumed";

/// Ends the `running low` or `out of work` key it follows in a fired file.
const ENDED: &str = " ended";

fn fired_path(dir: &Path, lead: &str) -> PathBuf {
    dir.join(format!("watch-{lead}.fired"))
}

/// The keys a fired file's `text` holds, or `None` when it isn't headed `header`.
fn keys_under(text: &str, header: &str) -> Option<HashSet<String>> {
    let mut lines = text.lines();
    if lines.next() != Some(header) {
        return None;
    }
    let mut keys: HashSet<String> = HashSet::new();
    for line in lines {
        if line == RESUMED {
            keys.retain(|k| !is_claimable(k));
        } else if let Some(name) = line.strip_prefix(RELEASED) {
            keys.retain(|k| !is_about(k, name));
        } else if let Some(ended) = line.strip_suffix(ENDED) {
            keys.remove(ended);
        } else if let Some(failing) = failing_key(line) {
            keys.remove(&failing);
        } else {
            keys.insert(line.to_string());
        }
    }
    Some(keys)
}

fn since_newest_member(base: &str, herd: &str, entries: &BTreeMap<String, AgentRecord>) -> String {
    let newest = (entries.values())
        .filter(|e| e.herd.as_deref() == Some(herd))
        .map(|e| e.created.as_str())
        .max();
    match newest {
        Some(created) => format!("{base} since {created}"),
        None => base.to_string(),
    }
}

/// Makes `lead`'s watch announce its herd's claimable tasks again, as the
/// herd was resumed. A lead whose watch never fired has nothing to forget.
pub fn forget_claimable(dir: &Path, lead: &str) -> Result<()> {
    let path = fired_path(dir, lead);
    match OpenOptions::new().append(true).open(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        file => file
            .and_then(|mut file| file.write_all(format!("{RESUMED}\n").as_bytes()))
            .with_context(|| format!("write {}", path.display())),
    }
}

/// Starts the line `release` appends to `watch-<lead>.fired`, before the
/// released member's name.
const RELEASED: &str = "released: ";

/// Makes `lead`'s watch forget every event it printed about the released
/// member `name`, whose name feed may now spawn again. A lead whose watch
/// never fired has nothing to forget.
pub fn forget_member(dir: &Path, lead: &str, name: &str) -> Result<()> {
    let path = fired_path(dir, lead);
    match OpenOptions::new().append(true).open(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        file => file
            .and_then(|mut file| file.write_all(format!("{RELEASED}{name}\n").as_bytes()))
            .with_context(|| format!("write {}", path.display())),
    }
}

/// Whether `key` is an event about the member `name`: its own line, or its
/// herd's `failing` or `recovered` line for one of its checks.
fn is_about(key: &str, name: &str) -> bool {
    let names = |rest: &str| {
        rest.strip_prefix(name)
            .is_some_and(|after| after.starts_with(' '))
    };
    names(key)
        || key.split_once(' ').is_some_and(|(_, rest)| {
            [line::FAILING, line::RECOVERED].iter().any(|kind| {
                rest.strip_prefix(kind)
                    .and_then(|r| r.strip_prefix(' '))
                    .is_some_and(names)
            })
        })
}

/// The `failing` key a `recovered` key ends.
fn failing_key(key: &str) -> Option<String> {
    let (herd, rest) = key.split_once(' ')?;
    let what = rest.strip_prefix(line::RECOVERED)?.strip_prefix(' ')?;
    Some(format!("{herd} {} {what}", line::FAILING))
}

fn is_claimable(key: &str) -> bool {
    key.strip_suffix(line::CLAIMABLE)
        .is_some_and(|name| name.ends_with(' '))
}

fn append(path: &Path, text: &str) -> Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .with_context(|| format!("write {}", path.display()))
}

/// When a member herdr no longer matches counts as gone. herdr resumes
/// agents seconds after a restart, unnamed at first, so it takes a member
/// seen live since herdr was last unreadable, then two polls in a row with
/// no agent at all in its pane.
#[derive(Default)]
struct Gone {
    /// When each member was last seen live.
    alive: HashMap<String, SystemTime>,
    missing: HashSet<String>,
}

impl Gone {
    fn seen(&mut self, member: &Member, agents_read: bool) {
        self.missing.remove(&member.name);
        if agents_read {
            self.alive.insert(member.name.clone(), SystemTime::now());
        }
    }

    /// When `member` was last seen live, on the second poll in a row that
    /// it is really gone.
    fn confirm(&mut self, member: &Member, occupied: &HashSet<String>) -> Option<SystemTime> {
        let seen_live = self.alive.get(&member.name).copied();
        if seen_live.is_none() || occupied.contains(&member.pane) {
            self.missing.remove(&member.name);
            return None;
        }
        (!self.missing.insert(member.name.clone()))
            .then_some(seen_live)
            .flatten()
    }
}

/// Whose watch this is. Each member's events have exactly one reader: a
/// lead reads its herd's workers, the shepherd reads the rest.
enum Reader {
    Shepherd,
    Lead {
        name: String,
        herd: String,
        /// When this lead was spawned, telling it from an earlier one of the same name.
        created: String,
    },
}

/// A lead's when the caller's pane holds one, else the shepherd's. A lead
/// may only name its own herd.
fn reader(ledger: &dyn Ledger, req: &Request, warn: &mut dyn FnMut(&str)) -> Result<Reader> {
    let entries = ledger.entries()?;
    let member = req
        .own_pane
        .and_then(|pane| member_of_own_pane(&entries, pane, req.own_lineage, req.own_socket));
    let routed = member.filter(|entry| entry.is_lead());
    // Pane ids repeat across herdrs, but this may be a lead whose recorded socket is stale.
    let on_other_socket = || {
        let pane = req.own_pane?;
        let on_pane =
            |e: &&AgentRecord| !e.retired && e.is_lead() && e.is_own_pane(pane, req.own_lineage);
        entries.values().find(on_pane).map(|lead| {
            format!(
                "pane {pane} holds lead {} on socket {}, but this watch's socket is {}",
                lead.name,
                shown(lead.socket.as_deref()),
                shown(req.own_socket),
            )
        })
    };
    if let Some(caller) = req.caller {
        let why = |rest: String| {
            anyhow!("refused: {caller}'s Claude runs this watch, which may only be its own; {rest}")
        };
        match routed {
            None => {
                let retired = entries.get(caller).is_some_and(|e| e.retired);
                let pane = on_other_socket().unwrap_or_else(|| match retired {
                    true => format!("{caller} is retired in the ledger"),
                    false => format!("this pane isn't {caller}'s in the ledger"),
                });
                return Err(why(format!("{pane}, so it would be the shepherd's watch")));
            }
            Some(lead) if lead.name != caller => {
                return Err(why(format!("this pane is {}'s", lead.name)));
            }
            Some(_) => {}
        }
    }
    let lead = match (member, routed) {
        (_, Some(lead)) => lead,
        (Some(_), None) => return Ok(Reader::Shepherd),
        (None, None) => {
            if let Some(stale) = on_other_socket() {
                warn(&format!("warning: {stale}; running the shepherd's watch"));
            }
            return Ok(Reader::Shepherd);
        }
    };
    let name = lead.name.clone();
    let herd = lead
        .herd
        .clone()
        .with_context(|| format!("lead {name} has no herd"))?;
    if let Some(other) = req.herds.iter().find(|h| **h != herd) {
        bail!("{name} leads {herd}; its watch can't read {other}");
    }
    let created = lead.created.clone();
    Ok(Reader::Lead {
        name,
        herd,
        created,
    })
}

/// The shepherd's handoff line, from a shepherd's watch run from no member's
/// pane, measuring no member's session: members are told by their Stop hook.
fn shepherd_handoff(
    ledger: &dyn Ledger,
    req: &Request,
    reader: &Reader,
) -> Option<ShepherdHandoff> {
    if !matches!(reader, Reader::Shepherd) {
        return None;
    }
    let entries = ledger.entries().ok()?;
    if req.own_pane.is_some_and(|pane| {
        member_of_own_pane(&entries, pane, req.own_lineage, req.own_socket).is_some()
    }) {
        return None;
    }
    let handoff = (req.shepherd_handoff?)()?;
    let a_members =
        |e: &AgentRecord| !e.retired && e.session_id.as_deref() == Some(handoff.session.as_str());
    (!entries.values().any(a_members)).then_some(handoff)
}

fn shown(socket: Option<&Path>) -> String {
    socket.map_or("unset".into(), |s| s.display().to_string())
}

pub(crate) fn lead_lock_path(dir: &Path, lead: &str) -> PathBuf {
    dir.join(format!("watch-{lead}.lock"))
}

/// Whether a watch holds `lead`'s lock in `dir`, probed with the same
/// non-blocking take a watch uses. A missing lock file means none does.
pub fn lead_watch_running(dir: &Path, lead: &str) -> Result<bool> {
    let path = lead_lock_path(dir, lead);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let free = Lock::try_take(file).with_context(|| format!("lock {}", path.display()))?;
    Ok(free.is_none())
}

/// Holds the reader's lock in `dir`, with this process's PID in it, or
/// fails naming the lock and its holder while another watch has it.
fn only_watch(dir: &Path, reader: &Reader) -> Result<Lock> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = match reader {
        Reader::Shepherd => dir.join("watch.lock"),
        Reader::Lead { name, .. } => lead_lock_path(dir, name),
    };
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let Some(mut lock) =
        Lock::try_take(file).with_context(|| format!("lock {}", path.display()))?
    else {
        let holder = fs::read_to_string(&path).unwrap_or_default();
        let holder = match holder.trim() {
            "" => String::new(),
            pid => format!(" (pid {pid})"),
        };
        bail!("another watch{holder} holds {}", path.display());
    };
    lock.set_len(0)
        .and_then(|()| writeln!(lock, "{}", std::process::id()))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(lock)
}

/// A wake line for each unread stop that changes the member's state:
/// paused from not paused, finished, or failed. The rest of the mailbox
/// says what the state was and whether a report came since the last finish.
fn stop_wakes(mailbox: &Mailbox) -> Vec<String> {
    let mut paused = false;
    let mut reported = false;
    let mut wakes = vec![];
    for (i, entry) in mailbox.entries.iter().enumerate() {
        let wake = match &entry.body {
            Body::Start { .. } => {
                (paused, reported) = (false, false);
                None
            }
            Body::Done { .. } | Body::Question { .. } | Body::Handoff { .. } => {
                reported = true;
                None
            }
            Body::Note { .. }
            | Body::Blocked {}
            | Body::Gone { .. }
            | Body::Moved {}
            | Body::Usage { .. } => None,
            Body::Stop(Stop {
                error: Some(error), ..
            }) => {
                paused = false;
                Some(format!("{}  {error}", line::FAILED))
            }
            Body::Stop(stop) => match stop.pending() {
                Some(pending) => (!std::mem::replace(&mut paused, true))
                    .then(|| format!("{}  {pending}", line::PAUSED)),
                None => {
                    let note = if reported { "" } else { "  no report" };
                    (paused, reported) = (false, false);
                    Some(format!("{}{note}", line::FINISHED))
                }
            },
        };
        if let (true, Some(wake)) = (i >= mailbox.unread, wake) {
            wakes.push(format!("{} {wake}", mailbox.name));
        }
    }
    wakes
}

/// A wake line for each unread `gone` no `moved` retracts, and unless
/// `quiet`, one for any unread `blocked`, which also waits on the human.
fn herdr_wakes(
    mailbox: &Mailbox,
    quiet: bool,
    lines: &mut Vec<String>,
    waiting: &mut Vec<Waiting>,
) {
    let mut blocked = quiet;
    for (i, entry) in mailbox.entries.iter().enumerate().skip(mailbox.unread) {
        match entry.body {
            Body::Blocked {} if !std::mem::replace(&mut blocked, true) => {
                lines.push(format!("{} {}", mailbox.name, line::BLOCKED));
                waiting.push(Waiting {
                    herd: None,
                    member: mailbox.name.clone(),
                    kind: notify::Kind::Blocked,
                    text: String::new(),
                    at: None,
                });
            }
            Body::Gone { .. } if !retracted(&mailbox.entries[i + 1..]) => {
                lines.push(format!("{} {}", mailbox.name, line::GONE));
            }
            _ => {}
        }
    }
}

/// The view's non-retired members `reader` owns, narrowed to `req.herds`,
/// and for a lead's watch the lead itself. A herd with a lead, even a gone
/// one, is the lead's but for the lead itself. Fails once a lead reading is
/// no longer its herd's lead.
fn in_scope(view: View, req: &Request, reader: &Reader) -> Result<(Vec<Member>, Option<Member>)> {
    if let Reader::Lead { name, herd, .. } = reader {
        let own = view.herds.into_iter().find(|h| h.name == *herd);
        let (leads, workers): (Vec<_>, Vec<_>) = own
            .map(|h| h.members)
            .unwrap_or_default()
            .into_iter()
            .partition(|m| m.lead);
        let Some(lead) = leads.into_iter().find(|m| m.name == *name) else {
            bail!("{name} was retired, so its watch ends");
        };
        return Ok((workers, Some(lead)));
    }
    let mut members = vec![];
    for herd in view.herds {
        if req.herds.is_empty() || req.herds.contains(&herd.name) {
            let led = herd.members.iter().any(|m| m.lead);
            members.extend(herd.members.into_iter().filter(|m| m.lead || !led));
        }
    }
    if req.herds.is_empty() {
        members.extend(view.ungrouped);
    }
    Ok((members, None))
}

/// `lead`'s unread questions not in `seen`, adding them; its cursor is left
/// alone, as they are the shepherd's to read.
fn own_questions(
    ledger: &dyn Ledger,
    dir: &Path,
    lead: &str,
    seen: &mut HashSet<String>,
) -> Result<Vec<Waiting>> {
    let names = [lead.to_string()];
    let filter = Filter {
        names: &names,
        herd: None,
        all: false,
        since: None,
        tail: None,
    };
    let (read, _unsaved) = peek_reports(ledger, dir, &filter)?;
    Ok(read
        .entries
        .into_iter()
        .filter_map(|(member, entry)| match entry.body {
            Body::Question { text } if seen.insert(entry.at.clone()) => Some(Waiting {
                herd: None,
                member,
                kind: notify::Kind::Question,
                text,
                at: Some(entry.at),
            }),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::fake::FakeMerges;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{FileLedger, MemLedger};
    use crate::report::{Background, Entry, Kind, Reporter, Tally, Unknown, append, report};

    fn member(name: &str, pane: &str, herd: Option<&str>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
            session_id: None,
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
            outcome: None,
            released_at: None,
            herd: herd.map(Into::into),
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            attended: None,
            after_merge: None,
            unsandboxed: None,
            sockets_unblocked: None,
            earlier: vec![],
            unknown: Default::default(),
        }
    }

    fn ledger(members: Vec<AgentRecord>) -> MemLedger {
        MemLedger(
            members.into_iter().map(|m| (m.name.clone(), m)).collect(),
            vec![],
        )
    }

    fn send(ledger: &dyn Ledger, dir: &Path, name: &str, kind: Kind, text: &str) {
        let who = Reporter {
            own_pane: None,
            own_socket: None,
            as_name: Some(name),
            own_lineage: &[],
        };
        let at = humantime::parse_rfc3339("2026-11-01T09:00:00Z").unwrap();
        report(ledger, dir, &who, kind, text, at).unwrap();
    }

    /// Runs `step` with the poll count before each sleep, so a test changes
    /// the world between polls.
    struct Steps<F: FnMut(u32)> {
        start: SystemTime,
        elapsed: Duration,
        polls: u32,
        step: F,
    }

    impl<F: FnMut(u32)> Clock for Steps<F> {
        fn elapsed(&self) -> Duration {
            self.elapsed
        }
        fn now(&self) -> SystemTime {
            self.start + self.elapsed
        }
        fn sleep(&mut self, duration: Duration) {
            self.polls += 1;
            assert!(self.polls < 100, "watch never woke");
            (self.step)(self.polls);
            self.elapsed += duration;
        }
    }

    fn steps<F: FnMut(u32)>(step: F) -> Steps<F> {
        steps_from("2026-11-01T09:00:00Z", step)
    }

    /// Steps whose wall clock starts at `start`, an RFC 3339 time.
    fn steps_from<F: FnMut(u32)>(start: &str, step: F) -> Steps<F> {
        Steps {
            start: humantime::parse_rfc3339(start).unwrap(),
            elapsed: Duration::ZERO,
            polls: 0,
            step,
        }
    }

    const EVERY: Request = Request {
        caller: None,
        as_lead: None,
        procs: &crate::process::ProcFs,
        herds: &[],
        timeout: None,
        poll: Duration::from_secs(2),
        own_pane: None,
        own_socket: None,
        herd_files: None,
        check_timeout: || bail!("no check timeout in this test"),
        shepherd_handoff: None,
        notify_own: None,
        merges: None,
        pane_label: false,
        own_lineage: &[],
    };

    fn run(
        herdr: &FakeHerdr,
        ledger: &dyn Ledger,
        dir: &Path,
        req: &Request,
        clock: &mut dyn Clock,
    ) -> Option<Vec<String>> {
        let woke = watch(herdr, ledger, dir, req, clock, &mut |_| {}).unwrap();
        assert_eq!(herdr.mutations(), 0);
        woke.map(printed)
    }

    fn printed(wake: Wake) -> Vec<String> {
        let lines = wake.lines.clone();
        wake.mark_read(&mut |_| {}).unwrap();
        lines
    }

    #[test]
    fn an_unread_report_at_start_wakes_it_at_once_and_is_marked_read() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p9", None)]);
        let herdr = FakeHerdr::new(&[]).with_agent_in("lh-1", AgentStatus::Working);
        send(&ledger, dir.path(), "lh-1", Kind::Note, "starting");
        send(&ledger, dir.path(), "lh-1", Kind::Question, "which DB?");

        let mut clock = steps(|_| panic!("slept"));
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            [
                "lh-1 question 2026-11-01 09:00 UTC which DB?",
                "lh-1 note 2026-11-01 09:00 UTC starting",
            ]
        );

        let quiet = Request {
            timeout: Some(Duration::ZERO),
            ..EVERY
        };
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &quiet, &mut steps(|_| {})),
            None
        );
    }

    #[test]
    fn only_a_member_going_blocked_while_watched_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Working);
        herdr.set_status("lh-2", AgentStatus::Blocked);

        let mut clock = steps(|poll| match poll {
            1 | 2 => {}
            3 => herdr.set_status("lh-1", AgentStatus::Blocked),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 blocked"]
        );
    }

    fn waiting(wake: &Wake) -> Vec<(Option<&str>, &str, notify::Kind, &str, bool)> {
        wake.waiting
            .iter()
            .map(|w| {
                let herd = w.herd.as_deref();
                (
                    herd,
                    w.member.as_str(),
                    w.kind,
                    w.text.as_str(),
                    w.at.is_some(),
                )
            })
            .collect()
    }

    #[test]
    fn a_wake_hands_on_each_question_and_blocked_member_it_read_as_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", Some("cards")),
            member("solo", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("solo"), None, "w9:p2");
        herdr.set_status("solo", AgentStatus::Working);
        send(&ledger, dir.path(), "lh-1", Kind::Note, "starting");
        let mut clock = steps(|poll| match poll {
            1 => {
                send(&ledger, dir.path(), "lh-1", Kind::Question, "which DB?");
                herdr.set_status("solo", AgentStatus::Blocked);
            }
            _ => panic!("slept past the wake"),
        });
        let first = watch(
            &herdr,
            &ledger,
            dir.path(),
            &EVERY,
            &mut steps(|_| {}),
            &mut |_| {},
        );
        first.unwrap().unwrap().mark_read(&mut |_| {}).unwrap();

        let wake = watch(&herdr, &ledger, dir.path(), &EVERY, &mut clock, &mut |_| {});

        let wake = wake.unwrap().unwrap();
        assert_eq!(
            waiting(&wake),
            [
                (
                    Some("cards"),
                    "lh-1",
                    notify::Kind::Question,
                    "which DB?",
                    true
                ),
                (None, "solo", notify::Kind::Blocked, "", false),
            ]
        );
    }

    #[test]
    fn herds_narrow_the_watch_and_strays_and_retired_members_never_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut retired = member("old", "w9:p4", Some("cards"));
        retired.retired = true;
        let mut ledger = ledger(vec![
            member("lh-1", "w9:p1", Some("cards")),
            member("zpr-2", "w9:p2", Some("prs")),
            member("solo", "w9:p3", None),
            retired.clone(),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("zpr-2"), None, "w9:p2")
            .with_live_agent(Some("solo"), None, "w9:p3")
            .with_live_agent(Some("old"), None, "w9:p4")
            .with_live_agent(Some("stray"), None, "w9:p5");
        ledger.0.get_mut("old").unwrap().retired = false;
        send(&ledger, dir.path(), "old", Kind::Note, "bye");
        ledger.0.insert("old".into(), retired);
        send(&ledger, dir.path(), "solo", Kind::Note, "hi");
        let cards = ["cards".to_string()];
        let req = Request {
            herds: &cards,
            timeout: Some(Duration::from_secs(4)),
            ..EVERY
        };

        let mut clock = steps(|_| {
            herdr.set_status("zpr-2", AgentStatus::Blocked);
            herdr.set_status("stray", AgentStatus::Blocked);
        });
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);

        send(&ledger, dir.path(), "lh-1", Kind::Note, "PR #9");
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap(),
            ["lh-1 note 2026-11-01 09:00 UTC PR #9"]
        );
        let every = Request {
            timeout: Some(Duration::ZERO),
            ..EVERY
        };
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &every, &mut steps(|_| {})).unwrap(),
            ["solo note 2026-11-01 09:00 UTC hi"]
        );
    }

    #[test]
    fn unreachable_herdr_warns_once_and_reports_still_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let mut herdr = FakeHerdr::new(&[]);
        herdr.unreachable = true;
        let mut clock = steps(|poll| match poll {
            1 | 2 => {}
            3 => send(&ledger, dir.path(), "lh-1", Kind::Question, "which DB?"),
            _ => panic!("slept past the wake"),
        });
        let mut warnings = vec![];

        let woke = watch(&herdr, &ledger, dir.path(), &EVERY, &mut clock, &mut |w| {
            warnings.push(w.to_string())
        })
        .unwrap();

        assert_eq!(
            printed(woke.unwrap()),
            ["lh-1 question 2026-11-01 09:00 UTC which DB?"]
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("herdr unreachable"), "{warnings:?}");
    }

    #[test]
    fn an_unreadable_mailbox_warns_once_and_another_members_report_still_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[]);
        let unreadable = dir.path().join("lh-1/mailbox.jsonl");
        fs::create_dir_all(&unreadable).unwrap();
        let mut clock = steps(|poll| match poll {
            1 | 2 => {}
            3 => send(&ledger, dir.path(), "lh-2", Kind::Question, "which DB?"),
            _ => panic!("slept past the wake"),
        });
        let mut warnings = vec![];

        let woke = watch(&herdr, &ledger, dir.path(), &EVERY, &mut clock, &mut |w| {
            warnings.push(w.to_string())
        })
        .unwrap();

        assert_eq!(
            printed(woke.unwrap()),
            ["lh-2 question 2026-11-01 09:00 UTC which DB?"]
        );
        assert_eq!(
            warnings,
            [format!(
                "warning: could not read {}: Is a directory (os error 21)",
                unreadable.display()
            )]
        );
    }

    #[test]
    fn the_timeout_cuts_the_last_sleep_short() {
        let dir = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[]);
        let req = Request {
            timeout: Some(Duration::from_secs(3)),
            ..EVERY
        };
        let mut clock = steps(|_| {});

        assert_eq!(
            run(&herdr, &ledger(vec![]), dir.path(), &req, &mut clock),
            None
        );
        assert_eq!(clock.elapsed, Duration::from_secs(3));
    }

    #[test]
    fn a_member_whose_pane_disappears_while_watched_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Idle);

        let mut clock = steps(|poll| match poll {
            1 | 2 => {}
            3 => herdr.remove_agent("lh-1"),
            // Gone counts on the second poll in a row.
            4 => {}
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 gone"]
        );
    }

    #[test]
    fn an_attended_member_wakes_it_only_when_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut grill = member("grill-1", "w9:p1", None);
        grill.attended = Some(Default::default());
        let ledger = ledger(vec![grill]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("grill-1"), None, "w9:p1");
        herdr.set_status("grill-1", AgentStatus::Working);
        send(&ledger, dir.path(), "grill-1", Kind::Note, "starting");
        send(&ledger, dir.path(), "grill-1", Kind::Question, "which DB?");
        put(dir.path(), "grill-1", stop(0, 0));

        let mut clock = steps(|poll| match poll {
            1 | 2 => {}
            3 => herdr.set_status("grill-1", AgentStatus::Blocked),
            4 | 5 => {}
            6 => herdr.remove_agent("grill-1"),
            7 => {}
            _ => panic!("slept past the wake"),
        });
        let wake = watch(&herdr, &ledger, dir.path(), &EVERY, &mut clock, &mut |_| {})
            .unwrap()
            .unwrap();

        assert!(waiting(&wake).is_empty(), "{:?}", waiting(&wake));
        assert_eq!(printed(wake), ["grill-1 gone"]);
    }

    fn put(dir: &Path, name: &str, body: Body) {
        let at = humantime::parse_rfc3339("2026-11-01T09:00:00Z").unwrap();
        append(dir, name, &Entry::new(at, body)).unwrap();
    }

    fn stop(shells: u32, crons: u32) -> Body {
        Body::Stop(Stop {
            session: None,
            background: Tally::Known(Background {
                shells,
                ..Background::default()
            }),
            crons: Tally::Known(crons),
            error: None,
            context: None,
            limit: None,
        })
    }

    fn failed(error: &str) -> Body {
        Body::Stop(Stop {
            session: None,
            background: Tally::Unknown(Unknown::Unknown),
            crons: Tally::Unknown(Unknown::Unknown),
            error: Some(error.into()),
            context: None,
            limit: None,
        })
    }

    fn working_pair() -> (MemLedger, FakeHerdr) {
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Working);
        herdr.set_status("lh-2", AgentStatus::Working);
        (ledger, herdr)
    }

    const ONCE: Request = Request {
        caller: None,
        as_lead: None,
        procs: &crate::process::ProcFs,
        herds: &[],
        timeout: Some(Duration::ZERO),
        poll: Duration::from_secs(2),
        own_pane: None,
        own_socket: None,
        herd_files: None,
        check_timeout: || bail!("no check timeout in this test"),
        shepherd_handoff: None,
        notify_own: None,
        merges: None,
        pane_label: false,
        own_lineage: &[],
    };

    /// One poll of a fresh watch.
    fn once(herdr: &FakeHerdr, ledger: &MemLedger, dir: &Path) -> Option<Vec<String>> {
        run(herdr, ledger, dir, &ONCE, &mut steps(|_| {}))
    }

    #[test]
    fn a_stop_with_work_in_flight_wakes_it_paused_once() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", Body::Start { resumed: false });

        let mut clock = steps(|poll| match poll {
            1 => put(dir.path(), "lh-1", stop(1, 1)),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 paused  1 shell, 1 cron"]
        );

        put(dir.path(), "lh-1", stop(1, 0));
        assert_eq!(once(&herdr, &ledger, dir.path()), None);

        put(dir.path(), "lh-1", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 finished  no report"]
        );
    }

    #[test]
    fn a_stop_with_nothing_in_flight_wakes_it_finished() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", Body::Start { resumed: false });
        put(dir.path(), "lh-1", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 finished  no report"]
        );
    }

    #[test]
    fn a_stops_context_use_stays_out_of_its_wake_line() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", Body::Start { resumed: false });
        let Body::Stop(measured) = stop(1, 0) else {
            unreachable!()
        };
        let measured = Stop {
            context: Some(120_000),
            limit: Some(150_000),
            ..measured
        };
        put(dir.path(), "lh-1", Body::Stop(measured));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 paused  1 shell"]
        );
    }

    #[test]
    fn a_handoff_counts_as_the_report_before_a_finish() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", Body::Start { resumed: false });
        put(
            dir.path(),
            "lh-1",
            Body::Handoff {
                text: "at step 3".into(),
            },
        );
        put(dir.path(), "lh-1", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            [
                "lh-1 handoff 2026-11-01 09:00 UTC at step 3",
                "lh-1 finished"
            ]
        );
    }

    #[test]
    fn a_failed_stop_wakes_it_with_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-2", stop(1, 0));
        once(&herdr, &ledger, dir.path());

        put(dir.path(), "lh-2", failed("rate_limit"));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-2 failed  rate_limit"]
        );
    }

    #[test]
    fn unknown_background_reads_as_paused_with_it_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(
            dir.path(),
            "lh-1",
            Body::Stop(Stop {
                session: None,
                background: Tally::Unknown(Unknown::Unknown),
                crons: Tally::Known(0),
                error: None,
                context: None,
                limit: None,
            }),
        );
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 paused  background unknown"]
        );
    }

    #[test]
    fn a_report_read_by_an_earlier_watch_still_counts_for_the_finish() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", Body::Start { resumed: false });
        send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9");
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 done 2026-11-01 09:00 UTC PR #9"]
        );

        put(dir.path(), "lh-1", failed("overloaded"));
        put(dir.path(), "lh-1", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 failed  overloaded", "lh-1 finished"]
        );
    }

    #[test]
    fn a_finish_needs_a_report_since_the_last_finish_or_start() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        send(&ledger, dir.path(), "lh-1", Kind::Question, "which DB?");
        put(dir.path(), "lh-1", stop(0, 0));
        send(&ledger, dir.path(), "lh-2", Kind::Done, "PR #8");
        put(dir.path(), "lh-2", Body::Start { resumed: false });
        put(dir.path(), "lh-2", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            [
                "lh-1 question 2026-11-01 09:00 UTC which DB?",
                "lh-2 done 2026-11-01 09:00 UTC PR #8",
                "lh-1 finished",
                "lh-2 finished  no report",
            ]
        );

        send(&ledger, dir.path(), "lh-1", Kind::Note, "still here");
        put(dir.path(), "lh-1", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            [
                "lh-1 note 2026-11-01 09:00 UTC still here",
                "lh-1 finished  no report"
            ]
        );
    }

    #[test]
    fn herdr_going_done_or_idle_does_not_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        let req = Request {
            timeout: Some(Duration::from_secs(6)),
            ..EVERY
        };
        let mut clock = steps(|poll| match poll {
            1 => herdr.set_status("lh-1", AgentStatus::Done),
            _ => herdr.set_status("lh-2", AgentStatus::Idle),
        });
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);
    }

    #[test]
    fn a_herdr_restart_that_resumes_agents_unmatched_does_not_wake_it_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        let req = Request {
            timeout: Some(Duration::from_secs(6)),
            ..EVERY
        };
        let mut clock = steps(|poll| match poll {
            1 => herdr.set_unreachable(true),
            2 => {
                herdr.remove_agent("lh-1");
                herdr.set_unreachable(false);
            }
            _ => {}
        });
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);
    }

    #[test]
    fn a_member_gone_while_herdr_was_down_at_start_does_not_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        herdr.remove_agent("lh-1");
        herdr.set_unreachable(true);
        let req = Request {
            timeout: Some(Duration::from_secs(4)),
            ..EVERY
        };
        let mut clock = steps(|_| herdr.set_unreachable(false));
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);
    }

    #[test]
    fn a_bad_mailbox_line_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", stop(0, 0));
        let path = dir.path().join("lh-1/mailbox.jsonl");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(b"not json\n");
        std::fs::write(&path, bytes).unwrap();
        let mut warnings = vec![];

        let wake = watch(
            &herdr,
            &ledger,
            dir.path(),
            &ONCE,
            &mut steps(|_| {}),
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        printed(wake.unwrap());

        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn a_member_gone_when_the_watch_starts_does_not_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        herdr.remove_agent("lh-1");
        assert_eq!(once(&herdr, &ledger, dir.path()), None);
    }

    #[test]
    fn a_second_watch_on_the_same_mailboxes_is_refused_naming_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        let _first = only_watch(dir.path(), &Reader::Shepherd).unwrap();

        let Err(err) = watch(
            &herdr,
            &ledger,
            dir.path(),
            &ONCE,
            &mut steps(|_| {}),
            &mut |_| {},
        ) else {
            panic!("a second watch ran");
        };
        let lock = dir.path().join("watch.lock").display().to_string();
        let pid = format!("(pid {})", std::process::id());
        assert!(err.to_string().contains(&lock), "{err}");
        assert!(err.to_string().contains(&pid), "{err}");
    }

    #[test]
    fn an_error_after_reading_stops_leaves_them_unread() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", failed("rate_limit"));
        std::fs::write(dir.path().join(".cursor"), "corrupt").unwrap();
        assert!(
            watch(
                &herdr,
                &ledger,
                dir.path(),
                &ONCE,
                &mut steps(|_| {}),
                &mut |_| {}
            )
            .is_err()
        );

        std::fs::remove_file(dir.path().join(".cursor")).unwrap();
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 failed  rate_limit"]
        );
    }

    #[test]
    fn a_wake_never_marked_read_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9");
        put(dir.path(), "lh-1", stop(0, 0));
        let lost = watch(
            &herdr,
            &ledger,
            dir.path(),
            &ONCE,
            &mut steps(|_| {}),
            &mut |_| {},
        );
        drop(lost.unwrap().unwrap());

        let want = ["lh-1 done 2026-11-01 09:00 UTC PR #9", "lh-1 finished"];
        assert_eq!(once(&herdr, &ledger, dir.path()).unwrap(), want);
        assert_eq!(once(&herdr, &ledger, dir.path()), None);
    }

    #[test]
    fn a_member_gone_while_herdrs_reply_was_malformed_at_start_does_not_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        herdr.remove_agent("lh-1");
        herdr.set_malformed(true);
        let req = Request {
            timeout: Some(Duration::from_secs(6)),
            ..EVERY
        };
        let mut clock = steps(|_| herdr.set_malformed(false));
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);
    }

    #[test]
    fn a_restart_inside_one_poll_gap_does_not_wake_it_gone_while_agents_resume() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, mut herdr) = working_pair();
        herdr.resume_settles_after = None;
        let req = Request {
            timeout: Some(Duration::from_secs(10)),
            ..EVERY
        };
        let mut clock = steps(|poll| match poll {
            1 => {
                herdr.remove_agent("lh-1");
                herdr.remove_agent("lh-2");
                herdr.resume_agent("lh-1", "s1", "w9:p1");
            }
            // lh-2's resume lands a poll later.
            2 => herdr.resume_agent("lh-2", "s2", "w9:p2"),
            4 => herdr.settle(),
            _ => {}
        });
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);
    }

    #[test]
    fn a_pane_empty_for_only_one_poll_does_not_wake_it_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        let req = Request {
            timeout: Some(Duration::from_secs(6)),
            ..EVERY
        };
        let mut clock = steps(|poll| match poll {
            1 => herdr.remove_agent("lh-1"),
            2 => {
                herdr.resume_agent("lh-1", "s1", "w9:p1");
                herdr.settle();
            }
            _ => {}
        });
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);
    }

    fn lead(name: &str, pane: &str, herd: &str) -> AgentRecord {
        AgentRecord {
            role: Some(crate::ledger::Role::Lead),
            after_merge: None,
            unsandboxed: None,
            sockets_unblocked: None,
            earlier: vec![],
            ..member(name, pane, Some(herd))
        }
    }

    /// A lead of cards and a worker in cards, prs, and no herd, each with a
    /// note waiting.
    fn led_world(dir: &Path) -> (MemLedger, FakeHerdr) {
        let ledger = ledger(vec![
            lead("lead-cards", "w9:p0", "cards"),
            member("lh-1", "w9:p1", Some("cards")),
            member("zpr-2", "w9:p2", Some("prs")),
            member("solo", "w9:p3", None),
        ]);
        let mut herdr = FakeHerdr::new(&[]);
        for (name, pane) in [
            ("lead-cards", "w9:p0"),
            ("lh-1", "w9:p1"),
            ("zpr-2", "w9:p2"),
            ("solo", "w9:p3"),
        ] {
            herdr = herdr.with_live_agent(Some(name), None, pane);
            herdr.set_status(name, AgentStatus::Working);
            send(&ledger, dir, name, Kind::Note, "hi");
        }
        (ledger, herdr)
    }

    fn from_pane(pane: &str) -> Request<'_> {
        Request {
            own_pane: Some(pane),
            ..ONCE
        }
    }

    /// A working lead of cards and its working worker, nothing in a mailbox.
    fn quiet_led_world() -> (MemLedger, FakeHerdr) {
        let ledger = ledger(vec![
            lead("lead-cards", "w9:p0", "cards"),
            member("lh-1", "w9:p1", Some("cards")),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lead-cards"), None, "w9:p0")
            .with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lead-cards", AgentStatus::Working);
        herdr.set_status("lh-1", AgentStatus::Working);
        (ledger, herdr)
    }

    /// Notifying through `dir`'s marks, recording each command launched.
    struct Notifier {
        config: notify::Config,
        dir: PathBuf,
        sent: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl Notifier {
        fn new(dir: &Path) -> Notifier {
            Notifier::with_command(dir, "['n', '{herd}', '{member}', '{kind}']")
        }

        fn with_command(dir: &Path, command: &str) -> Notifier {
            let config = dir.join("config.toml");
            fs::write(
                &config,
                format!("[notify]\nenabled = true\nherdr = false\ncommand = {command}\n"),
            )
            .unwrap();
            Notifier {
                config: notify::load(&config).unwrap().unwrap(),
                dir: dir.to_path_buf(),
                sent: Default::default(),
            }
        }

        fn send(&self, events: &[Waiting]) {
            notify::send(&self.config, &self.dir, events, &mut |l| {
                self.sent.borrow_mut().push(l.args);
                Ok(())
            })
            .unwrap();
        }

        /// The shepherd's watch, sending what it wakes on as `loopherd watch` does.
        fn shepherd(
            &self,
            herdr: &FakeHerdr,
            ledger: &dyn Ledger,
            clock: &mut dyn Clock,
        ) -> Vec<String> {
            let req = Request {
                timeout: Some(Duration::from_secs(4)),
                ..EVERY
            };
            let wake = watch(herdr, ledger, &self.dir, &req, clock, &mut |_| {})
                .unwrap()
                .expect("the shepherd's watch woke");
            self.send(&wake.waiting);
            printed(wake)
        }

        /// The lead's watch, sending its own events through `notify_own`.
        fn lead(
            &self,
            herdr: &FakeHerdr,
            ledger: &dyn Ledger,
            clock: &mut dyn Clock,
        ) -> Option<Vec<String>> {
            let own = |events: &[Waiting]| self.send(events);
            let req = Request {
                timeout: Some(Duration::from_secs(4)),
                notify_own: Some(&own),
                ..from_pane("w9:p0")
            };
            run(herdr, ledger, &self.dir, &req, clock)
        }

        fn sent(&self) -> Vec<Vec<String>> {
            self.sent.borrow().clone()
        }
    }

    fn sent_for(member: &str, kind: &str) -> Vec<String> {
        ["n", "cards", member, kind].map(String::from).to_vec()
    }

    #[test]
    fn a_block_after_the_hook_saw_the_last_one_end_is_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        let notifier = Notifier::new(dir.path());
        // Every watch reads the lead blocked; only the hook sees the blocks end.
        herdr.set_status("lead-cards", AgentStatus::Blocked);

        blocked_hook(&ledger, dir.path(), "w9:p0");
        let printed = notifier.shepherd(&herdr, &ledger, &mut steps(|_| {}));
        assert_eq!(printed, ["lead-cards blocked"]);
        hook(
            &ledger,
            dir.path(),
            "pane.agent_status_changed",
            "w9:p0",
            Some("idle"),
        );
        blocked_hook(&ledger, dir.path(), "w9:p0");
        let printed = notifier.shepherd(&herdr, &ledger, &mut steps(|_| {}));

        assert_eq!(printed, ["lead-cards blocked"]);
        assert_eq!(notifier.sent(), vec![sent_for("lead-cards", "blocked"); 2]);
    }

    #[test]
    fn a_leads_watch_sends_the_leads_own_question_without_waking_on_it_or_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        send(
            &ledger,
            dir.path(),
            "lead-cards",
            Kind::Question,
            "Merge #1?",
        );
        let notifier = Notifier::new(dir.path());

        let woke = notifier.lead(&herdr, &ledger, &mut steps(|_| {}));

        assert_eq!(woke, None);
        assert_eq!(notifier.sent(), [sent_for("lead-cards", "question")]);
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})).unwrap(),
            ["lead-cards question 2026-11-01 09:00 UTC Merge #1?"]
        );
    }

    #[test]
    fn a_leads_question_read_by_its_watch_and_the_shepherds_is_sent_once() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        send(
            &ledger,
            dir.path(),
            "lead-cards",
            Kind::Question,
            "Merge #1?",
        );
        let notifier = Notifier::new(dir.path());

        notifier.lead(&herdr, &ledger, &mut steps(|_| {}));
        let printed = notifier.shepherd(&herdr, &ledger, &mut steps(|_| {}));

        assert_eq!(
            printed,
            ["lead-cards question 2026-11-01 09:00 UTC Merge #1?"]
        );
        assert_eq!(notifier.sent(), [sent_for("lead-cards", "question")]);
    }

    #[test]
    fn a_lead_going_blocked_is_sent_once_by_both_watches_and_again_once_unblocked() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        let notifier = Notifier::new(dir.path());
        let blocks = |poll| {
            if poll == 1 {
                herdr.set_status("lead-cards", AgentStatus::Blocked);
            }
        };

        // The lead's watch runs between two of the shepherd's polls, so both
        // see the lead go from working to blocked.
        let printed = notifier.shepherd(
            &herdr,
            &ledger,
            &mut steps(|poll| {
                if poll == 1 {
                    assert_eq!(notifier.lead(&herdr, &ledger, &mut steps(blocks)), None);
                }
            }),
        );
        assert_eq!(printed, ["lead-cards blocked"]);
        assert_eq!(notifier.sent(), [sent_for("lead-cards", "blocked")]);

        herdr.set_status("lead-cards", AgentStatus::Working);
        notifier.shepherd(&herdr, &ledger, &mut steps(blocks));
        assert_eq!(notifier.sent().len(), 2);
    }

    #[test]
    fn a_leads_watch_alone_sends_the_leads_block_and_again_after_it_unblocks() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        let notifier = Notifier::new(dir.path());
        let own = |events: &[Waiting]| notifier.send(events);
        let req = Request {
            timeout: Some(Duration::from_secs(8)),
            notify_own: Some(&own),
            ..from_pane("w9:p0")
        };

        let woke = run(
            &herdr,
            &ledger,
            dir.path(),
            &req,
            &mut steps(|poll| match poll {
                1 | 3 => herdr.set_status("lead-cards", AgentStatus::Blocked),
                2 => {
                    assert_eq!(notifier.sent(), [sent_for("lead-cards", "blocked")]);
                    herdr.set_status("lead-cards", AgentStatus::Working);
                }
                _ => {}
            }),
        );

        assert_eq!(woke, None);
        assert_eq!(notifier.sent(), vec![sent_for("lead-cards", "blocked"); 2]);
    }

    #[test]
    fn a_block_marked_by_another_watch_during_the_status_read_is_not_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, mut herdr) = quiet_led_world();
        let notifier = Notifier::new(dir.path());
        let mark = dir.path().join("lead-cards.blocked");
        let marked = std::sync::atomic::AtomicBool::new(false);
        // Another watch sees the lead blocked and marks it while this one reads it as working.
        let other_watch = mark.clone();
        herdr.on_agents = Some(Box::new(move || {
            if !marked.swap(true, std::sync::atomic::Ordering::SeqCst) {
                fs::write(&other_watch, "").unwrap();
            }
        }));

        let printed = notifier.shepherd(
            &herdr,
            &ledger,
            &mut steps(|_| herdr.set_status("lead-cards", AgentStatus::Blocked)),
        );

        assert_eq!(printed, ["lead-cards blocked"]);
        assert!(mark.exists());
        assert!(notifier.sent().is_empty());
    }

    /// The herdr plugin's event hook, as herdr runs it for `pane`.
    fn hook(ledger: &dyn Ledger, dir: &Path, event: &str, pane: &str, status: Option<&str>) {
        let status = status.map_or(String::new(), |s| format!(r#","agent_status":"{s}""#));
        let json = format!(r#"{{"event":"x","data":{{"pane_id":"{pane}"{status}}}}}"#);
        let at = humantime::parse_rfc3339("2026-11-01T09:00:00Z").unwrap();
        // Only pane.moved writes the ledger, and these tests send none.
        let mut snapshot = MemLedger(ledger.entries().unwrap(), vec![]);
        crate::pane_event::event_hook(&mut snapshot, dir, event, &json, None, at, &|_| Ok(None))
            .unwrap();
    }

    fn blocked_hook(ledger: &dyn Ledger, dir: &Path, pane: &str) {
        hook(
            ledger,
            dir,
            "pane.agent_status_changed",
            pane,
            Some("blocked"),
        );
    }

    #[test]
    fn a_gone_recorded_while_no_watch_ran_wakes_the_next_watch_once() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]);

        hook(&ledger, dir.path(), "pane.exited", "w9:p1", None);
        hook(&ledger, dir.path(), "pane.closed", "w9:p1", None);

        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})).unwrap(),
            ["lh-1 gone"]
        );
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})),
            None
        );
    }

    #[test]
    fn a_block_recorded_while_no_watch_ran_wakes_the_next_watch_once_as_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lh-1", AgentStatus::Blocked);

        blocked_hook(&ledger, dir.path(), "w9:p1");

        let wake = watch(
            &herdr,
            &ledger,
            dir.path(),
            &ONCE,
            &mut steps(|_| {}),
            &mut |_| {},
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            waiting(&wake),
            [(None, "lh-1", notify::Kind::Blocked, "", false)]
        );
        assert_eq!(printed(wake), ["lh-1 blocked"]);
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})),
            None
        );
    }

    #[test]
    fn a_block_both_the_hook_and_the_poll_saw_prints_once_whichever_came_first() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lh-1", AgentStatus::Working);

        let hook_first = steps(|poll| match poll {
            1 => {
                herdr.set_status("lh-1", AgentStatus::Blocked);
                blocked_hook(&ledger, dir.path(), "w9:p1");
            }
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut { hook_first }).unwrap(),
            ["lh-1 blocked"]
        );
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})),
            None
        );

        hook(
            &ledger,
            dir.path(),
            "pane.agent_status_changed",
            "w9:p1",
            Some("working"),
        );
        herdr.set_status("lh-1", AgentStatus::Working);
        let poll_first = steps(|poll| match poll {
            1 => herdr.set_status("lh-1", AgentStatus::Blocked),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut { poll_first }).unwrap(),
            ["lh-1 blocked"]
        );
        blocked_hook(&ledger, dir.path(), "w9:p1");
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})),
            None
        );
    }

    #[test]
    fn a_gone_the_poll_saw_before_the_hook_prints_once() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lh-1", AgentStatus::Idle);

        let mut clock = steps(|poll| match poll {
            1 => herdr.remove_agent("lh-1"),
            2 => {}
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 gone"]
        );
        hook(&ledger, dir.path(), "pane.closed", "w9:p1", None);
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})),
            None
        );
    }

    #[test]
    fn a_mailbox_the_poll_cannot_write_warns_and_another_members_block_still_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Idle);
        herdr.set_status("lh-2", AgentStatus::Idle);
        fs::create_dir_all(dir.path().join("lh-1/mailbox.jsonl")).unwrap();
        let mut clock = steps(|poll| match poll {
            1 => herdr.set_status("lh-1", AgentStatus::Blocked),
            2 => herdr.remove_agent("lh-1"),
            3 => {}
            4 => herdr.set_status("lh-2", AgentStatus::Blocked),
            _ => panic!("slept past the wake"),
        });
        let mut warnings = vec![];

        let woke = watch(&herdr, &ledger, dir.path(), &EVERY, &mut clock, &mut |w| {
            warnings.push(w.to_string())
        })
        .unwrap();

        assert_eq!(printed(woke.unwrap()), ["lh-2 blocked"]);
        let recording: Vec<&str> = warnings
            .iter()
            .filter_map(|w| w.split(':').nth(1))
            .filter(|w| w.starts_with(" record"))
            .collect();
        assert_eq!(recording, [" record lh-1 blocked", " record lh-1 gone"]);
    }

    #[test]
    fn a_member_back_without_a_start_wakes_a_later_watch_gone_again() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let goes_while_watched = |herdr: &FakeHerdr| {
            herdr.set_status("lh-1", AgentStatus::Idle);
            let mut clock = steps(|poll| match poll {
                1 => herdr.remove_agent("lh-1"),
                2 => {}
                _ => panic!("slept past the wake"),
            });
            run(herdr, &ledger, dir.path(), &EVERY, &mut clock)
        };

        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        assert_eq!(goes_while_watched(&herdr).unwrap(), ["lh-1 gone"]);
        let back = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        assert_eq!(goes_while_watched(&back).unwrap(), ["lh-1 gone"]);
    }

    #[test]
    fn blocks_recorded_between_watches_print_one_line_per_member() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lh-1", AgentStatus::Working);

        blocked_hook(&ledger, dir.path(), "w9:p1");
        hook(
            &ledger,
            dir.path(),
            "pane.agent_status_changed",
            "w9:p1",
            Some("working"),
        );
        blocked_hook(&ledger, dir.path(), "w9:p1");

        let wake = watch(
            &herdr,
            &ledger,
            dir.path(),
            &ONCE,
            &mut steps(|_| {}),
            &mut |_| {},
        )
        .unwrap()
        .unwrap();
        assert_eq!(wake.waiting.len(), 1);
        assert_eq!(printed(wake), ["lh-1 blocked"]);
    }

    #[test]
    fn a_leads_watch_reads_its_workers_hook_events_and_the_shepherds_the_leads() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        hook(&ledger, dir.path(), "pane.closed", "w9:p1", None);
        blocked_hook(&ledger, dir.path(), "w9:p0");

        let lead_once = Request {
            timeout: Some(Duration::ZERO),
            ..from_pane("w9:p0")
        };
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &lead_once, &mut steps(|_| {})).unwrap(),
            ["lh-1 gone"]
        );
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})).unwrap(),
            ["lead-cards blocked"]
        );
    }

    #[test]
    fn a_leads_watch_reads_only_its_own_herds_workers() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, herdr) = led_world(dir.path());
        ledger
            .0
            .insert("lead-prs".into(), lead("lead-prs", "w9:p8", "prs"));

        assert_eq!(
            run(
                &herdr,
                &ledger,
                dir.path(),
                &from_pane("w9:p0"),
                &mut steps(|_| {})
            )
            .unwrap(),
            ["lh-1 note 2026-11-01 09:00 UTC hi"]
        );
    }

    #[test]
    fn a_leads_watch_may_name_only_its_own_herd() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let prs = ["prs".to_string()];
        let req = Request {
            herds: &prs,
            ..from_pane("w9:p0")
        };

        let Err(err) = watch(
            &herdr,
            &ledger,
            dir.path(),
            &req,
            &mut steps(|_| {}),
            &mut |_| {},
        ) else {
            panic!("a lead watched another herd");
        };
        assert!(err.to_string().contains("lead-cards leads cards"), "{err}");
        assert!(err.to_string().contains("prs"), "{err}");

        let cards = ["cards".to_string()];
        let own = Request {
            herds: &cards,
            ..from_pane("w9:p0")
        };
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &own, &mut steps(|_| {})).unwrap(),
            ["lh-1 note 2026-11-01 09:00 UTC hi"]
        );
    }

    const TOLD: &str = "shepherd handoff 171k over 160k";

    /// Runs `req` with a shepherd's check that finds `session` over its limit.
    fn with_handoff(
        dir: &Path,
        ledger: &MemLedger,
        herdr: &FakeHerdr,
        req: &Request,
        session: &str,
    ) -> Option<Vec<String>> {
        let marker = dir.join("told").join(session);
        let check = || Some(ShepherdHandoff::new(TOLD, session, marker.clone()));
        let req = Request {
            shepherd_handoff: Some(&check),
            ..*req
        };
        let woke = run(herdr, ledger, dir, &req, &mut steps(|_| {}));
        assert_eq!(
            marker.exists(),
            woke.as_ref().is_some_and(|l| l.iter().any(|l| l == TOLD))
        );
        woke
    }

    #[test]
    fn the_shepherds_own_watch_adds_its_handoff_line_last_and_marks_it_once_printed() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let woke = with_handoff(dir.path(), &ledger, &herdr, &ONCE, "s-shepherd").unwrap();
        assert_eq!(woke.last().map(String::as_str), Some(TOLD));
        assert_eq!(woke.len(), 4);
        let woke = with_handoff(dir.path(), &ledger, &herdr, &from_pane("w9:p7"), "s2");
        assert_eq!(woke, Some(vec![TOLD.to_string()]));
    }

    #[test]
    fn a_due_handoff_line_wakes_a_watch_that_would_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {}));
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &ONCE, &mut steps(|_| {})),
            None
        );
        assert_eq!(
            with_handoff(dir.path(), &ledger, &herdr, &ONCE, "s1"),
            Some(vec![TOLD.to_string()])
        );
    }

    #[test]
    fn a_watch_from_a_members_pane_never_adds_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        for pane in ["w9:p0", "w9:p1"] {
            let woke = with_handoff(dir.path(), &ledger, &herdr, &from_pane(pane), "s1").unwrap();
            assert!(!woke.iter().any(|l| l == TOLD), "{pane}: {woke:?}");
        }
    }

    #[test]
    fn a_session_a_member_holds_is_never_told_it_is_the_shepherd() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, herdr) = led_world(dir.path());
        // Its recorded socket doesn't match the watch's, so its pane reads as no member's.
        let worker = ledger.0.get_mut("lh-1").unwrap();
        worker.session_id = Some("s-worker".into());
        worker.socket = Some("/run/other.sock".into());
        let req = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            ..from_pane("w9:p1")
        };
        let woke = with_handoff(dir.path(), &ledger, &herdr, &req, "s-worker").unwrap();
        assert!(!woke.iter().any(|l| l == TOLD), "{woke:?}");

        ledger.0.get_mut("lh-1").unwrap().retired = true;
        let woke = with_handoff(dir.path(), &ledger, &herdr, &ONCE, "s-worker");
        assert_eq!(woke, Some(vec![TOLD.to_string()]));
    }

    #[test]
    fn the_shepherds_watch_reads_leads_and_herds_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());

        // A worker's pane is not a lead's, so its watch is the shepherd's.
        let mut woke = run(
            &herdr,
            &ledger,
            dir.path(),
            &from_pane("w9:p1"),
            &mut steps(|_| {}),
        )
        .unwrap();
        woke.sort();
        assert_eq!(
            woke,
            [
                "lead-cards note 2026-11-01 09:00 UTC hi",
                "solo note 2026-11-01 09:00 UTC hi",
                "zpr-2 note 2026-11-01 09:00 UTC hi",
            ]
        );
    }

    #[test]
    fn naming_a_led_herd_narrows_the_shepherd_to_its_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let cards = ["cards".to_string()];
        let req = Request {
            herds: &cards,
            ..ONCE
        };

        assert_eq!(
            run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap(),
            ["lead-cards note 2026-11-01 09:00 UTC hi"]
        );
    }

    #[test]
    fn a_herd_gaining_a_lead_moves_its_workers_to_the_leads_watch_until_it_retires() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = ledger(vec![member("zpr-2", "w9:p2", Some("prs"))]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("zpr-2"), None, "w9:p2")
            .with_live_agent(Some("lead-prs"), None, "w9:p9");
        herdr.set_status("zpr-2", AgentStatus::Working);
        let shepherd = |ledger: &MemLedger| {
            send(ledger, dir.path(), "zpr-2", Kind::Note, "hi");
            run(&herdr, ledger, dir.path(), &ONCE, &mut steps(|_| {}))
        };
        assert_eq!(
            shepherd(&ledger).unwrap(),
            ["zpr-2 note 2026-11-01 09:00 UTC hi"]
        );

        ledger
            .0
            .insert("lead-prs".into(), lead("lead-prs", "w9:p9", "prs"));
        assert_eq!(shepherd(&ledger), None);
        assert_eq!(
            run(
                &herdr,
                &ledger,
                dir.path(),
                &from_pane("w9:p9"),
                &mut steps(|_| {})
            )
            .unwrap(),
            ["zpr-2 note 2026-11-01 09:00 UTC hi"]
        );

        // Gone, it still owns its herd; retired, it lets go.
        herdr.remove_agent("lead-prs");
        assert_eq!(shepherd(&ledger), None);
        ledger.0.get_mut("lead-prs").unwrap().retired = true;
        assert_eq!(
            shepherd(&ledger).unwrap(),
            [
                "zpr-2 note 2026-11-01 09:00 UTC hi",
                "zpr-2 note 2026-11-01 09:00 UTC hi",
            ]
        );
    }

    #[test]
    fn a_leads_watch_and_the_shepherds_hold_their_own_locks() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let _shepherd = only_watch(dir.path(), &Reader::Shepherd).unwrap();

        assert!(
            run(
                &herdr,
                &ledger,
                dir.path(),
                &from_pane("w9:p0"),
                &mut steps(|_| {})
            )
            .is_some()
        );

        let lead = Reader::Lead {
            created: "2026-11-01T00:00:00Z".into(),
            name: "lead-cards".into(),
            herd: "cards".into(),
        };
        let _lead = only_watch(dir.path(), &lead).unwrap();
        let Err(err) = watch(
            &herdr,
            &ledger,
            dir.path(),
            &from_pane("w9:p0"),
            &mut steps(|_| {}),
            &mut |_| {},
        ) else {
            panic!("a second watch for the lead ran");
        };
        let lock = dir
            .path()
            .join("watch-lead-cards.lock")
            .display()
            .to_string();
        assert!(err.to_string().contains(&lock), "{err}");
    }

    #[test]
    fn a_leads_watch_ends_with_an_error_once_the_lead_is_retired() {
        let dir = tempfile::tempdir().unwrap();
        let (mem, herdr) = led_world(dir.path());
        let path = dir.path().join("state/ledger.json");
        let mut ledger = FileLedger::new(path.clone());
        mem.0.into_values().for_each(|e| ledger.record(e).unwrap());
        assert_eq!(
            run_dyn(&herdr, &ledger, dir.path(), &from_pane("w9:p0")).unwrap(),
            ["lh-1 note 2026-11-01 09:00 UTC hi"]
        );
        let req = Request {
            own_pane: Some("w9:p0"),
            ..EVERY
        };

        let mut clock = steps(|poll| match poll {
            1 => {
                let mut retire = FileLedger::new(path.clone());
                retire
                    .update("lead-cards", &mut |e| {
                        e.retired = true;
                        Ok(())
                    })
                    .unwrap();
                send(&retire, dir.path(), "lh-1", Kind::Note, "after");
            }
            _ => panic!("polled after the lead retired"),
        });
        let Err(err) = watch(&herdr, &ledger, dir.path(), &req, &mut clock, &mut |_| {}) else {
            panic!("the retired lead's watch kept running");
        };
        assert!(err.to_string().contains("lead-cards was retired"), "{err}");

        // The shepherd now owns cards, and the lead's watch left it unread.
        let woke = run_dyn(&herdr, &ledger, dir.path(), &ONCE).unwrap();
        assert!(
            woke.contains(&"lh-1 note 2026-11-01 09:00 UTC after".to_string()),
            "{woke:?}"
        );
    }

    fn run_dyn(
        herdr: &FakeHerdr,
        ledger: &dyn Ledger,
        dir: &Path,
        req: &Request,
    ) -> Option<Vec<String>> {
        watch(herdr, ledger, dir, req, &mut steps(|_| {}), &mut |_| {})
            .unwrap()
            .map(printed)
    }

    /// `led_world` with the lead recorded on herdr socket `/run/live.sock`.
    fn socketed_lead(dir: &Path) -> (MemLedger, FakeHerdr) {
        let (mut ledger, herdr) = led_world(dir);
        ledger.0.get_mut("lead-cards").unwrap().socket = Some("/run/live.sock".into());
        (ledger, herdr)
    }

    #[test]
    fn a_socketed_leads_pane_on_its_own_socket_is_the_leads_watch() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = socketed_lead(dir.path());
        let req = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            ..from_pane("w9:p0")
        };

        assert_eq!(
            run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap(),
            ["lh-1 note 2026-11-01 09:00 UTC hi"]
        );
    }

    #[test]
    fn a_leads_pane_id_on_another_socket_warns_once_that_this_is_the_shepherds_watch() {
        let cases = [
            (
                socketed_lead as fn(&Path) -> (MemLedger, FakeHerdr),
                Some(Path::new("/run/dev.sock")),
                "warning: pane w9:p0 holds lead lead-cards on socket /run/live.sock, \
                 but this watch's socket is /run/dev.sock; running the shepherd's watch",
            ),
            (
                socketed_lead,
                None,
                "warning: pane w9:p0 holds lead lead-cards on socket /run/live.sock, \
                 but this watch's socket is unset; running the shepherd's watch",
            ),
            (
                led_world,
                Some(Path::new("/run/live.sock")),
                "warning: pane w9:p0 holds lead lead-cards on socket unset, \
                 but this watch's socket is /run/live.sock; running the shepherd's watch",
            ),
        ];
        for (world, socket, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (ledger, herdr) = world(dir.path());
            let req = Request {
                own_socket: socket,
                ..from_pane("w9:p0")
            };
            let mut warnings = vec![];

            watch(
                &herdr,
                &ledger,
                dir.path(),
                &req,
                &mut steps(|_| {}),
                &mut |w| warnings.push(w.to_string()),
            )
            .unwrap();

            assert_eq!(warnings, [expected]);
        }
    }

    #[test]
    fn a_workers_pane_on_its_own_socket_warns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let mut warnings = vec![];

        watch(
            &herdr,
            &ledger,
            dir.path(),
            &from_pane("w9:p1"),
            &mut steps(|_| {}),
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();

        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Why `req`'s watch over `led_world` with a socketed lead refused to start.
    fn refused(req: &Request) -> String {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, herdr) = socketed_lead(dir.path());
        ledger
            .0
            .insert("lead-prs".into(), lead("lead-prs", "w9:p8", "prs"));
        ledger.0.get_mut("lead-prs").unwrap().socket = Some("/run/live.sock".into());
        let mut warnings = vec![];
        let Err(err) = watch(
            &herdr,
            &ledger,
            dir.path(),
            req,
            &mut steps(|_| {}),
            &mut |w| warnings.push(w.to_string()),
        ) else {
            panic!("a lead's Claude ran a watch other than its own");
        };
        assert!(warnings.is_empty(), "{warnings:?}");
        format!("{err:#}")
    }

    #[test]
    fn a_watch_a_leads_claude_runs_from_its_own_pane_is_the_leads_watch() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = socketed_lead(dir.path());
        let req = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            caller: Some("lead-cards"),
            ..from_pane("w9:p0")
        };

        assert_eq!(
            run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap(),
            ["lh-1 note 2026-11-01 09:00 UTC hi"]
        );
    }

    #[test]
    fn a_leads_claude_may_not_run_the_shepherds_watch() {
        let shepherds = [
            (
                Request {
                    caller: Some("lead-cards"),
                    ..ONCE
                },
                "this pane isn't lead-cards's in the ledger",
            ),
            (
                Request {
                    own_socket: Some(Path::new("/run/dev.sock")),
                    caller: Some("lead-cards"),
                    ..from_pane("w9:p0")
                },
                "pane w9:p0 holds lead lead-cards on socket /run/live.sock, \
                 but this watch's socket is /run/dev.sock",
            ),
        ];
        for (req, why) in shepherds {
            assert_eq!(
                refused(&req),
                format!(
                    "refused: lead-cards's Claude runs this watch, which may only be its own; \
                     {why}, so it would be the shepherd's watch"
                )
            );
        }
    }

    #[test]
    fn a_retired_leads_claude_is_told_its_lead_is_retired() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, herdr) = socketed_lead(dir.path());
        ledger.0.get_mut("lead-cards").unwrap().retired = true;
        let req = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            caller: Some("lead-cards"),
            ..from_pane("w9:p0")
        };

        let err = watch(
            &herdr,
            &ledger,
            dir.path(),
            &req,
            &mut steps(|_| {}),
            &mut |_| {},
        )
        .err()
        .unwrap();

        assert_eq!(
            format!("{err:#}"),
            "refused: lead-cards's Claude runs this watch, which may only be its own; \
             lead-cards is retired in the ledger, so it would be the shepherd's watch"
        );
    }

    #[test]
    fn a_leads_watch_takes_on_the_lead_before_its_checks_and_the_shepherds_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = socketed_lead(dir.path());
        let seen = std::cell::RefCell::new(vec![]);
        let as_lead = |lead: &str| -> Result<()> {
            seen.borrow_mut().push(lead.to_string());
            Ok(())
        };
        let socket = Some(Path::new("/run/live.sock"));
        let leads = Request {
            own_socket: socket,
            as_lead: Some(&as_lead),
            ..from_pane("w9:p0")
        };
        let shepherds = Request {
            own_socket: socket,
            as_lead: Some(&as_lead),
            ..ONCE
        };

        run(&herdr, &ledger, dir.path(), &leads, &mut steps(|_| {}));
        run(&herdr, &ledger, dir.path(), &shepherds, &mut steps(|_| {}));

        assert_eq!(*seen.borrow(), ["lead-cards"]);
    }

    #[test]
    fn a_lead_that_cant_be_taken_on_stops_its_watch() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = socketed_lead(dir.path());
        let as_lead = |lead: &str| -> Result<()> { bail!("{lead}'s watch can't start: no token") };
        let req = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            as_lead: Some(&as_lead),
            ..from_pane("w9:p0")
        };

        let err = watch(
            &herdr,
            &ledger,
            dir.path(),
            &req,
            &mut steps(|_| {}),
            &mut |_| {},
        )
        .err()
        .unwrap();

        assert_eq!(
            format!("{err:#}"),
            "lead-cards's watch can't start: no token"
        );
    }

    #[test]
    fn a_leads_claude_may_not_run_another_leads_watch() {
        let req = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            caller: Some("lead-cards"),
            ..from_pane("w9:p8")
        };

        assert_eq!(
            refused(&req),
            "refused: lead-cards's Claude runs this watch, which may only be its own; \
             this pane is lead-prs's"
        );
    }

    #[test]
    fn a_leads_pane_id_on_another_socket_or_none_is_the_shepherds_watch() {
        for socket in [Some(Path::new("/run/dev.sock")), None] {
            let dir = tempfile::tempdir().unwrap();
            let (ledger, herdr) = socketed_lead(dir.path());
            let req = Request {
                own_socket: socket,
                ..from_pane("w9:p0")
            };

            let woke = run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap();
            assert!(
                woke.contains(&"lead-cards note 2026-11-01 09:00 UTC hi".to_string()),
                "{socket:?}: {woke:?}"
            );
        }
    }

    #[test]
    fn a_socketless_leads_pane_id_is_its_watch_only_from_a_caller_without_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let socketless = from_pane("w9:p0");
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &socketless, &mut steps(|_| {})).unwrap(),
            ["lh-1 note 2026-11-01 09:00 UTC hi"]
        );

        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let socketed = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            ..from_pane("w9:p0")
        };
        let woke = run(&herdr, &ledger, dir.path(), &socketed, &mut steps(|_| {})).unwrap();
        assert!(
            woke.contains(&"lead-cards note 2026-11-01 09:00 UTC hi".to_string()),
            "{woke:?}"
        );
    }

    #[test]
    fn a_pane_matching_no_entry_is_the_shepherds_watch() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = socketed_lead(dir.path());
        let req = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            ..from_pane("w9:p7")
        };

        let woke = run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap();
        assert!(
            woke.contains(&"lead-cards note 2026-11-01 09:00 UTC hi".to_string()),
            "{woke:?}"
        );
    }
    /// Herd `cards` led by lead-cards, with worker lh-1 of template `card`,
    /// whose `done` and `merged` checks test for files in the repo and whose
    /// queue lists the lines of `tasks`, two slots.
    struct Checked {
        config: tempfile::TempDir,
        repo: tempfile::TempDir,
        paths: crate::herd::Paths,
        ledger: MemLedger,
        herdr: FakeHerdr,
    }

    impl Checked {
        fn new() -> Checked {
            Checked::with_template("done = \"test -f done-{n}\"\nmerged = \"test -f merged-{n}\"\n")
        }

        fn with_template(checks: &str) -> Checked {
            let config = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            fs::create_dir(config.path().join("herds")).unwrap();
            let herd = format!(
                "repo = {:?}\n[template.card]\nparams = [\"n\"]\nname = \"lh-{{n}}\"\n\
                 prompt = \"go\"\n{checks}[queue]\ntemplate = \"card\"\nsource = \"cat tasks\"\nmax = 2\n",
                repo.path().display().to_string(),
            );
            fs::write(config.path().join("herds/cards.toml"), herd).unwrap();
            fs::write(repo.path().join("tasks"), "").unwrap();
            let paths = crate::herd::Paths {
                herds: config.path().join("herds"),
                home: PathBuf::from("/home/me"),
                feed_lock: config.path().join("feed.lock"),
                paused: config.path().join("paused"),
            };
            let worker = AgentRecord {
                template: Some("card".into()),
                params: Some([("n".to_string(), "1".to_string())].into()),
                ..member("lh-1", "w9:p1", Some("cards"))
            };
            let ledger = ledger(vec![lead("lead-cards", "w9:p0", "cards"), worker]);
            let mut herdr = FakeHerdr::new(&[]);
            for (name, pane) in [("lead-cards", "w9:p0"), ("lh-1", "w9:p1")] {
                herdr = herdr.with_live_agent(Some(name), None, pane);
                herdr.set_status(name, AgentStatus::Working);
            }
            Checked {
                config,
                repo,
                paths,
                ledger,
                herdr,
            }
        }

        fn touch(&self, file: &str, text: &str) {
            fs::write(self.repo.path().join(file), text).unwrap();
        }

        /// Rewrites the herd file through `edit`.
        fn edit_herd(&self, edit: impl Fn(String) -> String) {
            let file = self.paths.herds.join("cards.toml");
            fs::write(&file, edit(fs::read_to_string(&file).unwrap())).unwrap();
        }

        fn request(&self, pane: &'static str) -> Request<'_> {
            Request {
                herd_files: Some(&self.paths),
                check_timeout: || Ok(Duration::from_secs(5)),
                ..from_pane(pane)
            }
        }

        fn reports(&self) -> PathBuf {
            self.config.path().join("reports")
        }

        /// One poll of a fresh lead's watch at wall-clock `at`, asking
        /// `merges` and sending its own events through `notifier`.
        fn lead_at(&self, notifier: &Notifier, merges: &dyn Merges, at: &str) {
            assert_eq!(self.lead_wakes_at(notifier, merges, at), None);
        }

        /// What one poll of a fresh lead's watch at `at`, which wakes on
        /// nothing, warns.
        fn lead_warned_at(
            &self,
            notifier: &Notifier,
            merges: &dyn Merges,
            at: &str,
        ) -> Vec<String> {
            let own = |events: &[Waiting]| notifier.send(events);
            let req = Request {
                notify_own: Some(&own),
                merges: Some(merges),
                ..self.request("w9:p0")
            };
            let mut warned = vec![];
            let woke = watch(
                &self.herdr,
                &self.ledger,
                &self.reports(),
                &req,
                &mut steps_from(at, |_| {}),
                &mut |w| warned.push(w.to_string()),
            );
            assert!(woke.unwrap().is_none());
            warned
        }

        /// What one poll of a fresh lead's watch at `at` wakes on.
        fn lead_wakes_at(
            &self,
            notifier: &Notifier,
            merges: &dyn Merges,
            at: &str,
        ) -> Option<Vec<String>> {
            let own = |events: &[Waiting]| notifier.send(events);
            let req = Request {
                notify_own: Some(&own),
                merges: Some(merges),
                ..self.request("w9:p0")
            };
            run(
                &self.herdr,
                &self.ledger,
                &self.reports(),
                &req,
                &mut steps_from(at, |_| {}),
            )
        }

        /// One poll of a fresh watch from `pane`.
        fn once(&self, pane: &'static str) -> Option<Vec<String>> {
            let dir = self.config.path().join("reports");
            run(
                &self.herdr,
                &self.ledger,
                &dir,
                &self.request(pane),
                &mut steps(|_| {}),
            )
        }
    }

    /// Checked, its lead's `[lead]` ending in `lead`, with a notifier that
    /// sends each event's member, kind and text, and gh listing `merged` in its repo.
    fn guarded(lead: &str, merged: Vec<(&'static str, u64)>) -> (Checked, Notifier, FakeMerges) {
        let world = Checked::new();
        world.edit_herd(|h| format!("{h}[lead]\nprompt = \"go\"\n{lead}"));
        let reports = world.reports();
        fs::create_dir_all(&reports).unwrap();
        let notifier = Notifier::with_command(&reports, "['n', '{member}', '{kind}', '{text}']");
        let gh = FakeMerges::new(world.repo.path(), merged);
        (world, notifier, gh)
    }

    fn merge_sent(number: u64) -> Vec<String> {
        let url = format!("#{number} https://github.com/o/r/pull/{number}");
        ["n", "lead-cards", "merge_alert", &url]
            .map(String::from)
            .to_vec()
    }

    /// The first lookup's time, and one due after it.
    const FIRST: &str = "2026-11-01T09:00:00Z";
    const LATER: &str = "2026-11-01T09:10:00Z";

    #[test]
    fn a_leads_first_lookup_sets_a_baseline_and_later_ones_send_each_new_merge_once() {
        let older = ("2026-11-01T08:00:00Z", 6);
        let (world, notifier, gh) = guarded("merge = false\n", vec![older]);

        world.lead_at(&notifier, &gh, FIRST);
        gh.merged.borrow_mut().push(("2026-11-01T09:05:00Z", 7));
        world.lead_at(&notifier, &gh, LATER);
        world.lead_at(&notifier, &gh, "2026-11-01T09:20:00Z");

        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn a_new_lead_ignores_its_predecessors_merge_window_and_mailbox() {
        let predecessors = ("2026-10-20T00:00:00Z", 6);
        let (world, notifier, gh) = guarded("", vec![predecessors]);
        let reports = world.reports();
        let state = r#"{"created":"2026-10-01T00:00:00Z","since":"2026-10-01T00:00:00Z","polled":"2026-10-01T00:00:00Z","branch":"main"}"#;
        fs::write(reports.join("lead-cards.merges"), state).unwrap();
        for (at, body) in [
            (
                "2026-10-02T00:00:00Z",
                Body::Gone {
                    tab: None,
                    workspace: None,
                },
            ),
            ("2026-10-03T00:00:00Z", Body::Start { resumed: true }),
        ] {
            append(
                &reports,
                "lead-cards",
                &Entry {
                    at: at.into(),
                    body,
                },
            )
            .unwrap();
        }

        world.lead_at(&notifier, &gh, FIRST);
        gh.merged.borrow_mut().push(("2026-11-01T09:05:00Z", 7));
        world.lead_at(&notifier, &gh, LATER);

        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn merges_are_looked_up_at_most_once_a_minute_across_watches_with_the_branch_kept() {
        let (world, notifier, gh) = guarded("", vec![]);

        world.lead_at(&notifier, &gh, FIRST);
        assert_eq!(gh.lookups.get(), 0, "the baseline asks nothing");
        world.lead_at(&notifier, &gh, LATER);
        assert_eq!(gh.lookups.get(), 2, "the default branch, then the merges");
        world.lead_at(&notifier, &gh, "2026-11-01T09:10:30Z");
        assert_eq!(gh.lookups.get(), 2, "a watch re-armed within the minute");
        world.lead_at(&notifier, &gh, "2026-11-01T09:11:00Z");
        assert_eq!(gh.lookups.get(), 3, "the merges alone");
    }

    #[test]
    fn a_revived_lead_hears_only_of_merges_since_its_revive() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        let reports = world.reports();
        for (at, body) in [
            (
                "2026-11-01T09:20:00Z",
                Body::Gone {
                    tab: None,
                    workspace: None,
                },
            ),
            ("2026-11-01T09:40:00.5Z", Body::Start { resumed: true }),
        ] {
            append(
                &reports,
                "lead-cards",
                &Entry {
                    at: at.into(),
                    body,
                },
            )
            .unwrap();
        }
        let down = ("2026-11-01T09:30:00Z", 6);
        let revived = ("2026-11-01T09:45:00Z", 7);
        gh.merged.borrow_mut().extend([down, revived]);

        world.lead_at(&notifier, &gh, "2026-11-01T09:50:00Z");

        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn a_lead_with_merge_on_by_its_herd_file_or_lead_defaults_hears_of_no_merges() {
        for (lead, defaults) in [
            ("merge = true\n", ""),
            ("", "[lead_defaults]\nmerge = true\n"),
        ] {
            let (world, notifier, gh) = guarded(lead, vec![]);
            fs::write(world.config.path().join("config.toml"), defaults).unwrap();

            world.lead_at(&notifier, &gh, FIRST);
            gh.merged.borrow_mut().push(("2026-11-01T09:05:00Z", 7));
            world.lead_at(&notifier, &gh, LATER);

            assert_eq!(
                notifier.sent(),
                Vec::<Vec<String>>::new(),
                "{lead}{defaults}"
            );
            assert_eq!(gh.lookups.get(), 0);
        }
    }

    #[test]
    fn a_lead_with_merge_off_by_default_hears_of_merges() {
        let (world, notifier, gh) = guarded("merge_guard = \"hook\"\n", vec![]);

        world.lead_at(&notifier, &gh, FIRST);
        gh.merged.borrow_mut().push(("2026-11-01T09:05:00Z", 7));
        world.lead_at(&notifier, &gh, LATER);

        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn failing_merge_lookups_warn_once_a_streak_and_fail_no_watch() {
        let (world, notifier, mut gh) = guarded("", vec![]);
        gh.repo = PathBuf::from("/elsewhere");
        let own = |events: &[Waiting]| notifier.send(events);
        let req = Request {
            timeout: Some(Duration::from_secs(130)),
            notify_own: Some(&own),
            merges: Some(&gh),
            ..world.request("w9:p0")
        };
        let mut warned = vec![];

        let woke = watch(
            &world.herdr,
            &world.ledger,
            &world.reports(),
            &req,
            &mut steps(|_| {}),
            &mut |w| warned.push(w.to_string()),
        );

        assert!(woke.unwrap().is_none());
        assert_eq!(gh.lookups.get(), 2, "one lookup a round after the baseline");
        let repo = world.repo.path().display();
        assert_eq!(warned, [format!("cards's merges: no repo {repo}")]);
        assert_eq!(notifier.sent(), Vec::<Vec<String>>::new());
    }

    fn merges_summed(text: &str) -> Vec<String> {
        ["n", "lead-cards", "merge_alert", text]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn merges_while_no_lookup_ran_for_over_an_hour_are_sent_as_one_summary_once() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        let unwatched = [("2026-11-01T10:00:00Z", 7), ("2026-11-01T11:00:00Z", 8)];
        gh.merged.borrow_mut().extend(unwatched);

        world.lead_at(&notifier, &gh, "2026-11-01T13:00:00Z");
        gh.merged.borrow_mut().push(("2026-11-01T13:00:30Z", 9));
        world.lead_at(&notifier, &gh, "2026-11-01T13:02:00Z");

        let summary = merges_summed("2 merges while unwatched");
        assert_eq!(notifier.sent(), [summary, merge_sent(9)]);
    }

    #[test]
    fn a_stale_summary_counts_no_merge_already_sent() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        gh.merged.borrow_mut().push(("2026-11-01T09:09:00Z", 7));
        world.lead_at(&notifier, &gh, LATER);

        world.lead_at(&notifier, &gh, "2026-11-01T12:00:00Z");

        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn a_merge_indexed_late_after_a_stale_summary_is_still_sent_once() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        gh.merged.borrow_mut().push(("2026-11-01T10:00:00Z", 7));
        world.lead_at(&notifier, &gh, "2026-11-01T12:00:00Z");
        let indexed_late = ("2026-11-01T11:58:00Z", 8);
        gh.merged.borrow_mut().push(indexed_late);

        world.lead_at(&notifier, &gh, "2026-11-01T12:01:00Z");

        let summary = merges_summed("1 merge while unwatched");
        assert_eq!(notifier.sent(), [summary, merge_sent(8)]);
    }

    #[test]
    fn a_stale_summary_whose_window_cannot_be_saved_is_sent_by_the_next_lookup() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        gh.merged.borrow_mut().push(("2026-11-01T10:00:00Z", 7));
        let blocker = world.reports().join("lead-cards.merges.new");
        fs::create_dir(&blocker).unwrap();
        world.lead_at(&notifier, &gh, "2026-11-01T12:00:00Z");
        fs::remove_dir(&blocker).unwrap();

        world.lead_at(&notifier, &gh, "2026-11-01T12:01:00Z");

        assert_eq!(notifier.sent(), [merges_summed("1 merge while unwatched")]);
    }

    #[test]
    fn a_failing_default_branch_refresh_looks_up_the_cached_branch() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        world.lead_at(&notifier, &gh, LATER);
        gh.fail_branch.set(true);
        gh.merged.borrow_mut().push(("2026-11-01T10:05:00Z", 7));

        let warned = world.lead_warned_at(&notifier, &gh, "2026-11-01T10:11:00Z");

        assert_eq!(warned, Vec::<String>::new());
        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn a_failing_default_branch_with_none_cached_warns() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        gh.fail_branch.set(true);

        let warned = world.lead_warned_at(&notifier, &gh, LATER);

        assert_eq!(warned, ["cards's merges: no default branch"]);
    }

    #[test]
    fn a_revive_recorded_while_the_clock_ran_ahead_skips_no_merges() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        let reports = world.reports();
        for (at, body) in [
            (
                "2026-11-01T11:00:00Z",
                Body::Gone {
                    tab: None,
                    workspace: None,
                },
            ),
            ("2026-11-01T11:30:00Z", Body::Start { resumed: true }),
        ] {
            append(
                &reports,
                "lead-cards",
                &Entry {
                    at: at.into(),
                    body,
                },
            )
            .unwrap();
        }
        gh.merged.borrow_mut().push(("2026-11-01T09:05:00Z", 7));
        world.lead_at(&notifier, &gh, LATER);
        gh.merged.borrow_mut().push(("2026-11-01T09:15:00Z", 8));

        world.lead_at(&notifier, &gh, "2026-11-01T09:20:00Z");

        assert_eq!(notifier.sent(), [merge_sent(7), merge_sent(8)]);
    }

    #[test]
    fn a_merge_lookup_held_off_by_wakes_for_a_quarter_hour_runs_on_a_waking_round() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        gh.merged.borrow_mut().push(("2026-11-01T09:05:00Z", 7));
        world.touch("merged-1", "");

        let woke = world.lead_wakes_at(&notifier, &gh, "2026-11-01T09:16:00Z");

        assert_eq!(woke.unwrap(), ["lh-1 merged"]);
        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn a_due_merge_lookup_waits_for_a_round_with_nothing_to_report() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        world.touch("merged-1", "");

        let woke = world.lead_wakes_at(&notifier, &gh, LATER);
        assert_eq!(woke.unwrap(), ["lh-1 merged"]);
        assert_eq!(gh.lookups.get(), 0, "looked up before waking");

        world.lead_at(&notifier, &gh, "2026-11-01T09:10:10Z");
        assert_eq!(gh.lookups.get(), 2, "the default branch, then the merges");
    }

    #[test]
    fn a_lookup_after_the_clock_jumps_back_still_runs_and_finds_recent_merges() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, "2026-11-01T12:00:00Z");
        world.lead_at(&notifier, &gh, "2026-11-01T12:01:00Z");
        gh.merged.borrow_mut().push(("2026-11-01T09:05:00Z", 7));

        world.lead_at(&notifier, &gh, LATER);

        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn a_merge_window_holding_a_bad_time_starts_over() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        let file = world.reports().join("lead-cards.merges");
        let mut state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        state["since"] = "not a time".into();
        fs::write(&file, state.to_string()).unwrap();

        world.lead_at(&notifier, &gh, LATER);
        gh.merged.borrow_mut().push(("2026-11-01T09:15:00Z", 7));
        world.lead_at(&notifier, &gh, "2026-11-01T09:20:00Z");

        assert_eq!(notifier.sent(), [merge_sent(7)]);
    }

    #[test]
    fn a_merge_window_that_cannot_be_saved_after_a_lookup_warns_once_a_streak() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        fs::create_dir(world.reports().join("lead-cards.merges.new")).unwrap();
        let own = |events: &[Waiting]| notifier.send(events);
        let req = Request {
            timeout: Some(Duration::from_secs(130)),
            notify_own: Some(&own),
            merges: Some(&gh),
            ..world.request("w9:p0")
        };
        let mut warned = vec![];

        let woke = watch(
            &world.herdr,
            &world.ledger,
            &world.reports(),
            &req,
            &mut steps_from(LATER, |_| {}),
            &mut |w| warned.push(w.to_string()),
        );

        assert!(woke.unwrap().is_none());
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("lead-cards.merges.new"), "{warned:?}");
    }

    #[test]
    fn a_watch_that_cannot_save_its_merge_window_still_holds_a_lookup_off_a_waking_round() {
        let (world, notifier, gh) = guarded("", vec![]);
        world.lead_at(&notifier, &gh, FIRST);
        fs::create_dir(world.reports().join("lead-cards.merges.new")).unwrap();
        let own = |events: &[Waiting]| notifier.send(events);
        let req = Request {
            timeout: Some(Duration::from_secs(130)),
            notify_own: Some(&own),
            merges: Some(&gh),
            ..world.request("w9:p0")
        };
        let mut looked_up_quietly = 0;

        let woke = watch(
            &world.herdr,
            &world.ledger,
            &world.reports(),
            &req,
            &mut steps_from("2026-11-01T09:20:00Z", |poll| {
                if poll == 5 {
                    looked_up_quietly = gh.lookups.get();
                    world.touch("merged-1", "");
                }
            }),
            &mut |_| {},
        );

        assert_eq!(printed(woke.unwrap().unwrap()), ["lh-1 merged"]);
        assert_eq!(looked_up_quietly, 2, "the default branch, then the merges");
        assert_eq!(gh.lookups.get(), 2, "looked up again on the waking round");
    }

    #[test]
    fn a_leads_watch_wakes_once_on_a_worker_merged_but_not_done() {
        let world = Checked::new();
        assert_eq!(world.once("w9:p0"), None);

        world.touch("merged-1", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-1 merged"]);
        assert_eq!(
            world.once("w9:p0"),
            None,
            "a second watch woke on the same merge"
        );
    }

    #[test]
    fn a_merged_worker_already_done_is_no_event_and_a_failing_check_is_no_merge() {
        let done = Checked::new();
        done.touch("merged-1", "");
        done.touch("done-1", "");
        assert_eq!(done.once("w9:p0"), None, "woke on a done worker");

        for (checks, failing) in [
            (
                "done = \"exit 1\"\nmerged = \"exit 3\"\n",
                "cards failing  lh-1 merged \"exit 3\": exit 3",
            ),
            (
                "done = \"exit 2\"\nmerged = \"exit 0\"\n",
                "cards failing  lh-1 done \"exit 2\": exit 2",
            ),
        ] {
            let world = Checked::with_template(checks);
            assert_eq!(world.once("w9:p0").unwrap(), [failing], "{checks}");
        }

        let no_done = Checked::with_template("merged = \"exit 0\"\n");
        assert_eq!(no_done.once("w9:p0").unwrap(), ["lh-1 merged"]);
    }

    #[test]
    fn a_leads_watch_wakes_once_per_card_feed_would_spawn() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n3\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        assert_eq!(
            world.once("w9:p0"),
            None,
            "a second watch woke on the same card"
        );

        world.touch("done-1", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-3 claimable"]);
    }

    #[test]
    fn a_done_worker_whose_after_merge_is_pending_frees_no_slot_and_wakes_the_lead_once_its_feed_is_gone()
     {
        let mut world = Checked::new();
        world.touch("tasks", "1\n2\n3\n");
        world.touch("done-1", "");
        let owner = crate::ledger::Process { pid: 7, start: 70 };
        let pending = Some(AfterMerge::Pending { owner });
        world.ledger.0.get_mut("lh-1").unwrap().after_merge = pending;
        let mut live = crate::process::FakeProcs::default();
        live.add(owner.pid, "/usr/bin/loopherd", 1, owner.start);
        let dir = world.config.path().join("reports");
        let once = |procs: &dyn Procs| {
            let req = Request {
                procs,
                ..world.request("w9:p0")
            };
            run(&world.herdr, &world.ledger, &dir, &req, &mut steps(|_| {}))
        };

        assert_eq!(once(&live).unwrap(), ["lh-2 claimable"]);
        let gone = crate::process::FakeProcs::default();
        assert_eq!(once(&gone).unwrap(), ["lh-1 interrupted"]);
        assert_eq!(once(&gone), None, "woke twice on one interrupted run");
    }

    #[test]
    fn a_merged_and_done_worker_whose_after_merge_is_due_frees_no_slot_and_wakes_the_lead() {
        for (max, woke) in [
            ("2", vec!["lh-1 merged", "lh-2 claimable"]),
            ("1", vec!["lh-1 merged"]),
        ] {
            let world = Checked::new();
            world.edit_herd(|herd| {
                format!("after_merge = [\"true\"]\n{herd}")
                    .replace("max = 2", &format!("max = {max}"))
            });
            world.touch("tasks", "1\n2\n3\n");
            world.touch("merged-1", "");
            world.touch("done-1", "");
            assert_eq!(world.once("w9:p0").unwrap(), woke, "max {max}");
        }
    }

    #[test]
    fn a_done_member_whose_role_is_from_another_version_frees_no_slot() {
        let mut world = Checked::with_template("done = \"exit 0\"\nmerged = \"exit 0\"\n");
        world.edit_herd(|herd| herd.replace("max = 2", "max = 1"));
        world.ledger.0.get_mut("lh-1").unwrap().role =
            Some(crate::ledger::Role::Unknown("reviewer".into()));
        world.touch("tasks", "1\n2\n3\n");
        assert_eq!(world.once("w9:p0"), None);
    }

    #[test]
    fn a_done_worker_frees_its_slot_unless_its_after_merge_is_due_or_its_merged_check_errors() {
        let none = None;
        let noted = Some(AfterMerge::Noted);
        let unknown = Some(AfterMerge::Unknown("skipped".into()));
        let frees = Some(vec!["lh-2 claimable".to_string()]);
        let errors = Some(vec![
            "cards failing  lh-1 merged \"exit 3\": exit 3".to_string(),
        ]);
        // (case, herd's after_merge, merged check, ledger's after_merge, what it wakes on)
        let cases = [
            (
                "no after_merge, merged",
                "",
                "exit 0",
                none.clone(),
                frees.clone(),
            ),
            (
                "after_merge, unmerged",
                "[\"true\"]",
                "exit 1",
                none.clone(),
                frees.clone(),
            ),
            (
                "after_merge, merged check errors",
                "[\"true\"]",
                "exit 3",
                none,
                errors,
            ),
            (
                "after_merge noted, merged",
                "[\"true\"]",
                "exit 0",
                noted,
                frees,
            ),
            (
                "after_merge from another version, merged",
                "[\"true\"]",
                "exit 0",
                unknown,
                None,
            ),
        ];
        for (case, after_merge, merged, recorded, woke) in cases {
            let mut world =
                Checked::with_template(&format!("done = \"exit 0\"\nmerged = \"{merged}\"\n"));
            world.edit_herd(|herd| {
                let herd = herd.replace("max = 2", "max = 1");
                match after_merge {
                    "" => herd,
                    commands => format!("after_merge = {commands}\n{herd}"),
                }
            });
            world.ledger.0.get_mut("lh-1").unwrap().after_merge = recorded;
            world.touch("tasks", "1\n2\n3\n");
            assert_eq!(world.once("w9:p0"), woke, "{case}");
        }
    }

    /// The ledger of a watch whose procs' `start` stands in for a feed that
    /// notes lh-1's after_merge and exits while the watch runs its checks.
    struct NotedMeanwhile(std::cell::RefCell<MemLedger>);

    impl Ledger for &NotedMeanwhile {
        fn entries(&self) -> Result<BTreeMap<String, AgentRecord>> {
            self.0.borrow().entries()
        }
        fn record(&mut self, record: AgentRecord) -> Result<()> {
            self.0.borrow_mut().record(record)
        }
        fn update(
            &mut self,
            name: &str,
            change: &mut dyn FnMut(&mut AgentRecord) -> Result<()>,
        ) -> Result<AgentRecord> {
            self.0.borrow_mut().update(name, change)
        }
        fn remove_herd(
            &mut self,
            _: &str,
            _: &mut dyn FnMut(&[String]) -> anyhow::Result<()>,
        ) -> anyhow::Result<crate::ledger::RemovedHerd> {
            unreachable!()
        }
        fn started(&mut self, name: &str, at: std::time::SystemTime, resumed: bool) -> Result<()> {
            self.0.borrow_mut().started(name, at, resumed)
        }
    }

    impl Procs for &NotedMeanwhile {
        fn exe(&self, _: u32) -> Option<PathBuf> {
            None
        }
        fn parent(&self, _: u32) -> Option<u32> {
            None
        }
        fn start(&self, _: u32) -> Option<u64> {
            let mut ledger = self.0.borrow_mut();
            ledger.0.get_mut("lh-1").unwrap().after_merge = Some(AfterMerge::Noted);
            None
        }
        fn cmdline(&self, _: u32) -> Option<Vec<String>> {
            None
        }
        fn resolve(&self, _: u32, _: &str) -> Option<PathBuf> {
            None
        }
    }

    #[test]
    fn a_pending_after_merge_its_feed_noted_during_the_checks_is_not_interrupted() {
        let mut world = Checked::new();
        world.touch("done-1", "");
        let owner = Process { pid: 7, start: 70 };
        world.ledger.0.get_mut("lh-1").unwrap().after_merge = Some(AfterMerge::Pending { owner });
        let shared = NotedMeanwhile(std::cell::RefCell::new(MemLedger(
            world.ledger.0.clone(),
            vec![],
        )));
        let req = Request {
            procs: &&shared,
            ..world.request("w9:p0")
        };
        let dir = world.config.path().join("reports");
        let woke = run(&world.herdr, &&shared, &dir, &req, &mut steps(|_| {}));
        assert_eq!(woke, None);
    }

    #[test]
    fn a_routed_queues_lead_wakes_on_a_member_merged_by_its_own_template_and_a_routed_claimable() {
        let mut world = Checked::new();
        world.edit_herd(|herd| {
            herd.replace(
                "[queue]\ntemplate = \"card\"",
                "[template.research]\nparams = [\"n\"]\nname = \"research-{n}\"\nprompt = \"go\"\n\
                 merged = \"test -f researched-{n}\"\n[queue]\ntemplates = [\"card\", \"research\"]",
            )
            .replace("max = 2", "max = 3")
        });
        let research = AgentRecord {
            template: Some("research".into()),
            params: Some([("n".to_string(), "2".to_string())].into()),
            ..member("research-2", "w9:p2", Some("cards"))
        };
        world.ledger.0.insert(research.name.clone(), research);
        world.touch("tasks", "research 2\nnope 5\ncard 3\nresearch 4\n");
        world.touch("merged-2", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-3 claimable"]);

        world.touch("researched-2", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["research-2 merged"]);
    }

    #[test]
    fn a_failing_source_wakes_the_lead_once_per_streak_and_again_after_it_recovers() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        let source = "cat tasks; echo first >&2; echo origin unreachable >&2; exit 2";
        let failing = format!("cards failing  source {source:?}: origin unreachable");
        world.edit_herd(|h| h.replace("\"cat tasks\"", &format!("{source:?}")));
        assert_eq!(world.once("w9:p0").unwrap(), std::slice::from_ref(&failing));
        assert_eq!(world.once("w9:p0"), None, "woke twice in one streak");

        world.edit_herd(|h| h.replace(&format!("{source:?}"), "\"cat tasks\""));
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards recovered  source", "lh-2 claimable"]
        );
        assert_eq!(world.once("w9:p0"), None, "woke twice on one recovery");

        world.edit_herd(|h| h.replace("\"cat tasks\"", &format!("{source:?}")));
        assert_eq!(world.once("w9:p0").unwrap(), [failing]);
    }

    #[test]
    fn a_failing_member_check_wakes_the_lead_once_per_streak_with_its_last_stderr_line() {
        let check =
            "test -f done-{n} && exit 0; echo checking >&2; echo gh: rate limited >&2; exit 4";
        let world = Checked::with_template(&format!("done = {check:?}\n"));
        let rendered = check.replace("{n}", "'1'");
        assert_eq!(
            world.once("w9:p0").unwrap(),
            [format!(
                "cards failing  lh-1 done {rendered:?}: gh: rate limited"
            )]
        );
        assert_eq!(world.once("w9:p0"), None, "woke twice in one streak");

        world.touch("done-1", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards recovered  lh-1 done"]);
        assert_eq!(world.once("w9:p0"), None, "woke twice on one recovery");
    }

    #[test]
    fn a_check_with_no_recorded_params_names_the_check_as_the_herd_file_writes_it() {
        let mut world = Checked::new();
        world.ledger.0.get_mut("lh-1").unwrap().params = None;
        assert_eq!(
            world.once("w9:p0").unwrap(),
            [
                "cards failing  lh-1 done \"test -f done-{n}\": no recorded params",
                "cards failing  lh-1 merged \"test -f merged-{n}\": no recorded params",
            ]
        );
    }

    #[test]
    fn a_failing_line_drops_escapes_and_control_characters_and_cuts_a_long_stderr_line() {
        let check = "printf '\\033[31mred\\033[0m \\a%0300d\\n' 0 >&2; exit 2";
        let world = Checked::with_template(&format!("done = {check:?}\n"));
        let woke = world.once("w9:p0").unwrap();
        let shown = format!("exit 2: red {}…", "0".repeat(WHY_MAX - 12));
        assert_eq!(
            woke,
            [format!("cards failing  lh-1 done {check:?}: {shown}")]
        );
    }

    #[test]
    fn a_leads_checks_run_every_60s_and_their_lines_sit_between_stops_and_blocked() {
        let world = Checked::new();
        let dir = world.config.path().join("reports");
        let req = Request {
            timeout: None,
            ..world.request("w9:p0")
        };
        let mut clock = steps(|polls| match polls {
            1 => world.touch("merged-1", ""),
            30 => {
                world.touch("tasks", "1\n2\n");
                world.herdr.set_status("lh-1", AgentStatus::Blocked);
                send(&world.ledger, &dir, "lh-1", Kind::Note, "hi");
            }
            _ => {}
        });

        let woke = run(&world.herdr, &world.ledger, &dir, &req, &mut clock).unwrap();

        assert_eq!(clock.elapsed, Duration::from_secs(60));
        assert_eq!(
            woke,
            [
                "lh-1 note 2026-11-01 09:00 UTC hi",
                "lh-1 merged",
                "lh-2 claimable",
                "lh-1 blocked",
            ]
        );
    }

    #[test]
    fn the_shepherds_watch_runs_no_merged_or_claimable_checks() {
        let world = Checked::new();
        world.touch("merged-1", "");
        world.touch("tasks", "1\n2\n");
        // lh-1's pane is a worker's, so its watch is the shepherd's.
        assert_eq!(world.once("w9:p1"), None);
        assert!(
            !world
                .config
                .path()
                .join("reports/watch-lead-cards.fired")
                .exists()
        );
    }

    #[test]
    fn a_herd_with_no_queue_still_wakes_on_merged() {
        let world = Checked::new();
        world.edit_herd(|herd| herd[..herd.find("[queue]").unwrap()].to_string());
        world.touch("merged-1", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-1 merged"]);
    }

    #[test]
    fn a_herd_file_the_checks_cannot_load_warns_once_and_wakes_nothing() {
        let world = Checked::with_template("merged = \"test -f {nope}\"\n");
        let dir = world.config.path().join("reports");
        let req = Request {
            timeout: Some(Duration::from_secs(150)),
            ..world.request("w9:p0")
        };
        let mut warnings = vec![];
        let woke = watch(
            &world.herdr,
            &world.ledger,
            &dir,
            &req,
            &mut steps(|_| {}),
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert!(woke.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("unknown placeholder {nope}"),
            "{warnings:?}"
        );
    }

    #[test]
    fn each_done_check_runs_once_per_round_of_checks() {
        let world =
            Checked::with_template("done = \"echo x >> ran-{n}; exit 1\"\nmerged = \"exit 1\"\n");
        world.touch("tasks", "1\n2\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        let ran = fs::read_to_string(world.repo.path().join("ran-1")).unwrap();
        assert_eq!(ran.lines().count(), 1);
    }

    #[test]
    fn no_card_is_claimable_while_a_feed_holds_the_feed_lock() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        let feeding = world.paths.lock_feed().unwrap();
        assert_eq!(world.once("w9:p0"), None);
        drop(feeding);
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
    }

    #[test]
    fn only_a_leads_watch_reads_the_check_timeout() {
        let world = Checked::new();
        world.touch("merged-1", "");
        let broken = |pane| Request {
            check_timeout: || bail!("bad LOOPHERD_GIT_TIMEOUT"),
            ..world.request(pane)
        };
        let dir = world.config.path().join("reports");
        let shepherd = watch(
            &world.herdr,
            &world.ledger,
            &dir,
            &broken("w9:p1"),
            &mut steps(|_| {}),
            &mut |_| {},
        );
        assert!(shepherd.unwrap().is_none());
        let Err(err) = watch(
            &world.herdr,
            &world.ledger,
            &dir,
            &broken("w9:p0"),
            &mut steps(|_| {}),
            &mut |_| {},
        ) else {
            panic!("a lead's watch ran with a bad check timeout");
        };
        assert!(
            err.to_string().contains("bad LOOPHERD_GIT_TIMEOUT"),
            "{err}"
        );
    }

    #[test]
    fn fired_keys_saved_by_overlapping_watches_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let first = Fired::load(dir.path(), "lead-cards", "t0").unwrap();
        let second = Fired::load(dir.path(), "lead-cards", "t0").unwrap();
        first
            .with(&[Event::new("lh-1 merged".into())])
            .save()
            .unwrap();
        second
            .with(&[Event::new("lh-2 claimable".into())])
            .save()
            .unwrap();
        let keys = Fired::load(dir.path(), "lead-cards", "t0").unwrap().keys;
        assert_eq!(
            keys,
            HashSet::from(["lh-1 merged".to_string(), "lh-2 claimable".to_string()])
        );
    }

    #[test]
    fn a_round_of_checks_stops_running_commands_at_its_deadline() {
        let world = Checked::with_template("done = \"sleep 2; touch ran-{n}\"\n");
        let (checks, _) = lead_checks(&world.paths, "cards").unwrap();
        let worker = &world.ledger.0["lh-1"];
        let started = Instant::now();
        let budget = Budget {
            deadline: started + Duration::from_millis(300),
            each: Duration::from_secs(5),
        };

        let cut = run_member_check(&checks.done, &checks, worker, &budget);
        let after = run_member_check(&checks.done, &checks, worker, &budget);

        assert!(matches!(cut, Some(Err(Unchecked::OutOfTime))));
        assert!(matches!(after, Some(Err(Unchecked::OutOfTime))));
        assert!(started.elapsed() < Duration::from_secs(2));
        // Past the check's own `sleep 2`, so a check left running would have touched it.
        std::thread::sleep(Duration::from_millis(2200).saturating_sub(started.elapsed()));
        assert!(!world.repo.path().join("ran-1").exists());
    }

    #[test]
    fn a_check_that_outruns_its_own_timeout_before_the_deadline_is_failing() {
        let world = Checked::with_template("done = \"sleep 3\"\n");
        let (checks, _) = lead_checks(&world.paths, "cards").unwrap();
        let worker = &world.ledger.0["lh-1"];
        let budget = Budget {
            deadline: Instant::now() + Duration::from_secs(30),
            each: Duration::from_millis(300),
        };

        let timed_out = run_member_check(&checks.done, &checks, worker, &budget);

        assert!(
            matches!(&timed_out, Some(Err(Unchecked::Failed(why))) if why.contains("timed out")),
            "{:?}",
            timed_out.map(|r| r.map_err(|e| matches!(e, Unchecked::OutOfTime)))
        );
    }

    #[test]
    fn a_re_created_lead_wakes_on_what_its_predecessor_was_told() {
        let mut world = Checked::new();
        world.touch("merged-1", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-1 merged"]);

        let lead = world.ledger.0.get_mut("lead-cards").unwrap();
        lead.created = "2026-11-02T00:00:00Z".into();
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-1 merged"]);
        assert_eq!(world.once("w9:p0"), None);
    }

    impl Checked {
        fn pause(&self) {
            let herds = crate::pause::Herds::One("cards");
            crate::pause::pause(&self.paths, herds, None, SystemTime::now()).unwrap();
        }

        fn resume(&self) {
            let herds = crate::pause::Herds::One("cards");
            let reports = self.config.path().join("reports");
            crate::pause::resume(&self.paths, &self.ledger, &reports, herds).unwrap();
        }
    }

    #[test]
    fn a_paused_herd_wakes_its_lead_on_merged_but_not_claimable() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        world.touch("merged-1", "");
        world.pause();
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-1 merged"]);
        assert_eq!(world.once("w9:p0"), None, "woke on a paused herd's card");
    }

    #[test]
    fn the_first_check_after_resume_wakes_it_on_claimable() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        world.pause();
        let dir = world.config.path().join("reports");
        let mut clock = steps(|poll| {
            if poll == 3 {
                world.resume();
            }
        });
        let req = Request {
            timeout: Some(Duration::from_secs(150)),
            ..world.request("w9:p0")
        };
        let woke = run(&world.herdr, &world.ledger, &dir, &req, &mut clock);
        assert_eq!(woke.unwrap(), ["lh-2 claimable"]);
        assert_eq!(
            clock.elapsed, CHECK_EVERY,
            "woke after the first check past resume"
        );
    }

    #[test]
    fn a_leads_watch_wakes_on_an_attended_members_merge_and_frees_no_slot_when_it_is_done() {
        let mut world = Checked::new();
        for n in ["2", "9"] {
            let mut member = AgentRecord {
                template: Some("card".into()),
                params: Some([("n".to_string(), n.to_string())].into()),
                ..member(&format!("lh-{n}"), &format!("w9:p{n}"), Some("cards"))
            };
            if n == "9" {
                member.attended = Some(Default::default());
            }
            world.ledger.0.insert(member.name.clone(), member);
            world.herdr =
                world
                    .herdr
                    .with_live_agent(Some(&format!("lh-{n}")), None, &format!("w9:p{n}"));
        }
        world.touch("tasks", "1\n2\n3\n");
        world.touch("merged-9", "");
        world.touch("done-9", "");

        assert_eq!(world.once("w9:p0").unwrap(), ["lh-9 merged"]);
        assert_eq!(world.once("w9:p0"), None);
    }

    #[test]
    fn a_card_announced_before_a_pause_is_announced_again_after_resume() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        world.touch("merged-1", "");
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["lh-1 merged", "lh-2 claimable"]
        );
        world.pause();
        assert_eq!(world.once("w9:p0"), None);
        assert_eq!(world.once("w9:p0"), None);
        world.resume();
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        assert_eq!(
            world.once("w9:p0"),
            None,
            "announced twice after one resume"
        );
    }

    #[test]
    fn an_unreadable_pause_counts_as_paused_and_warns_once() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        fs::create_dir_all(&world.paths.paused).unwrap();
        fs::write(world.paths.paused.join("cards"), "not json").unwrap();
        let dir = world.config.path().join("reports");
        let req = Request {
            timeout: Some(Duration::from_secs(150)),
            ..world.request("w9:p0")
        };
        let mut warnings = vec![];
        let woke = watch(
            &world.herdr,
            &world.ledger,
            &dir,
            &req,
            &mut steps(|_| {}),
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert!(woke.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("cards's pause is unreadable"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_pause_no_watch_saw_still_re_announces_on_resume() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        world.pause();
        world.resume();
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
    }

    #[test]
    fn a_running_watch_re_announces_at_its_first_check_after_resume() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        let dir = world.config.path().join("reports");
        let mut clock = steps(|poll| match poll {
            3 => world.pause(),
            5 => world.resume(),
            _ => {}
        });
        let req = Request {
            timeout: Some(Duration::from_secs(150)),
            ..world.request("w9:p0")
        };
        let woke = run(&world.herdr, &world.ledger, &dir, &req, &mut clock);
        assert_eq!(woke.unwrap(), ["lh-2 claimable"]);
        assert_eq!(clock.elapsed, CHECK_EVERY);
    }

    #[test]
    fn a_released_member_is_announced_claimable_again() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        let dir = world.config.path().join("reports");

        forget_member(&dir, "lead-cards", "lh-2").unwrap();

        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
    }

    #[test]
    fn a_release_forgets_only_its_members_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = fired_path(dir.path(), "lead-cards");
        let keys = [
            "lh-2 claimable",
            "lh-2 merged",
            "cards failing lh-2 done",
            "lh-21 claimable",
            "cards failing lh-21 done",
            "cards failing source",
        ];
        fs::write(&path, format!("lead t\n{}\n", keys.join("\n"))).unwrap();

        forget_member(dir.path(), "lead-cards", "lh-2").unwrap();

        let fired = Fired::load(dir.path(), "lead-cards", "t").unwrap();
        let mut kept: Vec<&str> = fired.keys.iter().map(String::as_str).collect();
        kept.sort();
        assert_eq!(
            kept,
            [
                "cards failing lh-21 done",
                "cards failing source",
                "lh-21 claimable"
            ]
        );
    }

    #[test]
    fn resume_leaves_a_lead_with_no_fired_file_without_one() {
        let dir = tempfile::tempdir().unwrap();
        forget_claimable(dir.path(), "lead-cards").unwrap();
        assert!(!fired_path(dir.path(), "lead-cards").exists());
    }

    #[test]
    fn a_resume_during_the_member_checks_counts_in_the_same_round() {
        let world = Checked::with_template("done = \"sh resume.sh; exit 1\"\n");
        let fired = world.config.path().join("reports/watch-lead-cards.fired");
        let resume = format!(
            "rm -f {:?}; echo resumed >> {:?}\n",
            world.paths.paused.join("cards"),
            fired
        );
        world.touch("resume.sh", "");
        world.touch("tasks", "1\n2\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        world.pause();
        world.touch("resume.sh", &resume);
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
    }

    impl Checked {
        fn retire(&mut self, name: &str) {
            self.ledger.0.get_mut(name).unwrap().retired = true;
        }
    }

    impl Checked {
        /// The herd's queue gains `backlog = "cat unready"`, listing `unready`.
        fn with_backlog(unready: &str) -> Checked {
            let world = Checked::new();
            world.edit_herd(|h| h.replace("max = 2", "backlog = \"cat unready\"\nmax = 2"));
            world.touch("unready", unready);
            world
        }
    }

    #[test]
    fn a_herd_low_on_claimable_tasks_with_unready_ones_wakes_its_lead_once_per_streak() {
        let world = Checked::with_backlog("7 needs spec\n8 stale\n");
        world.touch("tasks", "1\n2\n");
        assert_eq!(
            world.once("w9:p0").unwrap(),
            [
                "cards running low  1 claimable, 2 unready",
                "lh-2 claimable"
            ]
        );

        world.touch("unready", "7 needs spec\n");
        assert_eq!(world.once("w9:p0"), None, "woke twice in one streak");
    }

    #[test]
    fn the_streak_ends_when_claimable_rises_above_max_or_unready_reaches_zero() {
        let world = Checked::with_backlog("7 needs spec\n");
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards running low  0 claimable, 1 unready"]
        );

        world.touch("tasks", "2\n3\n4\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        world.touch("tasks", "");
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards running low  0 claimable, 1 unready"]
        );

        world.touch("unready", "");
        assert_eq!(world.once("w9:p0"), None);
        world.touch("unready", "9 stale\n");
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards running low  0 claimable, 1 unready"]
        );
    }

    #[test]
    fn a_herd_at_max_claimable_is_low_and_one_with_no_backlog_or_unready_is_not() {
        let at_max = Checked::with_backlog("7 needs spec\n");
        at_max.touch("tasks", "2\n3\n");
        assert_eq!(
            at_max.once("w9:p0").unwrap(),
            [
                "cards running low  2 claimable, 1 unready",
                "lh-2 claimable"
            ]
        );

        assert_eq!(Checked::with_backlog("").once("w9:p0"), None);
        assert_eq!(Checked::new().once("w9:p0"), None);
    }

    #[test]
    fn dropping_the_backlog_ends_the_streak() {
        let world = Checked::with_backlog("7 needs spec\n");
        assert!(world.once("w9:p0").is_some());
        world.edit_herd(|h| h.replace("backlog = \"cat unready\"\n", ""));
        assert_eq!(world.once("w9:p0"), None);

        world.edit_herd(|h| h.replace("max = 2", "backlog = \"cat unready\"\nmax = 2"));
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards running low  0 claimable, 1 unready"]
        );
    }

    #[test]
    fn a_paused_herd_says_nothing_and_keeps_its_streak_through_resume() {
        let mid_streak = Checked::with_backlog("7 needs spec\n");
        assert!(mid_streak.once("w9:p0").is_some());
        mid_streak.pause();
        assert_eq!(mid_streak.once("w9:p0"), None);
        mid_streak.resume();
        assert_eq!(mid_streak.once("w9:p0"), None, "resume re-announced it");

        let paused_first = Checked::with_backlog("7 needs spec\n");
        paused_first.pause();
        assert_eq!(paused_first.once("w9:p0"), None);
        paused_first.resume();
        assert_eq!(
            paused_first.once("w9:p0").unwrap(),
            ["cards running low  0 claimable, 1 unready"]
        );
    }

    #[test]
    fn the_shepherds_watch_never_says_running_low() {
        let world = Checked::with_backlog("7 needs spec\n");
        assert_eq!(world.once("w9:p1"), None);
    }

    const STARVED: &str = "cards running low  0 claimable, 1 unready, no workers open";

    #[test]
    fn a_herd_that_empties_mid_streak_says_running_low_again_once() {
        let mut world = Checked::with_backlog("7 needs spec\n");
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards running low  0 claimable, 1 unready"]
        );
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);
        assert_eq!(world.once("w9:p0"), None, "woke twice in one streak");
    }

    #[test]
    fn a_herd_starved_from_the_start_says_one_line_and_keeps_the_plain_streak() {
        let mut world = Checked::with_backlog("7 needs spec\n");
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);
        world.ledger.0.get_mut("lh-1").unwrap().retired = false;
        assert_eq!(world.once("w9:p0"), None, "the plain streak re-announced");
    }

    #[test]
    fn the_starved_streak_ends_when_a_worker_opens_claimable_rises_or_unready_empties() {
        let mut world = Checked::with_backlog("7 needs spec\n");
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);

        world.ledger.0.get_mut("lh-1").unwrap().retired = false;
        assert_eq!(world.once("w9:p0"), None);
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);

        world.spawn_and_retire("lh-3");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);

        world.touch("tasks", "2\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        world.touch("tasks", "");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);

        world.touch("unready", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);
        world.touch("unready", "7 needs spec\n");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);
    }

    #[test]
    fn dropping_the_backlog_ends_the_starved_streak() {
        let mut world = Checked::with_backlog("7 needs spec\n");
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);
        world.edit_herd(|h| h.replace("backlog = \"cat unready\"\n", ""));
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);
        world.edit_herd(|h| h.replace("max = 2", "backlog = \"cat unready\"\nmax = 2"));
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);
    }

    impl Checked {
        /// One poll from `pane` whose wake is lost before it's marked read.
        fn lose(&self, pane: &'static str) {
            let dir = self.config.path().join("reports");
            let req = self.request(pane);
            let woke = watch(
                &self.herdr,
                &self.ledger,
                &dir,
                &req,
                &mut steps(|_| {}),
                &mut |_| {},
            );
            assert!(woke.unwrap().is_some());
        }
    }

    #[test]
    fn a_lost_starved_wake_leaves_the_plain_line_to_print() {
        let mut world = Checked::with_backlog("7 needs spec\n");
        world.retire("lh-1");
        world.lose("w9:p0");
        world.ledger.0.get_mut("lh-1").unwrap().retired = false;
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards running low  0 claimable, 1 unready"]
        );
    }

    #[test]
    fn a_paused_starved_herd_says_neither_and_keeps_its_streaks() {
        let mut mid_streak = Checked::with_backlog("7 needs spec\n");
        mid_streak.retire("lh-1");
        assert_eq!(mid_streak.once("w9:p0").unwrap(), [STARVED]);
        mid_streak.pause();
        assert_eq!(mid_streak.once("w9:p0").unwrap(), ["cards drained"]);
        mid_streak.resume();
        assert_eq!(mid_streak.once("w9:p0"), None, "resume re-announced it");

        let mut paused_first = Checked::with_backlog("7 needs spec\n");
        paused_first.retire("lh-1");
        paused_first.pause();
        assert_eq!(paused_first.once("w9:p0").unwrap(), ["cards drained"]);
        paused_first.resume();
        assert_eq!(paused_first.once("w9:p0").unwrap(), [STARVED]);
    }

    #[test]
    fn the_shepherds_watch_never_says_a_starved_herd_is_running_low() {
        let mut world = Checked::with_backlog("7 needs spec\n");
        world.retire("lh-1");
        world.retire("lead-cards");
        assert_eq!(world.shepherd(), None);
        world.touch("unready", "");
        assert_eq!(
            world.shepherd().unwrap(),
            ["cards out of work"],
            "herd unchecked"
        );
    }

    #[test]
    fn a_failing_backlog_wakes_the_lead_once_per_streak_and_says_nothing_low() {
        let world = Checked::with_backlog("7 needs spec\n");
        world.edit_herd(|h| h.replace("\"cat unready\"", "\"echo gone >&2; exit 3\""));
        assert_eq!(
            world.once("w9:p0").unwrap(),
            ["cards failing  backlog \"echo gone >&2; exit 3\": failed (exit status: 3): gone"]
        );
        world.edit_herd(|h| h.replace("\"echo gone >&2; exit 3\"", "\"cat unready\""));
        assert_eq!(
            world.once("w9:p0").unwrap(),
            [
                "cards recovered  backlog",
                "cards running low  0 claimable, 1 unready"
            ]
        );
    }

    #[test]
    fn an_unpaused_herd_with_no_work_and_no_open_worker_wakes_its_lead_out_of_work() {
        let mut no_backlog = Checked::new();
        no_backlog.retire("lh-1");
        assert_eq!(no_backlog.once("w9:p0").unwrap(), ["cards out of work"]);

        let mut empty_backlog = Checked::with_backlog("");
        empty_backlog.retire("lh-1");
        assert_eq!(empty_backlog.once("w9:p0").unwrap(), ["cards out of work"]);
    }

    #[test]
    fn out_of_work_prints_once_per_streak() {
        let mut world = Checked::with_backlog("");
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);
        assert_eq!(world.once("w9:p0"), None, "woke twice in one streak");
    }

    #[test]
    fn out_of_work_prints_again_once_work_or_a_worker_comes_and_goes() {
        let mut world = Checked::with_backlog("");
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);

        world.touch("tasks", "2\n");
        assert_eq!(world.once("w9:p0").unwrap(), ["lh-2 claimable"]);
        world.touch("tasks", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);

        world.touch("unready", "7 needs spec\n");
        assert_eq!(world.once("w9:p0").unwrap(), [STARVED]);
        world.touch("unready", "");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);

        world.ledger.0.get_mut("lh-1").unwrap().retired = false;
        assert_eq!(world.once("w9:p0"), None);
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);
    }

    #[test]
    fn a_paused_herd_is_not_out_of_work_and_keeps_its_streak_through_resume() {
        let mut paused_first = Checked::new();
        paused_first.retire("lh-1");
        paused_first.pause();
        assert_eq!(paused_first.once("w9:p0").unwrap(), ["cards drained"]);
        paused_first.resume();
        assert_eq!(paused_first.once("w9:p0").unwrap(), ["cards out of work"]);

        paused_first.pause();
        paused_first.resume();
        assert_eq!(paused_first.once("w9:p0"), None, "resume re-announced it");
    }

    impl Checked {
        /// A worker of cards spawned after the others and already retired,
        /// as if it opened and closed while no watch ran.
        fn spawn_and_retire(&mut self, name: &str) {
            let worker = AgentRecord {
                created: "2026-11-02T00:00:00Z".into(),
                retired: true,
                ..member(name, "w9:p5", Some("cards"))
            };
            self.ledger.0.insert(name.into(), worker);
        }
    }

    #[test]
    fn a_worker_that_opened_and_retired_between_watches_ends_the_leads_streak() {
        let mut world = Checked::new();
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);
        world.spawn_and_retire("lh-2");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);
        assert_eq!(world.once("w9:p0"), None);
    }

    #[test]
    fn a_failing_source_or_backlog_is_not_out_of_work() {
        let mut source = Checked::new();
        source.retire("lh-1");
        source.edit_herd(|h| h.replace("\"cat tasks\"", "\"exit 3\""));
        assert_eq!(
            source.once("w9:p0").unwrap(),
            ["cards failing  source \"exit 3\": failed (exit status: 3)"]
        );

        let mut backlog = Checked::with_backlog("");
        backlog.retire("lh-1");
        backlog.edit_herd(|h| h.replace("\"cat unready\"", "\"exit 3\""));
        assert_eq!(
            backlog.once("w9:p0").unwrap(),
            ["cards failing  backlog \"exit 3\": failed (exit status: 3)"]
        );
    }

    #[test]
    fn a_leads_watch_over_a_paused_herd_with_no_open_worker_says_drained() {
        let mut world = Checked::new();
        world.retire("lh-1");
        world.pause();
        assert_eq!(world.once("w9:p0").unwrap(), ["cards drained"]);
    }

    #[test]
    fn a_leads_watch_is_not_drained_with_a_worker_open_or_its_herd_unpaused() {
        let done_not_retired = Checked::new();
        done_not_retired.touch("done-1", "");
        done_not_retired.pause();
        assert_eq!(done_not_retired.once("w9:p0"), None);

        let mut unpaused = Checked::new();
        unpaused.retire("lh-1");
        assert_eq!(unpaused.once("w9:p0").unwrap(), ["cards out of work"]);

        let mut unreadable = Checked::new();
        unreadable.retire("lh-1");
        fs::create_dir_all(&unreadable.paths.paused).unwrap();
        fs::write(unreadable.paths.paused.join("cards"), "not json").unwrap();
        assert_eq!(unreadable.once("w9:p0"), None);
    }

    #[test]
    fn a_resume_between_polls_leaves_no_drained_line() {
        let world = Checked::new();
        let state = tempfile::tempdir().unwrap();
        let mut file = FileLedger::new(state.path().join("ledger.json"));
        for entry in world.ledger.0.values() {
            file.record(entry.clone()).unwrap();
        }
        world.pause();
        let dir = world.config.path().join("reports");
        let mut clock = steps(|poll| {
            if poll == 1 {
                let mut ledger = FileLedger::new(state.path().join("ledger.json"));
                ledger
                    .update("lh-1", &mut |e| {
                        e.retired = true;
                        Ok(())
                    })
                    .unwrap();
                world.resume();
            }
        });
        let req = Request {
            timeout: Some(Duration::from_secs(10)),
            ..world.request("w9:p0")
        };
        assert_eq!(run(&world.herdr, &file, &dir, &req, &mut clock), None);
    }

    impl Checked {
        /// One poll of a fresh shepherd's watch.
        fn shepherd(&self) -> Option<Vec<String>> {
            let dir = self.config.path().join("reports");
            let req = self.shepherd_request();
            run(&self.herdr, &self.ledger, &dir, &req, &mut steps(|_| {}))
        }

        fn shepherd_request(&self) -> Request<'_> {
            Request {
                herd_files: Some(&self.paths),
                check_timeout: || Ok(Duration::from_secs(5)),
                ..ONCE
            }
        }
    }

    #[test]
    fn the_shepherds_watch_says_all_drained_once_every_herd_is_paused_and_drained() {
        let mut world = Checked::new();
        world.retire("lh-1");
        world.pause();
        world.herdr.set_status("lead-cards", AgentStatus::Idle);
        assert_eq!(world.shepherd().unwrap(), ["all drained"]);
    }

    impl Checked {
        /// A paused herd whose only member left is its idle lead.
        fn drained() -> Checked {
            let mut world = Checked::new();
            world.retire("lh-1");
            world.pause();
            world.herdr.set_status("lead-cards", AgentStatus::Idle);
            world
        }

        fn add(&mut self, record: AgentRecord, status: AgentStatus) {
            let herdr = std::mem::replace(&mut self.herdr, FakeHerdr::new(&[]));
            self.herdr = herdr.with_live_agent(Some(&record.name), None, &record.pane_id);
            self.herdr.set_status(&record.name, status);
            self.ledger.0.insert(record.name.clone(), record);
        }
    }

    impl Checked {
        /// The herd with neither its lead nor its worker open.
        fn unled() -> Checked {
            let mut world = Checked::new();
            world.retire("lead-cards");
            world.retire("lh-1");
            world
        }
    }

    #[test]
    fn an_unled_herd_out_of_work_wakes_the_shepherd_once_per_streak() {
        let world = Checked::unled();
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);
        assert_eq!(world.shepherd(), None, "woke twice in one streak");

        world.touch("tasks", "2\n");
        assert_eq!(world.shepherd(), None);
        world.touch("tasks", "");
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);
    }

    #[test]
    fn the_shepherd_says_out_of_work_again_after_unready_work_or_a_worker() {
        let mut world = Checked::with_backlog("");
        world.retire("lead-cards");
        world.retire("lh-1");
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);

        world.touch("unready", "7 needs spec\n");
        assert_eq!(world.shepherd(), None);
        world.touch("unready", "");
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);

        world.ledger.0.get_mut("lh-1").unwrap().retired = false;
        world.herdr.set_status("lh-1", AgentStatus::Idle);
        assert_eq!(world.shepherd(), None);
        world.retire("lh-1");
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);
    }

    #[test]
    fn a_lead_or_worker_opening_ends_the_shepherds_streak_even_outside_its_scope() {
        let mut led_spell = Checked::unled();
        assert_eq!(led_spell.shepherd().unwrap(), ["cards out of work"]);
        led_spell.ledger.0.get_mut("lead-cards").unwrap().retired = false;
        led_spell.herdr.set_status("lead-cards", AgentStatus::Idle);
        assert_eq!(led_spell.shepherd(), None);
        led_spell.retire("lead-cards");
        assert_eq!(led_spell.shepherd().unwrap(), ["cards out of work"]);

        let mut scoped = Checked::unled();
        assert_eq!(scoped.shepherd().unwrap(), ["cards out of work"]);
        scoped.ledger.0.get_mut("lh-1").unwrap().retired = false;
        let dir = scoped.config.path().join("reports");
        let req = Request {
            herds: &["prs".to_string()],
            ..scoped.shepherd_request()
        };
        run(
            &scoped.herdr,
            &scoped.ledger,
            &dir,
            &req,
            &mut steps(|_| {}),
        );
        scoped.retire("lh-1");
        assert_eq!(scoped.shepherd().unwrap(), ["cards out of work"]);
    }

    #[test]
    fn a_worker_that_opened_and_retired_between_watches_ends_the_shepherds_streak() {
        let mut world = Checked::unled();
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);
        world.spawn_and_retire("lh-2");
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);
        assert_eq!(world.shepherd(), None);
    }

    #[test]
    fn the_shepherd_doesnt_repeat_what_a_retired_lead_said_out_of_work() {
        let mut world = Checked::new();
        world.retire("lh-1");
        assert_eq!(world.once("w9:p0").unwrap(), ["cards out of work"]);
        world.retire("lead-cards");
        assert_eq!(world.shepherd(), None, "the lead already said it");

        world.spawn_and_retire("lh-2");
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);
    }

    #[test]
    fn a_pending_blocked_line_wakes_the_shepherd_before_its_queue_commands_run() {
        let mut world = Checked::unled();
        world.add(member("solo", "w9:p3", None), AgentStatus::Idle);
        let dir = world.config.path().join("reports");
        crate::pane_event::record_blocked(&dir, "solo", SystemTime::now()).unwrap();
        world.edit_herd(|h| h.replace("\"cat tasks\"", "\"touch ran; cat tasks\""));

        assert_eq!(world.shepherd().unwrap(), ["solo blocked"]);
        assert!(
            !world.repo.path().join("ran").exists(),
            "ran the source first"
        );
        assert_eq!(world.shepherd().unwrap(), ["cards out of work"]);
    }

    #[test]
    fn the_shepherd_warns_once_when_an_unled_herd_file_is_bad() {
        let world = Checked::unled();
        world.edit_herd(|h| h.replace("[queue]", "[queue"));
        let dir = world.config.path().join("reports");
        let req = Request {
            timeout: Some(Duration::from_secs(150)),
            ..world.shepherd_request()
        };
        let mut warnings = vec![];
        let woke = watch(
            &world.herdr,
            &world.ledger,
            &dir,
            &req,
            &mut steps(|_| {}),
            &mut |w| warnings.push(w.to_string()),
        )
        .unwrap();
        assert!(woke.is_none());
        let cards: Vec<_> = warnings.iter().filter(|w| w.contains("cards")).collect();
        assert_eq!(cards.len(), 1, "{warnings:?}");
    }

    #[test]
    fn the_shepherd_never_says_out_of_work_for_a_led_paused_or_unread_herd() {
        let mut led = Checked::new();
        led.retire("lh-1");
        led.herdr.set_status("lead-cards", AgentStatus::Idle);
        assert_eq!(led.shepherd(), None, "a led herd is its lead's");

        let paused = Checked::unled();
        paused.pause();
        assert_eq!(paused.shepherd(), None, "a paused herd");

        let other = Checked::unled();
        let dir = other.config.path().join("reports");
        let req = Request {
            herds: &["prs".to_string()],
            ..other.shepherd_request()
        };
        let woke = run(&other.herdr, &other.ledger, &dir, &req, &mut steps(|_| {}));
        assert_eq!(woke, None, "a herd the watch doesn't read");
    }

    #[test]
    fn a_member_in_no_herd_drains_with_the_herds_only_once_it_stops_working() {
        let mut world = Checked::drained();
        world.add(member("solo", "w9:p3", None), AgentStatus::Working);
        assert_eq!(world.shepherd(), None);

        world.herdr.set_status("solo", AgentStatus::Idle);
        assert_eq!(world.shepherd().unwrap(), ["all drained"]);
    }

    #[test]
    fn the_shepherds_watch_is_not_drained_while_anything_it_reads_may_still_work() {
        let working_lead = Checked::drained();
        working_lead
            .herdr
            .set_status("lead-cards", AgentStatus::Working);
        assert_eq!(working_lead.shepherd(), None, "working lead");

        let mut open_worker = Checked::drained();
        open_worker.ledger.0.get_mut("lh-1").unwrap().retired = false;
        assert_eq!(open_worker.shepherd(), None, "open worker");

        let unpaused = Checked::drained();
        unpaused.resume();
        assert_eq!(unpaused.shepherd(), None, "unpaused herd");

        let mut unled = Checked::drained();
        unled.add(member("zpr-2", "w9:p2", Some("prs")), AgentStatus::Idle);
        assert_eq!(unled.shepherd(), None, "a member of an unled herd");

        let mut no_herds = Checked::new();
        no_herds.ledger.0.clear();
        no_herds.pause();
        assert_eq!(no_herds.shepherd(), None, "nothing to drain");

        let shell = Checked::drained();
        put(
            &shell.config.path().join("reports"),
            "lead-cards",
            stop(1, 0),
        );
        assert_eq!(
            shell.shepherd().unwrap(),
            ["lead-cards paused  1 shell"],
            "the stop wakes it first"
        );
        assert_eq!(shell.shepherd(), None, "a lead with a shell in flight");

        let unreadable = Checked::drained();
        let reports = unreadable.config.path().join("reports");
        fs::create_dir_all(reports.join("lead-cards/mailbox.jsonl")).unwrap();
        assert_eq!(unreadable.shepherd(), None, "an unreadable mailbox");
    }

    #[test]
    fn a_resume_between_the_shepherds_polls_leaves_no_drained_line() {
        let world = Checked::drained();
        world.herdr.set_status("lead-cards", AgentStatus::Working);
        let dir = world.config.path().join("reports");
        let mut clock = steps(|poll| {
            if poll == 1 {
                world.herdr.set_status("lead-cards", AgentStatus::Idle);
                world.resume();
            }
        });
        let req = Request {
            timeout: Some(Duration::from_secs(10)),
            herd_files: Some(&world.paths),
            ..EVERY
        };
        assert_eq!(
            run(&world.herdr, &world.ledger, &dir, &req, &mut clock),
            None
        );
    }

    impl Checked {
        /// A shepherd's watch polling until it wakes, `step` run between polls.
        fn shepherd_until(
            &self,
            timeout: Option<Duration>,
            step: impl FnMut(u32),
        ) -> (Option<Vec<String>>, u32) {
            let dir = self.config.path().join("reports");
            let req = Request {
                timeout,
                herd_files: Some(&self.paths),
                ..EVERY
            };
            let mut clock = steps(step);
            let woke = run(&self.herdr, &self.ledger, &dir, &req, &mut clock);
            (woke, clock.polls)
        }
    }

    #[test]
    fn a_member_dying_while_watched_wakes_it_gone_not_drained() {
        let mut solo = Checked::drained();
        solo.add(member("solo", "w9:p3", None), AgentStatus::Working);
        let (woke, _) = solo.shepherd_until(None, |poll| {
            if poll == 1 {
                solo.herdr.remove_agent("solo");
            }
        });
        assert_eq!(woke.unwrap(), ["solo gone"]);

        let lead = Checked::drained();
        lead.herdr.set_status("lead-cards", AgentStatus::Working);
        let (woke, _) = lead.shepherd_until(None, |poll| {
            if poll == 1 {
                lead.herdr.remove_agent("lead-cards");
            }
        });
        assert_eq!(woke.unwrap(), ["lead-cards gone"]);
    }

    #[test]
    fn a_member_left_unmatched_by_a_herdr_restart_keeps_the_shepherd_watching() {
        let world = Checked::drained();
        world.herdr.set_status("lead-cards", AgentStatus::Working);
        let (woke, _) = world.shepherd_until(Some(Duration::from_secs(10)), |poll| match poll {
            1 => world.herdr.set_unreachable(true),
            2 => {
                world.herdr.remove_agent("lead-cards");
                world.herdr.set_unreachable(false);
            }
            _ => {}
        });
        assert_eq!(woke, None);
    }

    #[test]
    fn a_member_gone_when_the_shepherds_watch_starts_still_drains() {
        let world = Checked::drained();
        world.herdr.remove_agent("lead-cards");
        assert_eq!(world.shepherd().unwrap(), ["all drained"]);
    }

    #[test]
    fn a_member_whose_last_stop_failed_is_in_flight() {
        let world = Checked::drained();
        let failed = Body::Stop(Stop {
            session: None,
            background: Tally::Known(Background::default()),
            crons: Tally::Known(0),
            error: Some("rate_limit".into()),
            context: None,
            limit: None,
        });
        put(&world.config.path().join("reports"), "lead-cards", failed);
        assert_eq!(world.shepherd().unwrap(), ["lead-cards failed  rate_limit"]);
        assert_eq!(world.shepherd(), None);
    }

    #[test]
    fn a_member_seen_working_gets_one_more_poll_for_its_stop_to_land() {
        let world = Checked::drained();
        world.herdr.set_status("lead-cards", AgentStatus::Working);
        let (woke, polls) = world.shepherd_until(None, |poll| {
            if poll == 1 {
                world.herdr.set_status("lead-cards", AgentStatus::Idle);
            }
        });
        assert_eq!(woke.unwrap(), ["all drained"]);
        assert_eq!(
            polls, 2,
            "drained on the first idle poll, with no grace poll"
        );
    }

    #[test]
    fn a_member_gone_at_start_that_comes_back_and_dies_wakes_it_gone() {
        let mut world = Checked::drained();
        world.add(member("solo", "w9:p3", None), AgentStatus::Working);
        world.herdr.remove_agent("lead-cards");
        let (woke, _) = world.shepherd_until(None, |poll| match poll {
            1 => {
                world.herdr.resume_agent("lead-cards", "s1", "w9:p0");
                world.herdr.settle();
                world.herdr.set_status("lead-cards", AgentStatus::Idle);
                world.herdr.set_status("solo", AgentStatus::Idle);
            }
            2 => world.herdr.remove_agent("lead-cards"),
            _ => {}
        });
        assert_eq!(woke.unwrap(), ["lead-cards gone"]);
    }

    #[test]
    fn a_member_gone_when_herdr_first_answers_does_not_hold_off_drained() {
        let world = Checked::drained();
        world.herdr.remove_agent("lead-cards");
        world.herdr.set_unreachable(true);
        let (woke, polls) = world.shepherd_until(Some(Duration::from_secs(10)), |poll| {
            if poll == 1 {
                world.herdr.set_unreachable(false);
            }
        });
        assert_eq!(woke.unwrap(), ["all drained"]);
        assert_eq!(polls, 1);
    }

    /// The shepherd's watch from pane w1:p7, which holds no member.
    fn labelling_shepherd(pane_label: bool) -> Request<'static> {
        Request {
            own_pane: Some("w1:p7"),
            pane_label,
            ..ONCE
        }
    }

    #[test]
    fn the_shepherds_watch_labels_its_pane_with_the_question_it_prints_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        send(
            &ledger,
            dir.path(),
            "lead-cards",
            Kind::Question,
            "\nauth=q ok?\nsecond line",
        );

        let req = labelling_shepherd(true);
        run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap();

        assert_eq!(
            herdr.label("w1:p7").as_deref(),
            Some("lead-cards asks: [redacted] ok?")
        );
        assert_eq!(herdr.label("w9:p0"), None);
    }

    #[test]
    fn the_shepherds_watch_labels_its_pane_with_a_member_gone_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("solo", "w9:p3", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("solo"), None, "w9:p3");
        herdr.set_status("solo", AgentStatus::Working);
        let req = Request {
            timeout: None,
            ..labelling_shepherd(true)
        };

        let mut clock = steps(|_| herdr.set_status("solo", AgentStatus::Blocked));
        run(&herdr, &ledger, dir.path(), &req, &mut clock).unwrap();

        assert_eq!(herdr.label("w1:p7").as_deref(), Some("solo is blocked"));
    }

    #[test]
    fn no_watch_labels_a_pane_unless_asked_or_for_a_wake_with_nothing_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        send(
            &ledger,
            dir.path(),
            "lead-cards",
            Kind::Question,
            "which DB?",
        );
        run(
            &herdr,
            &ledger,
            dir.path(),
            &labelling_shepherd(false),
            &mut steps(|_| {}),
        );
        send(&ledger, dir.path(), "lead-cards", Kind::Note, "hi");
        run(
            &herdr,
            &ledger,
            dir.path(),
            &labelling_shepherd(true),
            &mut steps(|_| {}),
        );

        assert_eq!(herdr.label_calls(), 0);
    }

    #[test]
    fn a_leads_watch_leaves_its_pane_unlabelled_for_a_workers_question() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        send(&ledger, dir.path(), "lh-1", Kind::Question, "which DB?");
        let req = Request {
            pane_label: true,
            ..from_pane("w9:p0")
        };

        let woke = run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {}));

        assert_eq!(
            woke.unwrap(),
            ["lh-1 question 2026-11-01 09:00 UTC which DB?"]
        );
        assert_eq!(herdr.label_calls(), 0);
    }

    #[test]
    fn neither_watch_reads_a_leads_labelled_pane_as_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        let notifier = Notifier::new(dir.path());
        let mut clock = steps(|poll| {
            if poll == 1 {
                send(
                    &ledger,
                    dir.path(),
                    "lead-cards",
                    Kind::Question,
                    "which DB?",
                );
                let asked = "which DB?";
                let before = herdr.agents().unwrap();
                crate::pane_label::lead_asked(
                    &herdr,
                    &ledger,
                    dir.path(),
                    "w9:p0",
                    "lead-cards",
                    asked,
                )
                .unwrap();
                let after = herdr.agents().unwrap();
                let lead = after.iter().find(|a| a.pane_id == "w9:p0").unwrap();
                assert_eq!(
                    lead.display_agent.as_deref(),
                    Some("lead-cards asks: which DB?")
                );
                let unlabelled: Vec<_> = after
                    .into_iter()
                    .map(|a| crate::herdr::Agent {
                        display_agent: None,
                        ..a
                    })
                    .collect();
                assert_eq!(unlabelled, before);
                herdr.set_status("lead-cards", AgentStatus::Idle);
            }
        });
        let own = |events: &[Waiting]| notifier.send(events);
        let lead = Request {
            timeout: Some(Duration::from_secs(4)),
            notify_own: Some(&own),
            pane_label: true,
            ..from_pane("w9:p0")
        };

        assert_eq!(run(&herdr, &ledger, dir.path(), &lead, &mut clock), None);
        assert_eq!(
            herdr.label("w9:p0").as_deref(),
            Some("lead-cards asks: which DB?")
        );
        assert_eq!(notifier.sent(), [sent_for("lead-cards", "question")]);
        assert_eq!(
            run(
                &herdr,
                &ledger,
                dir.path(),
                &labelling_shepherd(true),
                &mut steps(|_| {})
            )
            .unwrap(),
            ["lead-cards question 2026-11-01 09:00 UTC which DB?"]
        );
    }

    #[test]
    fn a_watch_from_a_members_pane_labels_nothing_even_when_it_reads_as_the_shepherds() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        send(
            &ledger,
            dir.path(),
            "lead-cards",
            Kind::Question,
            "which DB?",
        );
        let req = Request {
            own_pane: Some("w9:p1"),
            pane_label: true,
            ..ONCE
        };

        run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap();

        assert_eq!(herdr.label_calls(), 0);
    }

    #[test]
    fn the_shepherds_watch_labels_a_pane_whose_id_a_member_holds_on_another_herdr() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = quiet_led_world();
        send(
            &ledger,
            dir.path(),
            "lead-cards",
            Kind::Question,
            "which DB?",
        );
        let req = Request {
            own_pane: Some("w9:p1"),
            own_socket: Some(Path::new("/run/other.sock")),
            pane_label: true,
            ..ONCE
        };

        run(&herdr, &ledger, dir.path(), &req, &mut steps(|_| {})).unwrap();

        assert_eq!(
            herdr.label("w9:p1").as_deref(),
            Some("lead-cards asks: which DB?")
        );
    }
}
