//! `loopherd watch`: block until a member needs the shepherd. Stops come
//! from the Stop hook's mailbox entries; herdr adds `blocked` and `gone`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::herd_view::{Member, Options, View};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::{AgentRecord, Ledger};
use crate::lock::Lock;
use crate::report::{Body, Filter, Mailbox, Pending, Stop, member_in, peek_reports, unread_stops};
use crate::status;

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
}

/// What woke the watch, one rendered line each. Nothing is marked read until
/// [`Wake::mark_read`], so a wake lost before it's printed comes back.
#[must_use = "what woke it stays unread until mark_read"]
pub struct Wake {
    pub lines: Vec<String>,
    pending: Vec<Pending>,
}

impl Wake {
    pub fn mark_read(self) -> Result<()> {
        self.pending.into_iter().try_for_each(Pending::save)
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
    let reader = reader(ledger, req)?;
    let _only = only_watch(dir, &reader)?;
    // Each member's last known status; an unreadable poll keeps it.
    let mut last: HashMap<String, AgentStatus> = HashMap::new();
    let mut gone = Gone::default();
    let mut code = 0;
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
        let members = in_scope(read.view, req, &reader)?;
        let names: Vec<String> = members.iter().map(|m| m.name.clone()).collect();
        let mut lines = vec![];
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
            lines.extend(stops.iter().flat_map(stop_wakes));
            pending.extend([stops_read, reports_read]);
        }
        if !agents_read {
            gone = Gone::default();
        }
        for member in &members {
            if member.gone {
                if gone.confirm(member, &occupied) {
                    lines.push(format!("{} gone", member.name));
                }
                continue;
            }
            gone.seen(member, agents_read);
            let was = last.get(&member.name).copied();
            // Only on change: one blocked at start would re-wake every re-armed watch.
            if member.status == AgentStatus::Blocked
                && was.is_some_and(|w| w != AgentStatus::Blocked)
            {
                lines.push(format!("{} blocked", member.name));
            }
            if member.status != AgentStatus::Unknown {
                last.insert(member.name.clone(), member.status);
            }
        }
        if !lines.is_empty() {
            return Ok(Some(Wake { lines, pending }));
        }
        // Nothing to print, so nothing to lose.
        pending.into_iter().try_for_each(Pending::save)?;
        let left = req.timeout.map(|t| t.saturating_sub(clock.elapsed()));
        if left == Some(Duration::ZERO) {
            return Ok(None);
        }
        clock.sleep(left.map_or(req.poll, |left| left.min(req.poll)));
    }
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
    Lead { name: String, herd: String },
}

/// A lead's when the caller's pane holds one, else the shepherd's. A lead
/// may only name its own herd.
fn reader(ledger: &dyn Ledger, req: &Request) -> Result<Reader> {
    let Some(pane) = req.own_pane else {
        return Ok(Reader::Shepherd);
    };
    let entries = ledger.entries()?;
    let lead = match member_in(&entries, pane, req.own_socket) {
        Some(entry) if entry.is_lead() => entry,
        Some(_) => return Ok(Reader::Shepherd),
        None => {
            let on_pane = |e: &&AgentRecord| !e.retired && e.is_lead() && e.pane_id == pane;
            if let Some(lead) = entries.values().find(on_pane) {
                bail!(
                    "pane {pane} holds lead {} on socket {}, but this watch's socket is {}",
                    lead.name,
                    shown(lead.socket.as_deref()),
                    shown(req.own_socket),
                );
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
    Ok(Reader::Lead { name, herd })
}

fn shown(socket: Option<&Path>) -> String {
    socket.map_or("unset".into(), |s| s.display().to_string())
}

/// Holds the reader's lock in `dir`, with this process's PID in it, or
/// fails naming the lock and its holder while another watch has it.
fn only_watch(dir: &Path, reader: &Reader) -> Result<Lock> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(match reader {
        Reader::Shepherd => "watch.lock".to_string(),
        Reader::Lead { name, .. } => format!("watch-{name}.lock"),
    });
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
            Body::Done { .. } | Body::Question { .. } => {
                reported = true;
                None
            }
            Body::Note { .. } => None,
            Body::Stop(Stop {
                error: Some(error), ..
            }) => {
                paused = false;
                Some(format!("failed  {error}"))
            }
            Body::Stop(stop) => match stop.pending() {
                Some(pending) => {
                    (!std::mem::replace(&mut paused, true)).then(|| format!("paused  {pending}"))
                }
                None => {
                    let note = if reported { "" } else { "  no report" };
                    (paused, reported) = (false, false);
                    Some(format!("finished{note}"))
                }
            },
        };
        if let (true, Some(wake)) = (i >= mailbox.unread, wake) {
            wakes.push(format!("{} {wake}", mailbox.name));
        }
    }
    wakes
}

/// The view's non-retired members `reader` owns, narrowed to `req.herds`.
/// A herd with a lead, even a gone one, is the lead's but for the lead itself.
/// Fails once a lead reading is no longer its herd's lead.
fn in_scope(view: View, req: &Request, reader: &Reader) -> Result<Vec<Member>> {
    if let Reader::Lead { name, herd } = reader {
        let own = view.herds.into_iter().find(|h| h.name == *herd);
        let workers = own.map(|h| h.members).unwrap_or_default();
        if !workers.iter().any(|m| m.lead && m.name == *name) {
            bail!("{name} was retired, so its watch ends");
        }
        return Ok(workers.into_iter().filter(|m| !m.lead).collect());
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
    Ok(members)
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
        herds: &[],
        timeout: None,
        poll: Duration::from_secs(2),
        own_pane: None,
        own_socket: None,
    };

    fn run(
        herdr: &FakeHerdr,
        ledger: &MemLedger,
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
        herds: &[],
        timeout: Some(Duration::ZERO),
        poll: Duration::from_secs(2),
        own_pane: None,
        own_socket: None,
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
    fn a_leads_pane_on_another_socket_or_none_is_an_error_naming_the_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = socketed_lead(dir.path());
        for socket in [Some(Path::new("/run/dev.sock")), None] {
            let req = Request {
                own_socket: socket,
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
                panic!("a mismatched socket watched as {socket:?}");
            };
            assert!(err.to_string().contains("lead-cards"), "{err}");
            assert!(err.to_string().contains("/run/live.sock"), "{err}");
        }
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
}
