//! `loopherd watch`: block until a member needs the shepherd.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::herd_view::{Member, Options, View};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::report::{Filter, Kind, Read, reports};
use crate::status;
use crate::stop::{self, Mark, State, Stop};

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

/// Where members' reports and stop records live.
pub struct Dirs {
    pub reports: PathBuf,
    pub stops: PathBuf,
}

pub struct Request<'a> {
    /// Only members of these herds; empty is every member.
    pub herds: &'a [String],
    pub timeout: Option<Duration>,
    pub poll: Duration,
}

/// Polls until a member needs the shepherd and returns what woke it, one
/// rendered line each; `None` when `req.timeout` passes first. Calls `warn`
/// as herdr becomes unreadable and for each bad report line.
pub fn watch(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    dirs: &Dirs,
    req: &Request,
    clock: &mut dyn Clock,
    warn: &mut dyn FnMut(&str),
) -> Result<Option<Vec<String>>> {
    // Each member's last known status; an unreadable poll keeps it.
    let mut last: HashMap<String, AgentStatus> = HashMap::new();
    // Members seen going working -> done/idle whose stop record hadn't landed.
    let mut awaiting: HashMap<String, AgentStatus> = HashMap::new();
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
        let read = match names.is_empty() {
            // An empty filter means every mailbox.
            true => Read::default(),
            false => reports(
                ledger,
                &dirs.reports,
                &Filter {
                    names: &names,
                    herd: None,
                    all: false,
                },
            )?,
        };
        read.warnings
            .iter()
            .for_each(|w| warn(&format!("warning: {w}")));
        // Any report read ends this run, so only this poll's can precede a stop.
        let reported: HashSet<String> = read
            .senders
            .into_iter()
            .filter(|(_, kind)| *kind != Kind::Note)
            .map(|(name, _)| name)
            .collect();
        let mut lines = read.lines;
        let waited = std::mem::take(&mut awaiting);
        let left = req.timeout.map(|t| t.saturating_sub(clock.elapsed()));
        let last_poll = left == Some(Duration::ZERO);
        stop::with_marks(&dirs.stops, |marks| {
            for member in &members {
                let name = &member.name;
                let was = last.get(name).copied();
                if member.gone {
                    if was == Some(AgentStatus::Working) {
                        lines.push(stopped(&reported, name, "gone", ""));
                    }
                    continue;
                }
                let stop = latest_stop(&dirs.stops, name, warn);
                // First sight: stops before it are history, not news.
                let mark = marks
                    .entry(name.clone())
                    .or_insert_with(|| Mark::of(stop.as_ref()));
                match member.status {
                    // Only on change: one blocked at start would re-wake every re-armed watch.
                    AgentStatus::Blocked if was.is_some_and(|w| w != AgentStatus::Blocked) => {
                        lines.push(format!("{name} blocked"))
                    }
                    now @ (AgentStatus::Done | AgentStatus::Idle) => {
                        let seen = was == Some(AgentStatus::Working);
                        match judge(stop.as_ref(), mark, seen, waited.contains_key(name)) {
                            // Not a finish, so no report is expected yet.
                            Judged::Paused(work) => {
                                lines.push(format!("{name} {}  (paused: {work})", now.as_str()))
                            }
                            Judged::Stopped(also) => {
                                lines.push(stopped(&reported, name, now.as_str(), also))
                            }
                            Judged::Await => {
                                awaiting.insert(name.clone(), now);
                            }
                            Judged::Quiet => {}
                        }
                    }
                    _ => {}
                }
                if member.status != AgentStatus::Unknown {
                    last.insert(name.clone(), member.status);
                }
            }
            // This run ends here, so stops still awaiting their record go out now.
            if !lines.is_empty() || last_poll {
                let mut flushed: Vec<_> = awaiting.drain().collect();
                flushed.sort_by(|a, b| a.0.cmp(&b.0));
                for (name, now) in flushed {
                    lines.push(stopped(&reported, &name, now.as_str(), ""));
                    marks.entry(name).or_default().unrecorded = true;
                }
            }
        })?;
        if !lines.is_empty() {
            return Ok(Some(lines));
        }
        if last_poll {
            return Ok(None);
        }
        clock.sleep(left.map_or(req.poll, |left| left.min(req.poll)));
    }
}

