//! `loopherd watch`: block until a member needs the shepherd. Stops come
//! from the Stop hook's mailbox entries; herdr adds `blocked` and `gone`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::context::ShepherdHandoff;
use crate::feed::{claimable, run_check};
use crate::herd::{MemberCheck, MemberChecks, Paths, lead_checks};
use crate::herd_view::{Member, Options, View};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::{AfterMerge, AgentRecord, Ledger, Process};
use crate::lock::Lock;
use crate::notify::{self, Waiting};
use crate::pause;
use crate::process::{Procs, live};
use crate::report::{Body, Filter, Mailbox, Pending, Stop, member_in, peek_reports, unread_stops};
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
    #[cfg(test)]
    pub const ALL: [&str; 8] = [
        PAUSED,
        FINISHED,
        FAILED,
        MERGED,
        CLAIMABLE,
        INTERRUPTED,
        BLOCKED,
        GONE,
    ];
}

pub trait Clock {
    /// Time since the watch started.
    fn elapsed(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
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
}

pub struct Request<'a> {
    /// Only members of these herds; empty is every member.
    pub herds: &'a [String],
    pub timeout: Option<Duration>,
    pub poll: Duration,
    /// The caller's pane and herdr socket, which make it a lead's watch
    /// when they match a lead as `loopherd report` matches.
    pub own_pane: Option<&'a str>,
    pub own_socket: Option<&'a Path>,
    /// Where a lead's watch finds its herd file for its merged and
    /// claimable checks; `None` runs no checks.
    pub herd_files: Option<&'a Paths>,
    /// How long each check's command may run; read only by a lead's watch,
    /// so a bad setting can't stop the shepherd's.
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
}

pub type NotifyOwn<'a> = &'a dyn Fn(&[Waiting]);

