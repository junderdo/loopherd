//! `loopherd watch`: block until a member needs the shepherd.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::herd_view::{Member, Options, View};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::report::{Filter, Read, reports};
use crate::status;
use crate::stop::{self, Stop};

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
    // Each member's stop record at the last poll.
    let mut stops: HashMap<String, Option<Stop>> = HashMap::new();
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
            true => Read {
                lines: vec![],
                warnings: vec![],
            },
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
        let mut lines = read.lines;
        for member in &members {
            let was = last.get(&member.name).copied();
            let stop = latest_stop(&dirs.stops, &member.name, warn);
            // Only a record new since the last poll is this stop's; an older one
            // is a past turn's, or the hook hasn't written yet.
            let before = stops.insert(member.name.clone(), stop.clone());
            let fresh = before.as_ref().is_some_and(|before| *before != stop);
            let paused = stop.as_ref().filter(|_| fresh).and_then(Stop::paused);
            let was_paused = before.flatten().as_ref().and_then(Stop::paused).is_some();
            if member.gone {
                if was == Some(AgentStatus::Working) {
                    lines.push(format!("{} gone  (was working)", member.name));
                }
                continue;
            }
            match member.status {
                // Only on change: one blocked at start would re-wake every re-armed watch.
                AgentStatus::Blocked if was.is_some_and(|w| w != AgentStatus::Blocked) => {
                    lines.push(format!("{} blocked", member.name))
                }
                now @ (AgentStatus::Done | AgentStatus::Idle)
                    if was == Some(AgentStatus::Working) =>
                {
                    let why = paused.map_or("was working".into(), |p| format!("paused: {p}"));
                    lines.push(format!("{} {}  ({why})", member.name, now.as_str()))
                }
                // Its resume shows as working for about a second, so a poll can
                // miss it; the paused record turning empty is the finish.
                now @ (AgentStatus::Done | AgentStatus::Idle)
                    if fresh && was_paused && paused.is_none() =>
                {
                    lines.push(format!("{} {}  (was working)", member.name, now.as_str()))
                }
                _ => {}
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
    use crate::report::{Kind, Reporter, report};
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
        let who = Reporter {
            own_pane: None,
            as_name: Some(name),
        };
        let input = format!(r#"{{"background_tasks":[{tasks}],"session_crons":[{crons}]}}"#);
        let at = humantime::parse_rfc3339(at).unwrap();
        stop::record(ledger, &dirs(dir).stops, &who, input.as_bytes(), at).unwrap();
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
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 done  (was working)", "lh-2 idle  (was working)"]
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
            ["lh-1 gone  (was working)"]
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
            ["lh-1 done  (was working)", "lh-2 done  (was working)"]
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
            ["lh-1 done  (was working)"]
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
}
