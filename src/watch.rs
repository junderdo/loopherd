//! `loopherd watch`: block until a member needs the shepherd.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::herd_view::{Member, Options, View};
use crate::herdr::{AgentStatus, Herdr};
use crate::ledger::Ledger;
use crate::report::{Filter, Kind, Read, reports};
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
/// as herdr becomes unreadable and for each bad report line.
pub fn watch(
    herdr: &dyn Herdr,
    ledger: &dyn Ledger,
    dir: &Path,
    req: &Request,
    clock: &mut dyn Clock,
    warn: &mut dyn FnMut(&str),
) -> Result<Option<Vec<String>>> {
    // Each member's last known status; an unreadable poll keeps it.
    let mut last: HashMap<String, AgentStatus> = HashMap::new();
    // Reports are consumed as read, so whether one came since a member went
    // working is only known from what this run saw.
    let mut went_working: HashMap<String, u64> = HashMap::new();
    let mut reported: HashMap<String, u64> = HashMap::new();
    let mut code = 0;
    let mut poll: u64 = 0;
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
                dir,
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
        for (name, kind) in read.senders {
            if kind != Kind::Note {
                reported.insert(name, poll);
            }
        }
        let mut lines = read.lines;
        for member in &members {
            let was = last.get(&member.name).copied();
            let stopped = |now: &str| {
                let since = went_working.get(&member.name);
                let note = match reported.get(&member.name) >= since {
                    true => "",
                    false => ", no report",
                };
                format!("{} {now}  (was working{note})", member.name)
            };
            if member.gone {
                if was == Some(AgentStatus::Working) {
                    lines.push(stopped("gone"));
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
                    lines.push(stopped(now.as_str()))
                }
                AgentStatus::Working if was != Some(AgentStatus::Working) => {
                    went_working.insert(member.name.clone(), poll);
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
        poll += 1;
    }
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
    fn a_report_filed_before_the_stop_keeps_the_plain_wake() {
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
    fn a_report_filed_after_the_stop_still_counts_when_read_in_the_same_poll() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();

        let mut clock = steps(|poll| match poll {
            1 => {}
            2 => {
                herdr.set_status("lh-1", AgentStatus::Done);
                send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9");
            }
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            [
                "lh-1 done 2026-11-01 09:00 PR #9",
                "lh-1 done  (was working)"
            ]
        );
    }

    #[test]
    fn a_report_filed_after_the_wake_is_left_for_the_next_watch() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, herdr) = working_pair();

        let mut clock = steps(|poll| match poll {
            1 => herdr.set_status("lh-1", AgentStatus::Idle),
            _ => panic!("slept past the wake"),
        });
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut clock).unwrap(),
            ["lh-1 idle  (was working, no report)"]
        );

        send(&ledger, dir.path(), "lh-1", Kind::Done, "PR #9");
        assert_eq!(
            run(&herdr, &ledger, dir.path(), &EVERY, &mut steps(|_| {})).unwrap(),
            ["lh-1 done 2026-11-01 09:00 PR #9"]
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
}