/// How often a lead's watch runs its merged and claimable checks.
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
    pub fn mark_read(self) -> Result<()> {
        self.pending.into_iter().try_for_each(Pending::save)?;
        self.fired.map_or(Ok(()), Fired::save)?;
        self.handoff.map_or(Ok(()), ShepherdHandoff::mark)
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
            paths,
            procs: req.procs,
            timeout: (req.check_timeout)()?,
            fired: Fired::load(dir, name, created)?,
            next: Duration::ZERO,
            problem: None,
            unreadable_pause: None,
        }),
        _ => None,
    };
    // Each member's last known status; an unreadable poll keeps it.
    let mut last: HashMap<String, AgentStatus> = HashMap::new();
    let mut gone = Gone::default();
    let mut code = 0;
    let mut own_questions_seen = HashSet::new();
    loop {
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
        let mut lines = vec![];
        let mut waiting = vec![];
        let mut pending = vec![];
        // An empty filter means every mailbox.
        if !names.is_empty() {
            // Stops first: a stop is appended after its report, so any stop
            // read here has its report visible to the read after it.
            let (stops, stops_read) = unread_stops(dir, &names)?;
            let (read, reports_read) = peek_reports(
                ledger,
                dir,
                &Filter {
                    names: &names,
                    herd: None,
                    all: false,
                },
            )?;
            read.warnings
                .iter()
                .for_each(|w| warn(&format!("warning: {w}")));
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
            lines.extend(stops.iter().flat_map(stop_wakes));
            pending.extend([stops_read, reports_read]);
        }
        let mut fired = None;
        if let Some(checks) = checks.as_mut().filter(|c| clock.elapsed() >= c.next) {
            let events = checks.run(ledger, warn)?;
            checks.next = clock.elapsed() + CHECK_EVERY;
            if !events.is_empty() {
                lines.extend(events.iter().cloned());
                fired = Some(checks.fired.with(events));
            }
        }
        if !agents_read {
            gone = Gone::default();
        }
        for member in &members {
            if member.gone {
                if gone.confirm(member, &occupied) {
                    lines.push(format!("{} {}", member.name, line::GONE));
                }
                continue;
            }
            gone.seen(member, agents_read);
            let was = last.get(&member.name).copied();
            // Only on change: one blocked at start would re-wake every re-armed watch.
            if member.status == AgentStatus::Blocked
                && was.is_some_and(|w| w != AgentStatus::Blocked)
            {
                lines.push(format!("{} {}", member.name, line::BLOCKED));
                waiting.push(Waiting {
                    herd: None,
                    member: member.name.clone(),
                    kind: notify::Kind::Blocked,
                    text: String::new(),
                    at: None,
                });
            }
            if member.status != AgentStatus::Unknown {
                last.insert(member.name.clone(), member.status);
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
                notify::unblocked(dir, &member.name)?;
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

/// A lead's merged and claimable checks, run every [`CHECK_EVERY`].
struct Checks<'a> {
    herd: String,
    paths: &'a Paths,
    procs: &'a dyn Procs,
    timeout: Duration,
    fired: Fired,
    /// When the next run is due, as time since the watch started.
    next: Duration,
    /// Why the herd file last failed to load, warned once.
    problem: Option<String>,
    /// Why the herd's pause file was last unreadable, warned once.
    unreadable_pause: Option<String>,
}

impl Checks<'_> {
    /// One line per event not fired before. A check or source that fails,
    /// or runs out of the round's [`CHECK_EVERY`], is no event. A paused
    /// herd has no claimable events (D23).
    fn run(&mut self, ledger: &dyn Ledger, warn: &mut dyn FnMut(&str)) -> Result<Vec<String>> {
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
            !e.retired && !e.is_lead() && e.herd.as_deref() == Some(self.herd.as_str())
        });
        let after_merge = queue.as_ref().is_some_and(|q| !q.after_merge.is_empty());
        let mut events = vec![];
        let mut freed = 0;
        for worker in workers {
            let done = run_member_check(&checks.done, &checks, &worker, &budget);
            let merged = run_member_check(&checks.merged, &checks, &worker, &budget);
            let due = after_merge && worker.after_merge.is_none();
            // Feed keeps, done or not, a member whose after_merge is due or unnoted.
            let kept = match worker.after_merge {
                Some(AfterMerge::Pending { owner }) => {
                    if live(self.procs, Some(owner)).is_none()
                        && still_pending(ledger, &worker.name, owner)?
                    {
                        events.push(format!("{} {}", worker.name, line::INTERRUPTED));
                    }
                    true
                }
                Some(AfterMerge::Noted) => false,
                None => due && matches!(merged, Some(Ok(true) | Err(_))),
            };
            freed += usize::from(done == Some(Ok(true)) && !kept);
            if merged == Some(Ok(true)) && (due || matches!(done, None | Some(Ok(false)))) {
                events.push(format!("{} {}", worker.name, line::MERGED));
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
            let names = claimable(ledger, self.paths, &queue, &self.herd, freed, timeout);
            // A running feed may be spawning these before it records them.
            if let (Ok(names), false) = (names, feeding(self.paths)?) {
                events.extend(
                    names
                        .into_iter()
                        .map(|name| format!("{name} {}", line::CLAIMABLE)),
                );
            }
        }
        Ok(events
            .into_iter()
            .filter(|e| !self.fired.keys.contains(e))
            .collect())
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
}

/// `worker`'s check from `checks` for its template: `None` without one,
/// else whether it passed, or why it failed or wasn't run.
fn run_member_check(
    checks: &BTreeMap<String, MemberCheck>,
    herd: &MemberChecks,
    worker: &AgentRecord,
    budget: &Budget,
) -> Option<Result<bool, String>> {
    let check = checks.get(worker.template.as_ref()?)?;
    let Some(params) = &worker.params else {
        return Some(Err("no recorded params".into()));
    };
    let Some(timeout) = budget.left() else {
        return Some(Err("out of time".into()));
    };
    Some(
        check
            .render(worker, params)
            .map_err(|e| e.to_string())
            .and_then(|command| run_check(&command, &herd.repo, timeout)),
    )
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

/// The events a lead's watch has printed, one per line of
/// `watch-<lead>.fired`, so a re-armed watch doesn't wake on them again.
/// Its first line names the lead's spawn, so a re-created lead starts afresh.
/// A [`RESUMED`] line forgets the claimable keys above it, so a card announced
/// before a pause is announced again after resume.
struct Fired {
    path: PathBuf,
    header: String,
    keys: HashSet<String>,
    new: Vec<String>,
}

impl Fired {
    /// Called holding the lead's watch lock, so no other watch writes meanwhile.
    fn load(dir: &Path, lead: &str, created: &str) -> Result<Fired> {
        let mut fired = Fired {
            path: fired_path(dir, lead),
            header: format!("lead {created}"),
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
        let mut lines = text.lines();
        self.keys.clear();
        if lines.next() != Some(self.header.as_str()) {
            return fs::write(path, format!("{}\n", self.header))
                .with_context(|| format!("write {}", path.display()));
        }
        for line in lines {
            if line == RESUMED {
                self.keys.retain(|k| !is_claimable(k));
            } else {
                self.keys.insert(line.to_string());
            }
        }
        Ok(())
    }

    /// The keys to append once `events` print.
    fn with(&self, events: Vec<String>) -> Fired {
        Fired {
            path: self.path.clone(),
            header: self.header.clone(),
            keys: HashSet::new(),
            new: events,
        }
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

fn fired_path(dir: &Path, lead: &str) -> PathBuf {
    dir.join(format!("watch-{lead}.fired"))
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
    alive: HashSet<String>,
    missing: HashSet<String>,
}

impl Gone {
    fn seen(&mut self, member: &Member, agents_read: bool) {
        self.missing.remove(&member.name);
        if agents_read {
            self.alive.insert(member.name.clone());
        }
    }

    /// True on the second poll in a row that `member` is really gone.
    fn confirm(&mut self, member: &Member, occupied: &HashSet<String>) -> bool {
        if !self.alive.contains(&member.name) || occupied.contains(&member.pane) {
            self.missing.remove(&member.name);
            return false;
        }
        !self.missing.insert(member.name.clone())
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
    let Some(pane) = req.own_pane else {
        return Ok(Reader::Shepherd);
    };
    let entries = ledger.entries()?;
    let lead = match member_in(&entries, pane, req.own_socket) {
        Some(entry) if entry.is_lead() => entry,
        Some(_) => return Ok(Reader::Shepherd),
        None => {
            // Pane ids repeat across herdrs, but this may be a lead whose recorded socket is stale.
            let on_pane = |e: &&AgentRecord| !e.retired && e.is_lead() && e.pane_id == pane;
            if let Some(lead) = entries.values().find(on_pane) {
                warn(&format!(
                    "warning: pane {pane} holds lead {} on socket {}, \
                     but this watch's socket is {}; running the shepherd's watch",
                    lead.name,
                    shown(lead.socket.as_deref()),
                    shown(req.own_socket),
                ));
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
    if req
        .own_pane
        .is_some_and(|pane| member_in(&entries, pane, req.own_socket).is_some())
    {
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
            Body::Start => {
                (paused, reported) = (false, false);
                None
            }
            Body::Done { .. } | Body::Question { .. } | Body::Handoff { .. } => {
                reported = true;
                None
            }
            Body::Note { .. } => None,
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
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{FileLedger, MemLedger};
    use crate::report::{Background, Entry, Kind, Reporter, Tally, Unknown, append, report};

    fn member(name: &str, pane: &str, herd: Option<&str>) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: pane.into(),
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
            herd: herd.map(Into::into),
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
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
        };
        let at = humantime::parse_rfc3339("2026-11-01T09:00:00Z").unwrap();
        report(ledger, dir, &who, kind, text, at).unwrap();
    }

    /// Runs `step` with the poll count before each sleep, so a test changes
    /// the world between polls.
    struct Steps<F: FnMut(u32)> {
        elapsed: Duration,
        polls: u32,
        step: F,
    }

    impl<F: FnMut(u32)> Clock for Steps<F> {
        fn elapsed(&self) -> Duration {
            self.elapsed
        }
        fn sleep(&mut self, duration: Duration) {
            self.polls += 1;
            assert!(self.polls < 100, "watch never woke");
            (self.step)(self.polls);
            self.elapsed += duration;
        }
    }

    fn steps<F: FnMut(u32)>(step: F) -> Steps<F> {
        Steps {
            elapsed: Duration::ZERO,
            polls: 0,
            step,
        }
    }

    const EVERY: Request = Request {
        procs: &crate::process::ProcFs,
        herds: &[],
        timeout: None,
        poll: Duration::from_secs(2),
        own_pane: None,
        own_socket: None,
        herd_files: None,
        check_timeout: || bail!("only a lead's watch reads the check timeout"),
        shepherd_handoff: None,
        notify_own: None,
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
        wake.mark_read().unwrap();
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
                "lh-1 question 2026-11-01 09:00 which DB?",
                "lh-1 note 2026-11-01 09:00 starting",
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
        first.unwrap().unwrap().mark_read().unwrap();

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
            ["lh-1 note 2026-11-01 09:00 PR #9"]
        );
        let every = Request {
            timeout: Some(Duration::ZERO),
            ..EVERY
        };
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &every, &mut steps(|_| {})).unwrap(),
            ["solo note 2026-11-01 09:00 hi"]
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
            ["lh-1 question 2026-11-01 09:00 which DB?"]
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("herdr unreachable"), "{warnings:?}");
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
        })
    }

    fn failed(error: &str) -> Body {
        Body::Stop(Stop {
            session: None,
            background: Tally::Unknown(Unknown::Unknown),
            crons: Tally::Unknown(Unknown::Unknown),
            error: Some(error.into()),
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
        procs: &crate::process::ProcFs,
        herds: &[],
        timeout: Some(Duration::ZERO),
        poll: Duration::from_secs(2),
        own_pane: None,
        own_socket: None,
        herd_files: None,
        check_timeout: || bail!("only a lead's watch reads the check timeout"),
        shepherd_handoff: None,
        notify_own: None,
    };

    /// One poll of a fresh watch.
    fn once(herdr: &FakeHerdr, ledger: &MemLedger, dir: &Path) -> Option<Vec<String>> {
        run(herdr, ledger, dir, &ONCE, &mut steps(|_| {}))
    }

    #[test]
    fn a_stop_with_work_in_flight_wakes_it_paused_once() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", Body::Start);

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
        put(dir.path(), "lh-1", Body::Start);
        put(dir.path(), "lh-1", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 finished  no report"]
        );
    }

    #[test]
    fn a_handoff_counts_as_the_report_before_a_finish() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        put(dir.path(), "lh-1", Body::Start);
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
            ["lh-1 handoff 2026-11-01 09:00 at step 3", "lh-1 finished"]
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
        put(dir.path(), "lh-1", Body::Start);
        send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9");
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            ["lh-1 done 2026-11-01 09:00 PR #9"]
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
        put(dir.path(), "lh-2", Body::Start);
        put(dir.path(), "lh-2", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            [
                "lh-1 question 2026-11-01 09:00 which DB?",
                "lh-2 done 2026-11-01 09:00 PR #8",
                "lh-1 finished",
                "lh-2 finished  no report",
            ]
        );

        send(&ledger, dir.path(), "lh-1", Kind::Note, "still here");
        put(dir.path(), "lh-1", stop(0, 0));
        assert_eq!(
            once(&herdr, &ledger, dir.path()).unwrap(),
            [
                "lh-1 note 2026-11-01 09:00 still here",
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
        let path = dir.path().join("lh-1.jsonl");
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

        let want = ["lh-1 done 2026-11-01 09:00 PR #9", "lh-1 finished"];
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
            let config = dir.join("config.toml");
            fs::write(
                &config,
                "[notify]\nenabled = true\nherdr = false\ncommand = ['n', '{herd}', '{member}', '{kind}']\n",
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
            ["lead-cards question 2026-11-01 09:00 Merge #1?"]
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

        assert_eq!(printed, ["lead-cards question 2026-11-01 09:00 Merge #1?"]);
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
            ["lh-1 note 2026-11-01 09:00 hi"]
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
            ["lh-1 note 2026-11-01 09:00 hi"]
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
                "lead-cards note 2026-11-01 09:00 hi",
                "solo note 2026-11-01 09:00 hi",
                "zpr-2 note 2026-11-01 09:00 hi",
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
            ["lead-cards note 2026-11-01 09:00 hi"]
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
            ["zpr-2 note 2026-11-01 09:00 hi"]
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
            ["zpr-2 note 2026-11-01 09:00 hi"]
        );

        // Gone, it still owns its herd; retired, it lets go.
        herdr.remove_agent("lead-prs");
        assert_eq!(shepherd(&ledger), None);
        ledger.0.get_mut("lead-prs").unwrap().retired = true;
        assert_eq!(
            shepherd(&ledger).unwrap(),
            [
                "zpr-2 note 2026-11-01 09:00 hi",
                "zpr-2 note 2026-11-01 09:00 hi",
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
            ["lh-1 note 2026-11-01 09:00 hi"]
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
            woke.contains(&"lh-1 note 2026-11-01 09:00 after".to_string()),
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
            ["lh-1 note 2026-11-01 09:00 hi"]
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
                woke.contains(&"lead-cards note 2026-11-01 09:00 hi".to_string()),
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
            ["lh-1 note 2026-11-01 09:00 hi"]
        );

        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = led_world(dir.path());
        let socketed = Request {
            own_socket: Some(Path::new("/run/live.sock")),
            ..from_pane("w9:p0")
        };
        let woke = run(&herdr, &ledger, dir.path(), &socketed, &mut steps(|_| {})).unwrap();
        assert!(
            woke.contains(&"lead-cards note 2026-11-01 09:00 hi".to_string()),
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
            woke.contains(&"lead-cards note 2026-11-01 09:00 hi".to_string()),
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
    fn a_merged_worker_already_done_or_a_failing_check_is_no_event() {
        let done = Checked::new();
        done.touch("merged-1", "");
        done.touch("done-1", "");
        assert_eq!(done.once("w9:p0"), None, "woke on a done worker");

        for checks in [
            "done = \"exit 1\"\nmerged = \"exit 3\"\n",
            "done = \"exit 2\"\nmerged = \"exit 0\"\n",
        ] {
            let world = Checked::with_template(checks);
            assert_eq!(world.once("w9:p0"), None, "{checks}");
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
    fn a_done_worker_frees_its_slot_unless_its_after_merge_is_due_or_its_merged_check_errors() {
        let none = None;
        let noted = Some(AfterMerge::Noted);
        // (case, herd's after_merge, merged check, ledger's after_merge, frees its slot)
        let cases = [
            ("no after_merge, merged", "", "exit 0", none, true),
            ("after_merge, unmerged", "[\"true\"]", "exit 1", none, true),
            (
                "after_merge, merged check errors",
                "[\"true\"]",
                "exit 3",
                none,
                false,
            ),
            (
                "after_merge noted, merged",
                "[\"true\"]",
                "exit 0",
                noted,
                true,
            ),
        ];
        for (case, after_merge, merged, recorded, frees) in cases {
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
            let woke = frees.then(|| vec!["lh-2 claimable".to_string()]);
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
        fn started(&mut self, name: &str, at: std::time::SystemTime) -> Result<()> {
            self.0.borrow_mut().started(name, at)
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
    fn a_failing_source_is_no_claimable_event() {
        let world = Checked::new();
        world.touch("tasks", "1\n2\n");
        fs::write(
            world.paths.herds.join("cards.toml"),
            fs::read_to_string(world.paths.herds.join("cards.toml"))
                .unwrap()
                .replace("cat tasks", "cat tasks; exit 1"),
        )
        .unwrap();
        assert_eq!(world.once("w9:p0"), None);
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
                "lh-1 note 2026-11-01 09:00 hi",
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
        first.with(vec!["lh-1 merged".into()]).save().unwrap();
        second.with(vec!["lh-2 claimable".into()]).save().unwrap();
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

        assert!(cut.unwrap().unwrap_err().contains("timed out"));
        assert_eq!(after, Some(Err("out of time".into())));
        assert!(started.elapsed() < Duration::from_secs(2));
        // Past the check's own `sleep 2`, so a check left running would have touched it.
        std::thread::sleep(Duration::from_millis(2200).saturating_sub(started.elapsed()));
        assert!(!world.repo.path().join("ran-1").exists());
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
            crate::pause::pause(&self.paths, herds, None, std::time::SystemTime::now()).unwrap();
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
}
