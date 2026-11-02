//! Ledger members grouped by herd with herdr's live status. No I/O: the
//! `status` command and the TUI feed it what they read.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;

use crate::herdr::{Agent, AgentStatus};
use crate::ledger::AgentRecord;
use crate::matching::live_agent;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Show retired entries and herds with only retired entries.
    pub all: bool,
    /// List strays, not just count them.
    pub strays: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub name: String,
    pub status: AgentStatus,
    /// Not retired, yet no live agent matches it.
    pub gone: bool,
    pub branch: Option<String>,
    /// The worktree's basename; `None` when the entry has no worktree.
    pub worktree: Option<String>,
    pub pane: String,
    pub tab: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Herd {
    pub name: String,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct View {
    pub herdr_reachable: bool,
    pub herds: Vec<Herd>,
    pub ungrouped: Vec<Member>,
    /// Only with [`Options::all`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired: Option<Vec<Member>>,
    pub strays: Strays,
}

/// Live herdr agents no ledger entry matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Strays {
    /// `None` when herdr couldn't be read.
    pub count: Option<usize>,
    /// Only with [`Options::strays`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<Stray>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stray {
    pub pane: String,
    pub tab: String,
    pub status: AgentStatus,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
}

/// `agents` is `None` when herdr couldn't be read.
pub fn view(
    entries: &BTreeMap<String, AgentRecord>,
    agents: Option<&[Agent]>,
    options: Options,
    branch: impl Fn(&AgentRecord) -> Option<String>,
) -> View {
    let mut herds: BTreeMap<&str, Vec<Member>> = BTreeMap::new();
    let mut ungrouped = vec![];
    let mut retired = vec![];
    for record in entries.values() {
        let group = match &record.herd {
            Some(herd) => herds.entry(herd).or_default(),
            None => &mut ungrouped,
        };
        match (record.retired, options.all) {
            (false, _) => group.push(member(record, agents, &branch)),
            (true, true) => retired.push(member(record, agents, &branch)),
            (true, false) => {}
        }
    }
    View {
        herdr_reachable: agents.is_some(),
        herds: herds
            .into_iter()
            .filter(|(_, members)| options.all || !members.is_empty())
            .map(|(name, members)| Herd {
                name: name.to_string(),
                members: sorted(members),
            })
            .collect(),
        ungrouped: sorted(ungrouped),
        retired: options.all.then(|| sorted(retired)),
        strays: strays(entries, agents, options),
    }
}

fn strays(
    entries: &BTreeMap<String, AgentRecord>,
    agents: Option<&[Agent]>,
    options: Options,
) -> Strays {
    let Some(agents) = agents else {
        return Strays {
            count: None,
            agents: None,
        };
    };
    let herded: BTreeSet<&str> = entries
        .values()
        .filter_map(|r| live_agent(r, agents))
        .map(|a| a.pane_id.as_str())
        .collect();
    let mut strays: Vec<Stray> = agents
        .iter()
        .filter(|a| !herded.contains(a.pane_id.as_str()))
        .map(|a| Stray {
            pane: a.pane_id.clone(),
            tab: a.tab_id.clone(),
            status: a.status,
            cwd: a.cwd.clone(),
            title: a.title.clone(),
        })
        .collect();
    strays.sort_by(|a, b| a.pane.cmp(&b.pane));
    Strays {
        count: Some(strays.len()),
        agents: options.strays.then_some(strays),
    }
}

/// What needs the human first.
fn sorted(mut members: Vec<Member>) -> Vec<Member> {
    members.sort_by_key(|m| (rank(m), m.name.clone()));
    members
}

fn rank(member: &Member) -> u8 {
    if member.gone {
        return 5;
    }
    match member.status {
        AgentStatus::Blocked => 0,
        AgentStatus::Done => 1,
        AgentStatus::Idle => 2,
        AgentStatus::Working => 3,
        AgentStatus::Unknown => 4,
    }
}