/// A finished stop's wake line; `also` adds to its note.
fn stopped(reported: &HashSet<String>, name: &str, now: &str, also: &str) -> String {
    let note = match reported.contains(name) {
        true => "",
        false => ", no report",
    };
    format!("{name} {now}  (was working{also}{note})")
}

enum Judged {
    /// Background work in flight, e.g. `1 shell`.
    Paused(String),
    /// Finished, with `also` to add to its note.
    Stopped(&'static str),
    /// Seen stopping, but the hook hasn't written its record yet.
    Await,
    Quiet,
}

/// What a done or idle member's latest `stop` means, given the last one
/// judged (`mark`, advanced here), whether this poll saw it stop working, and
/// whether the last poll was already awaiting its record.
fn judge(stop: Option<&Stop>, mark: &mut Mark, stopped: bool, awaited: bool) -> Judged {
    let Some(stop) = stop.filter(|s| mark.at.as_ref() != Some(&s.at)) else {
        return match (stopped, awaited) {
            // No hook (spawned before it), or it failed: wake as before.
            (_, true) => {
                mark.unrecorded = true;
                Judged::Stopped("")
            }
            (true, false) => Judged::Await,
            (false, false) => Judged::Quiet,
        };
    };
    let (was_paused, woken) = (mark.paused, mark.unrecorded);
    *mark = Mark::of(Some(stop));
    match stop.state() {
        State::Paused(work) if !was_paused => Judged::Paused(work),
        // Its stop already woke the shepherd; only news of a pause is worth more.
        _ if woken => Judged::Quiet,
        // Still paused, e.g. a /loop turn: it wakes the shepherd when it finishes.
        State::Paused(_) => Judged::Quiet,
        State::Finished => Judged::Stopped(""),
        State::Unknown => Judged::Stopped(", background unknown"),
    }
}

/// `None` too when the record can't be read, after a warning.
fn latest_stop(dir: &Path, name: &str, warn: &mut dyn FnMut(&str)) -> Option<Stop> {
    stop::latest(dir, name)
        .inspect_err(|e| warn(&format!("warning: {e:#}")))
        .ok()
        .flatten()
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
    use crate::report::{Reporter, report};
    use crate::stop;

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
        }
    }

    fn ledger(members: Vec<AgentRecord>) -> MemLedger {
        MemLedger(members.into_iter().map(|m| (m.name.clone(), m)).collect())
    }

    fn send(ledger: &MemLedger, dir: &Path, name: &str, kind: Kind, text: &str) {
        let who = Reporter {
            own_pane: None,
            as_name: Some(name),
        };
        let at = humantime::parse_rfc3339("2026-11-01T09:00:00Z").unwrap();
        report(ledger, &dirs(dir).reports, &who, kind, text, at).unwrap();
    }

    fn dirs(dir: &Path) -> Dirs {
        Dirs {
            reports: dir.join("reports"),
            stops: dir.join("stops"),
        }
    }

    /// Records a stop for `name` with these in-flight tasks and crons.
    fn stop(ledger: &MemLedger, dir: &Path, name: &str, at: &str, tasks: &str, crons: &str) {
        let input = format!(r#"{{"background_tasks":[{tasks}],"session_crons":[{crons}]}}"#);
        stop_with(ledger, dir, name, at, &input);
    }

    /// Records a stop for `name` from this Stop hook input.
    fn stop_with(ledger: &MemLedger, dir: &Path, name: &str, at: &str, input: &str) {
        let who = Reporter {
            own_pane: None,
            as_name: Some(name),
        };
        let at = humantime::parse_rfc3339(at).unwrap();
        stop::record(ledger, &dirs(dir).stops, &who, input.as_bytes(), at).unwrap();
    }

    /// One member, `lh-1`, live in herdr with `status`.
    fn one(status: AgentStatus) -> (MemLedger, FakeHerdr) {
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lh-1", status);
        (ledger, herdr)
    }

    fn timeout(secs: u64) -> Request<'static> {
        Request {
            timeout: Some(Duration::from_secs(secs)),
            ..EVERY
        }
    }

    const SHELL: &str = r#"{"id":"b1","type":"shell","status":"running","command":"sleep 240"}"#;
    const CRON: &str = r#"{"id":"c1","cron":"*/5 * * * *","prompt":"check"}"#;

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
        let woke = watch(herdr, ledger, &dirs(dir), req, clock, &mut |_| {}).unwrap();
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
    fn a_member_finishing_work_while_watched_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Working);
        herdr.set_status("lh-2", AgentStatus::Working);

        let mut clock = steps(|poll| match poll {
            1 => {}
            2 => {
                herdr.set_status("lh-1", AgentStatus::Done);
                herdr.set_status("lh-2", AgentStatus::Idle);
            }
            // A stop with no record yet waits one poll for the hook.
            3 => {}
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            [
                "lh-1 done  (was working, no report)",
                "lh-2 idle  (was working, no report)"
            ]
        );
    }

    #[test]
    fn a_member_already_done_at_start_waits_for_its_report() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Done);

        let mut clock = steps(|poll| match poll {
            1 | 2 => {}
            3 => send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9"),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 done 2026-11-01 09:00 PR #9"]
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

        let woke = watch(
            &herdr,
            &ledger,
            &dirs(dir.path()),
            &EVERY,
            &mut clock,
            &mut |w| warnings.push(w.to_string()),
        )
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
    fn a_working_member_whose_pane_disappears_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Working);

        let mut clock = steps(|poll| match poll {
            1 => herdr.remove_agent("lh-2"),
            2 => {}
            3 => herdr.remove_agent("lh-1"),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 gone  (was working, no report)"]
        );
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

    #[test]
    fn a_report_read_in_the_poll_of_the_stop_keeps_the_plain_wake() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();

        let mut clock = steps(|poll| match poll {
            1 => {}
            2 => {
                send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9");
                send(&ledger, dir.path(), "lh-2", Kind::Question, "which DB?");
                herdr.set_status("lh-1", AgentStatus::Idle);
                herdr.set_status("lh-2", AgentStatus::Idle);
            }
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            [
                "lh-2 question 2026-11-01 09:00 which DB?",
                "lh-1 done 2026-11-01 09:00 PR #9",
                "lh-1 idle  (was working)",
                "lh-2 idle  (was working)",
            ]
        );
    }

    #[test]
    fn a_report_consumed_by_an_earlier_watch_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();
        send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9");

        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut steps(|_| {})).unwrap(),
            ["lh-1 done 2026-11-01 09:00 PR #9"]
        );

        let mut clock = steps(|poll| match poll {
            1 => herdr.set_status("lh-1", AgentStatus::Idle),
            // A stop with no record yet waits one poll for the hook.
            2 => {}
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 idle  (was working, no report)"]
        );
    }

    #[test]
    fn notes_and_other_members_reports_do_not_count_as_a_report() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();

        let mut clock = steps(|poll| match poll {
            1 => {}
            2 => {
                send(&ledger, dir.path(), "lh-1", Kind::Note, "pausing");
                send(&ledger, dir.path(), "lh-2", Kind::Done, "PR #9");
                herdr.set_status("lh-1", AgentStatus::Idle);
            }
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            [
                "lh-1 note 2026-11-01 09:00 pausing",
                "lh-2 done 2026-11-01 09:00 PR #9",
                "lh-1 idle  (was working, no report)",
            ]
        );
    }

    #[test]
    fn a_member_that_stops_with_background_work_wakes_it_as_paused() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Working);
        herdr.set_status("lh-2", AgentStatus::Working);
        stop(&ledger, dir.path(), "lh-1", "2026-11-02T08:00:00Z", "", "");

        let mut clock = steps(|poll| match poll {
            1 => {}
            2 => {
                stop(
                    &ledger,
                    dir.path(),
                    "lh-1",
                    "2026-11-02T08:05:40Z",
                    SHELL,
                    "",
                );
                herdr.set_status("lh-1", AgentStatus::Done);
                stop(
                    &ledger,
                    dir.path(),
                    "lh-2",
                    "2026-11-02T08:05:41Z",
                    SHELL,
                    CRON,
                );
                herdr.set_status("lh-2", AgentStatus::Idle);
            }
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            [
                "lh-1 done  (paused: 1 shell)",
                "lh-2 idle  (paused: 1 shell, 1 cron)"
            ]
        );
    }

    #[test]
    fn a_stop_with_nothing_in_flight_or_a_past_turns_record_wakes_it_as_today() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![
            member("lh-1", "w9:p1", None),
            member("lh-2", "w9:p2", None),
        ]);
        let herdr = FakeHerdr::new(&[])
            .with_live_agent(Some("lh-1"), None, "w9:p1")
            .with_live_agent(Some("lh-2"), None, "w9:p2");
        herdr.set_status("lh-1", AgentStatus::Working);
        herdr.set_status("lh-2", AgentStatus::Working);
        stop(
            &ledger,
            dir.path(),
            "lh-2",
            "2026-11-02T08:00:00Z",
            SHELL,
            "",
        );

        let mut clock = steps(|poll| match poll {
            1 => {}
            2 => {
                stop(&ledger, dir.path(), "lh-1", "2026-11-02T08:05:40Z", "", "");
                herdr.set_status("lh-1", AgentStatus::Done);
                herdr.set_status("lh-2", AgentStatus::Done);
            }
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            [
                "lh-1 done  (was working, no report)",
                "lh-2 done  (was working, no report)"
            ]
        );

        // lh-2's record lands after its stop already woke the shepherd.
        stop(&ledger, dir.path(), "lh-2", "2026-11-02T08:05:43Z", "", "");
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &timeout(4), &mut steps(|_| {})),
            None
        );
    }

    #[test]
    fn a_paused_member_finishing_wakes_it_even_unseen_working() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lh-1", AgentStatus::Done);
        stop(
            &ledger,
            dir.path(),
            "lh-1",
            "2026-11-02T08:05:40Z",
            SHELL,
            "",
        );

        let mut clock = steps(|poll| match poll {
            1 | 2 => {}
            // The ~1s resume as `working` falls between polls.
            3 => stop(&ledger, dir.path(), "lh-1", "2026-11-02T08:09:52Z", "", ""),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 done  (was working, no report)"]
        );
    }

    #[test]
    fn a_paused_member_stopping_paused_again_unseen_working_does_not_wake_it() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(vec![member("lh-1", "w9:p1", None)]);
        let herdr = FakeHerdr::new(&[]).with_live_agent(Some("lh-1"), None, "w9:p1");
        herdr.set_status("lh-1", AgentStatus::Done);
        stop(
            &ledger,
            dir.path(),
            "lh-1",
            "2026-11-02T08:05:40Z",
            SHELL,
            "",
        );
        let req = Request {
            timeout: Some(Duration::from_secs(8)),
            ..EVERY
        };

        let mut clock = steps(|poll| {
            if poll == 2 {
                stop(
                    &ledger,
                    dir.path(),
                    "lh-1",
                    "2026-11-02T08:09:52Z",
                    SHELL,
                    "",
                );
            }
        });
        assert_eq!(run(&herdr, &ledger, dir.path(), &req, &mut clock), None);
    }

    #[test]
    fn a_finish_after_a_paused_wake_wakes_the_next_watch_whenever_it_lands() {
        for written_after_polls in [0, 2] {
            let dir = tempfile::tempdir().unwrap();
            let (ledger, herdr) = one(AgentStatus::Working);
            let mut clock = steps(|poll| {
                if poll == 1 {
                    stop(
                        &ledger,
                        dir.path(),
                        "lh-1",
                        "2026-11-02T08:05:40Z",
                        SHELL,
                        "",
                    );
                    herdr.set_status("lh-1", AgentStatus::Done);
                }
            });
            assert_eq!(
                run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
                ["lh-1 done  (paused: 1 shell)"]
            );

            // The shepherd re-arms; the resume as `working` falls between polls.
            let finish = || stop(&ledger, dir.path(), "lh-1", "2026-11-02T08:09:52Z", "", "");
            if written_after_polls == 0 {
                finish();
            }
            let mut clock = steps(|poll| {
                if poll == written_after_polls {
                    finish();
                }
            });
            assert_eq!(
                run(&herdr, &ledger, dir.path(), &timeout(60), &mut clock),
                Some(vec!["lh-1 done  (was working, no report)".to_string()]),
                "finish written after {written_after_polls} polls"
            );
        }
    }

    #[test]
    fn the_paused_label_survives_the_record_and_the_flip_to_done_landing_in_different_polls() {
        for record_first in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (ledger, herdr) = one(AgentStatus::Working);
            let paused = || {
                stop(
                    &ledger,
                    dir.path(),
                    "lh-1",
                    "2026-11-02T08:05:40Z",
                    SHELL,
                    "",
                )
            };
            let done = || herdr.set_status("lh-1", AgentStatus::Done);

            let mut clock = steps(|poll| match (poll, record_first) {
                (1, true) | (2, false) => paused(),
                (2, true) | (1, false) => done(),
                (3, _) => {}
                _ => panic!("slept past the wake"),
            });
            assert_eq!(
                run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
                ["lh-1 done  (paused: 1 shell)"],
                "record first: {record_first}"
            );
        }
    }

    #[test]
    fn a_member_without_a_stop_record_still_wakes_it_one_poll_later() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = one(AgentStatus::Working);

        let mut clock = steps(|poll| match poll {
            1 => herdr.set_status("lh-1", AgentStatus::Idle),
            2 => {}
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 idle  (was working, no report)"]
        );

        herdr.set_status("lh-1", AgentStatus::Working);
        let mut clock = steps(|poll| {
            if poll == 1 {
                herdr.set_status("lh-1", AgentStatus::Done);
            }
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &timeout(3), &mut clock).unwrap(),
            ["lh-1 done  (was working, no report)"],
            "the timeout doesn't swallow a stop still waiting on its record"
        );
    }

    #[test]
    fn a_stop_whose_background_work_is_unknown_wakes_it_saying_so() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = one(AgentStatus::Done);
        stop(
            &ledger,
            dir.path(),
            "lh-1",
            "2026-11-02T08:05:40Z",
            SHELL,
            "",
        );

        let mut clock = steps(|poll| match poll {
            1 => stop_with(&ledger, dir.path(), "lh-1", "2026-11-02T08:09:52Z", "{}"),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 done  (was working, background unknown, no report)"]
        );
    }

    #[test]
    fn a_looping_member_wakes_it_once_when_it_pauses_and_again_when_it_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = one(AgentStatus::Working);
        let mut clock = steps(|poll| {
            if poll == 1 {
                stop(
                    &ledger,
                    dir.path(),
                    "lh-1",
                    "2026-11-02T08:00:00Z",
                    "",
                    CRON,
                );
                herdr.set_status("lh-1", AgentStatus::Done);
            }
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 done  (paused: 1 cron)"]
        );

        // Each turn the cron fires ends paused on it again.
        let mut clock = steps(|poll| match poll % 3 {
            1 => herdr.set_status("lh-1", AgentStatus::Working),
            2 => {
                let at = format!("2026-11-02T08:{:02}:00Z", poll);
                stop(&ledger, dir.path(), "lh-1", &at, "", CRON);
                herdr.set_status("lh-1", AgentStatus::Done);
            }
            _ => {}
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &timeout(60), &mut clock),
            None
        );

        let mut clock = steps(|poll| match poll {
            1 => herdr.set_status("lh-1", AgentStatus::Working),
            2 => {
                stop(&ledger, dir.path(), "lh-1", "2026-11-02T09:00:00Z", "", "");
                herdr.set_status("lh-1", AgentStatus::Done);
            }
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 done  (was working, no report)"]
        );
    }
}
