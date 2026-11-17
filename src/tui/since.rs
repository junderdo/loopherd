//! How long each member has been in its status, as seen by this TUI: herdr
//! has no timestamps, so only changes observed while open have a time.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::herd_view::{Member, View};
use crate::herdr::AgentStatus;
use crate::status::sections;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Since(BTreeMap<String, Seen>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    status: &'static str,
    /// `None` until a change is observed.
    since: Option<Instant>,
}

impl Since {
    /// The tracking after one more snapshot taken at `now`.
    pub fn observe(&self, view: &View, now: Instant) -> Since {
        if view.strays.count.is_none() {
            // herdr's agents weren't read: every status is a placeholder.
            return self.clone();
        }
        let members = sections(view).into_iter().flat_map(|(_, members)| members);
        Since(
            members
                .map(|m| {
                    let status = label(m);
                    let since = match self.0.get(&m.name) {
                        Some(seen) if seen.status == status => seen.since,
                        Some(_) => Some(now),
                        None => None,
                    };
                    (m.name.clone(), Seen { status, since })
                })
                .collect(),
        )
    }

    /// How long `member` has been in its status, if a change was observed.
    pub fn of(&self, member: &str, now: Instant) -> Option<Duration> {
        let since = self.0.get(member)?.since?;
        Some(now.saturating_duration_since(since))
    }
}

const WORKING_STALE: Duration = Duration::from_secs(30 * 60);
const WORKING_OVERDUE: Duration = Duration::from_secs(60 * 60);
const BLOCKED_OVERDUE: Duration = Duration::from_secs(5 * 60);

/// How long a member has sat in its status, against how long it should.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    Fresh,
    Stale,
    Overdue,
}

pub fn staleness(member: &Member, time: Duration) -> Staleness {
    match (member.gone, member.status) {
        (false, AgentStatus::Working) if time >= WORKING_OVERDUE => Staleness::Overdue,
        (false, AgentStatus::Working) if time >= WORKING_STALE => Staleness::Stale,
        (false, AgentStatus::Blocked) if time >= BLOCKED_OVERDUE => Staleness::Overdue,
        _ => Staleness::Fresh,
    }
}

/// A member's status as shown: `gone` or herdr's status.
pub fn label(member: &Member) -> &'static str {
    match member.gone {
        true => "gone",
        false => member.status.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herd_view::Strays;

    fn member(name: &str, status: AgentStatus, gone: bool) -> Member {
        Member {
            name: name.into(),
            status,
            gone,
            branch: None,
            worktree: None,
            worktree_path: None,
            pane: format!("w1:p{name}"),
            tab: None,
            lead: false,
            template: None,
        }
    }

    fn snapshot(members: Vec<Member>) -> View {
        View {
            herdr_reachable: true,
            herds: vec![],
            ungrouped: members,
            retired: None,
            strays: Strays {
                count: Some(0),
                agents: None,
            },
        }
    }

    fn unread(members: Vec<Member>) -> View {
        View {
            herdr_reachable: false,
            strays: Strays {
                count: None,
                agents: None,
            },
            ..snapshot(members)
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn mins(n: u64) -> Duration {
        secs(n * 60)
    }

    #[test]
    fn working_turns_stale_at_30_minutes_and_overdue_at_60() {
        let working = member("a", AgentStatus::Working, false);
        for (time, expected) in [
            (mins(30) - secs(1), Staleness::Fresh),
            (mins(30), Staleness::Stale),
            (mins(60) - secs(1), Staleness::Stale),
            (mins(60), Staleness::Overdue),
        ] {
            assert_eq!(staleness(&working, time), expected, "{time:?}");
        }
    }

    #[test]
    fn blocked_turns_overdue_at_5_minutes() {
        let blocked = member("a", AgentStatus::Blocked, false);
        assert_eq!(staleness(&blocked, mins(5) - secs(1)), Staleness::Fresh);
        assert_eq!(staleness(&blocked, mins(5)), Staleness::Overdue);
    }

    #[test]
    fn other_statuses_and_gone_members_never_go_stale() {
        for m in [
            member("a", AgentStatus::Idle, false),
            member("a", AgentStatus::Done, false),
            member("a", AgentStatus::Unknown, false),
            member("a", AgentStatus::Working, true),
            member("a", AgentStatus::Blocked, true),
        ] {
            assert_eq!(staleness(&m, mins(600)), Staleness::Fresh, "{m:?}");
        }
    }

    #[test]
    fn a_status_first_seen_has_no_time() {
        let t0 = Instant::now();
        let since = Since::default().observe(
            &snapshot(vec![member("a", AgentStatus::Working, false)]),
            t0,
        );
        assert_eq!(since.of("a", t0 + secs(30)), None);
    }

    #[test]
    fn a_change_starts_the_clock_and_an_unchanged_status_keeps_it() {
        let t0 = Instant::now();
        let since = Since::default()
            .observe(
                &snapshot(vec![member("a", AgentStatus::Working, false)]),
                t0,
            )
            .observe(
                &snapshot(vec![member("a", AgentStatus::Blocked, false)]),
                t0 + secs(2),
            )
            .observe(
                &snapshot(vec![member("a", AgentStatus::Blocked, false)]),
                t0 + secs(4),
            );
        assert_eq!(since.of("a", t0 + secs(182)), Some(secs(180)));
    }

    #[test]
    fn going_gone_is_a_change() {
        let t0 = Instant::now();
        let since = Since::default()
            .observe(&snapshot(vec![member("a", AgentStatus::Idle, false)]), t0)
            .observe(
                &snapshot(vec![member("a", AgentStatus::Unknown, true)]),
                t0 + secs(2),
            );
        assert_eq!(since.of("a", t0 + secs(5)), Some(secs(3)));
    }

    #[test]
    fn a_member_that_leaves_and_returns_starts_over_without_a_time() {
        let t0 = Instant::now();
        let since = Since::default()
            .observe(&snapshot(vec![member("a", AgentStatus::Idle, false)]), t0)
            .observe(
                &snapshot(vec![member("a", AgentStatus::Working, false)]),
                t0 + secs(2),
            )
            .observe(&snapshot(vec![]), t0 + secs(4))
            .observe(
                &snapshot(vec![member("a", AgentStatus::Working, false)]),
                t0 + secs(6),
            );
        assert_eq!(since.of("a", t0 + secs(8)), None);
    }

    #[test]
    fn a_snapshot_without_herdrs_agents_changes_nothing() {
        let t0 = Instant::now();
        let before = Since::default()
            .observe(&snapshot(vec![member("a", AgentStatus::Idle, false)]), t0)
            .observe(
                &snapshot(vec![member("a", AgentStatus::Working, false)]),
                t0 + secs(2),
            );
        let after = before.observe(
            &unread(vec![member("a", AgentStatus::Unknown, false)]),
            t0 + secs(4),
        );
        assert_eq!(after, before);
    }

    #[test]
    fn tracks_herd_and_retired_members_too() {
        let t0 = Instant::now();
        let view = |status| View {
            herds: vec![crate::herd_view::Herd {
                name: "prs".into(),
                paused: None,
                members: vec![member("h", status, false)],
            }],
            retired: Some(vec![member("r", status, false)]),
            ..snapshot(vec![])
        };
        let since = Since::default()
            .observe(&view(AgentStatus::Idle), t0)
            .observe(&view(AgentStatus::Done), t0 + secs(1));
        assert_eq!(since.of("h", t0 + secs(2)), Some(secs(1)));
        assert_eq!(since.of("r", t0 + secs(2)), Some(secs(1)));
    }
}