fn member(
    record: &AgentRecord,
    agents: Option<&[Agent]>,
    branch: impl Fn(&AgentRecord) -> Option<String>,
) -> Member {
    let live = agents.and_then(|a| live_agent(record, a));
    Member {
        name: record.name.clone(),
        status: live.map_or(AgentStatus::Unknown, |a| a.status),
        gone: !record.retired && agents.is_some() && live.is_none(),
        branch: branch(record),
        worktree: record.worktree.as_ref().map(|dir| {
            dir.file_name()
                .map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into())
        }),
        pane: live.map_or(&record.pane_id, |a| &a.pane_id).clone(),
        tab: live.map(|a| a.tab_id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, herd: Option<&str>) -> AgentRecord {
        let n = name.len();
        AgentRecord {
            name: name.into(),
            pane_id: format!("w1:p{name}"),
            session_id: Some(format!("sess-{name}")),
            repo: "/src/repo".into(),
            worktree: Some(format!("/src/wt/{name}").into()),
            workspace: "w1".into(),
            workspace_label: Some("main".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: format!("2026-11-01T00:00:{n:02}Z"),
            retired: false,
            retired_at: None,
            herd: herd.map(str::to_string),
            template: herd.map(|_| "t".to_string()),
            params: None,
        }
    }

    fn live(name: Option<&str>, pane: &str, session: Option<&str>, status: AgentStatus) -> Agent {
        Agent {
            name: name.map(str::to_string),
            pane_id: pane.into(),
            tab_id: pane.replace(":p", ":t"),
            status,
            session_id: session.map(str::to_string),
            launch_pending: false,
            cwd: None,
            title: None,
        }
    }

    fn named(name: &str, status: AgentStatus) -> Agent {
        live(
            Some(name),
            &format!("w1:p{name}"),
            Some(&format!("sess-{name}")),
            status,
        )
    }

    fn ledger(entries: Vec<AgentRecord>) -> BTreeMap<String, AgentRecord> {
        entries.into_iter().map(|e| (e.name.clone(), e)).collect()
    }

    fn no_branch(_: &AgentRecord) -> Option<String> {
        None
    }

    fn names(members: &[Member]) -> Vec<&str> {
        members.iter().map(|m| m.name.as_str()).collect()
    }

    #[test]
    fn groups_herd_entries_under_their_herd_and_plain_spawns_under_ungrouped() {
        let entries = ledger(vec![
            entry("a", Some("prs")),
            entry("b", None),
            entry("c", Some("prs")),
        ]);
        let agents = ["a", "b", "c"].map(|n| named(n, AgentStatus::Idle));
        let view = view(&entries, Some(&agents), Options::default(), no_branch);
        assert_eq!(view.herds.len(), 1);
        assert_eq!(view.herds[0].name, "prs");
        assert_eq!(names(&view.herds[0].members), ["a", "c"]);
        assert_eq!(names(&view.ungrouped), ["b"]);
    }

    #[test]
    fn sorts_a_group_blocked_done_idle_working_unknown_then_gone_and_by_name_within_a_status() {
        let names_in_ledger = ["a", "b", "c", "d", "e", "f", "g"];
        let entries = ledger(names_in_ledger.map(|n| entry(n, Some("prs"))).to_vec());
        let agents = [
            named("a", AgentStatus::Working),
            named("b", AgentStatus::Idle),
            named("c", AgentStatus::Unknown),
            named("d", AgentStatus::Done),
            named("f", AgentStatus::Blocked),
            named("g", AgentStatus::Idle),
        ];
        let view = view(&entries, Some(&agents), Options::default(), no_branch);
        assert_eq!(
            names(&view.herds[0].members),
            ["f", "d", "b", "g", "a", "c", "e"]
        );
        assert!(view.herds[0].members[6].gone);
    }

    #[test]
    fn orders_herds_by_name() {
        let entries = ledger(vec![entry("a", Some("zeta")), entry("b", Some("alpha"))]);
        let agents = [named("a", AgentStatus::Idle), named("b", AgentStatus::Idle)];
        let view = view(&entries, Some(&agents), Options::default(), no_branch);
        let herds: Vec<_> = view.herds.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(herds, ["alpha", "zeta"]);
    }

    #[test]
    fn matches_an_unnamed_or_renamed_live_agent_by_its_recorded_pane_and_session() {
        let entries = ledger(vec![entry("a", None), entry("b", None)]);
        let agents = [
            live(None, "w1:pa", Some("sess-a"), AgentStatus::Blocked),
            live(Some("renamed"), "w1:pb", Some("sess-b"), AgentStatus::Done),
        ];
        let view = view(&entries, Some(&agents), Options::default(), no_branch);
        let a = &view.ungrouped[0];
        assert_eq!(
            (a.name.as_str(), a.status, a.gone),
            ("a", AgentStatus::Blocked, false)
        );
        assert_eq!(a.tab.as_deref(), Some("w1:ta"));
        let b = &view.ungrouped[1];
        assert_eq!(
            (b.name.as_str(), b.status, b.gone),
            ("b", AgentStatus::Done, false)
        );
    }

    #[test]
    fn a_recorded_pane_running_another_session_is_not_a_match() {
        let entries = ledger(vec![entry("a", None)]);
        let agents = [live(None, "w1:pa", Some("sess-other"), AgentStatus::Idle)];
        let view = view(&entries, Some(&agents), Options::default(), no_branch);
        assert!(view.ungrouped[0].gone);
        assert_eq!(view.ungrouped[0].tab, None);
    }

    #[test]
    fn a_name_held_by_another_session_is_not_a_match() {
        let entries = ledger(vec![entry("a", None)]);
        let agents = [live(
            Some("a"),
            "w1:p7",
            Some("sess-other"),
            AgentStatus::Idle,
        )];
        let view = view(&entries, Some(&agents), Options::default(), no_branch);
        assert!(view.ungrouped[0].gone);
    }

    #[test]
    fn a_name_match_counts_while_either_side_has_no_session_yet() {
        let mut fresh = entry("a", None);
        fresh.session_id = None;
        let entries = ledger(vec![fresh]);
        let agents = [live(
            Some("a"),
            "w1:p7",
            Some("sess-new"),
            AgentStatus::Working,
        )];
        let view = view(&entries, Some(&agents), Options::default(), no_branch);
        assert_eq!(view.ungrouped[0].status, AgentStatus::Working);
        assert_eq!(view.ungrouped[0].pane, "w1:p7");
    }

    fn retired(name: &str, herd: Option<&str>) -> AgentRecord {
        AgentRecord {
            retired: true,
            ..entry(name, herd)
        }
    }

    const ALL: Options = Options {
        all: true,
        strays: false,
    };

    #[test]
    fn hides_retired_entries_unless_all_shows_them_in_their_own_section() {
        let entries = ledger(vec![
            entry("a", Some("prs")),
            retired("b", Some("prs")),
            retired("c", None),
        ]);
        let agents = [named("a", AgentStatus::Idle), named("c", AgentStatus::Done)];

        let default = view(&entries, Some(&agents), Options::default(), no_branch);
        assert_eq!(names(&default.herds[0].members), ["a"]);
        assert!(default.ungrouped.is_empty());
        assert_eq!(default.retired, None);

        let all = view(&entries, Some(&agents), ALL, no_branch);
        assert_eq!(names(&all.herds[0].members), ["a"]);
        let retired = all.retired.unwrap();
        assert_eq!(names(&retired), ["c", "b"]);
        assert!(!retired[1].gone, "a retired entry is never gone");
    }

    #[test]
    fn looks_up_branches_only_for_entries_it_shows() {
        let entries = ledger(vec![
            entry("a", Some("prs")),
            retired("b", Some("prs")),
            retired("c", None),
        ]);
        let looked_up = std::cell::RefCell::new(vec![]);
        let counting = |r: &AgentRecord| {
            looked_up.borrow_mut().push(r.name.clone());
            None
        };

        view(&entries, Some(&[]), Options::default(), counting);
        assert_eq!(*looked_up.borrow(), ["a"]);

        looked_up.borrow_mut().clear();
        view(&entries, Some(&[]), ALL, counting);
        assert_eq!(*looked_up.borrow(), ["a", "b", "c"]);
    }

    #[test]
    fn hides_a_herd_with_only_retired_entries_unless_all() {
        let entries = ledger(vec![
            entry("a", Some("live")),
            retired("c", Some("retired")),
        ]);
        let agents = [named("a", AgentStatus::Idle)];

        let default = view(&entries, Some(&agents), Options::default(), no_branch);
        let herds: Vec<_> = default.herds.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(herds, ["live"]);

        let all = view(&entries, Some(&agents), ALL, no_branch);
        let herds: Vec<_> = all
            .herds
            .iter()
            .map(|h| (h.name.as_str(), h.members.len()))
            .collect();
        assert_eq!(herds, [("live", 1), ("retired", 0)]);
    }

    #[test]
    fn shows_a_herd_whose_members_are_all_gone_without_all() {
        let entries = ledger(vec![entry("b", Some("gone")), entry("d", Some("gone"))]);
        let view = view(&entries, Some(&[]), Options::default(), no_branch);
        assert_eq!(view.herds.len(), 1);
        assert_eq!(view.herds[0].name, "gone");
        assert_eq!(names(&view.herds[0].members), ["b", "d"]);
        assert!(view.herds[0].members.iter().all(|m| m.gone));
    }

    #[test]
    fn with_herdr_unreachable_every_member_is_unknown_and_none_is_gone() {
        let entries = ledger(vec![entry("a", Some("prs")), entry("b", None)]);
        let view = view(&entries, None, Options::default(), no_branch);
        assert!(!view.herdr_reachable);
        let members = view.herds[0].members.iter().chain(&view.ungrouped);
        for m in members {
            assert_eq!(
                (m.status, m.gone, m.tab.as_deref()),
                (AgentStatus::Unknown, false, None)
            );
        }
    }

    #[test]
    fn counts_live_agents_no_entry_matches_as_strays_and_lists_them_only_when_asked() {
        let entries = ledger(vec![entry("a", None), retired("r", None)]);
        let mut stray = live(None, "w2:p3", Some("sess-x"), AgentStatus::Working);
        stray.cwd = Some("/src/other".into());
        stray.title = Some("fixing CI".into());
        let agents = [
            named("a", AgentStatus::Idle),
            named("r", AgentStatus::Idle),
            live(Some("a"), "w1:p9", Some("sess-impostor"), AgentStatus::Idle),
            stray,
        ];
        // `a`'s name is taken by `a` itself, so the impostor matches nothing.

        let counted = view(&entries, Some(&agents), Options::default(), no_branch);
        assert_eq!(counted.strays.count, Some(2));
        assert_eq!(counted.strays.agents, None);

        let listed = view(
            &entries,
            Some(&agents),
            Options {
                all: false,
                strays: true,
            },
            no_branch,
        );
        let panes: Vec<_> = listed
            .strays
            .agents
            .as_ref()
            .unwrap()
            .iter()
            .map(|s| s.pane.as_str())
            .collect();
        assert_eq!(panes, ["w1:p9", "w2:p3"]);
        assert_eq!(
            listed.strays.agents.unwrap()[1],
            Stray {
                pane: "w2:p3".into(),
                tab: "w2:t3".into(),
                status: AgentStatus::Working,
                cwd: Some("/src/other".into()),
                title: Some("fixing CI".into()),
            }
        );
    }

    #[test]
    fn with_herdr_unreachable_the_stray_count_is_unknown() {
        let view = view(&BTreeMap::new(), None, Options::default(), no_branch);
        assert_eq!(view.strays.count, None);
    }
}
