//! `loopherd watch`: block until a member needs the shepherd. Stops come
//! from the Stop hook's mailbox entries; herdr adds `blocked` and `gone`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::herd_view::{Member, Options, View};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::lock::Lock;
use crate::report::{Body, Filter, Mailbox, Stop, reports, unread_stops};
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
}

/// Polls until a member needs the shepherd and returns what woke it, one
/// rendered line each; `None` when `req.timeout` passes first. Calls `warn`
/// as herdr becomes unreadable and for each bad mailbox line. Refuses to run
/// beside another watch on `dir`.
pub fn watch(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    dir: &Path,
    req: &Request,
    clock: &mut dyn Clock,
    warn: &mut dyn FnMut(&str),
) -> Result<Option<Vec<String>>> {
    let _only = only_watch(dir)?;
    // Each member's last known status; an unreadable poll keeps it.
    let mut last: HashMap<String, AgentStatus> = HashMap::new();
    let mut alive: HashSet<String> = HashSet::new();
    let mut code = 0;
    loop {
        let read = status::read_statuses(herdr, ledger, Options::default())?;
        if read.code != code {
            if let Some(problem) = &read.problem {
                warn(problem);
            }
            code = read.code;
        }
        let members = in_scope(read.view, req);
        let names: Vec<String> = members.iter().map(|m| m.name.clone()).collect();
        let mut lines = vec![];
        // An empty filter means every mailbox.
        if !names.is_empty() {
            let read = reports(
                ledger,
                dir,
                &Filter {
                    names: &names,
                    herd: None,
                    all: false,
                },
            )?;
            let stops = unread_stops(dir, &names)?;
            read.warnings
                .iter()
                .chain(&stops.warnings)
                .for_each(|w| warn(&format!("warning: {w}")));
            lines.extend(read.lines);
            lines.extend(stops.mailboxes.iter().flat_map(stop_wakes));
        }
        for member in &members {
            if member.gone {
                if alive.contains(&member.name) {
                    lines.push(format!("{} gone", member.name));
                }
                continue;
            }
            alive.insert(member.name.clone());
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
            return Ok(Some(lines));
        }
        let left = req.timeout.map(|t| t.saturating_sub(clock.elapsed()));
        if left == Some(Duration::ZERO) {
            return Ok(None);
        }
        clock.sleep(left.map_or(req.poll, |left| left.min(req.poll)));
    }
}

/// Holds `watch.lock` in `dir`, or fails naming it while another watch does.
fn only_watch(dir: &Path) -> Result<Lock> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("watch.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    Lock::try_take(file)
        .with_context(|| format!("lock {}", path.display()))?
        .ok_or_else(|| anyhow!("another watch holds {}", path.display()))
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

/// The view's non-retired members, narrowed to `req.herds`.
fn in_scope(view: View, req: &Request) -> Vec<Member> {
    let mut members = vec![];
    for herd in view.herds {
        if req.herds.is_empty() || req.herds.contains(&herd.name) {
            members.extend(herd.members);
        }
    }
    if req.herds.is_empty() {
        members.extend(view.ungrouped);
    }
    members
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger};
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
        }
    }

    fn ledger(members: Vec<AgentRecord>) -> MemLedger {
        MemLedger(
            members.into_iter().map(|m| (m.name.clone(), m)).collect(),
            vec![],
        )
    }

    fn send(ledger: &MemLedger, dir: &Path, name: &str, kind: Kind, text: &str) {
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
        woke
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

        assert_eq!(woke.unwrap(), ["lh-1 question 2026-11-01 09:00 which DB?"]);
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
        let _first = only_watch(dir.path()).unwrap();

        let err = watch(
            &herdr,
            &ledger,
            dir.path(),
            &ONCE,
            &mut steps(|_| {}),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains(&dir.path().join("watch.lock").display().to_string()),
            "{err}"
        );
    }
}
