//! `loopherd retire`: mark a ledger entry finished so revive leaves it alone.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Result, bail};

use crate::herdr::{Agent, Herdr, Pane};
use crate::ledger::{AgentRecord, Ledger};
use crate::matching::{by_name, same_session, session_conflict};
use crate::notify::forget_marks;
use crate::process::{Procs, is_ancestor, live};
use crate::report::member_in;
use crate::revive::find_workspace;
use crate::tool_hook::Pane as OwnPane;
use crate::worktree::{Gone, Removal, remove_if_gone, remove_if_merged, same_dir};

/// Fails when `name` is a lead recorded in `pane`, the pane running loopherd,
/// or whose live recorded process is an ancestor of `me`, which no
/// environment can hide: a lead that retired itself would drop its own gates
/// (D50, D58).
pub fn refuse_own_lead(
    ledger: &dyn Ledger,
    name: &str,
    pane: Option<&OwnPane>,
    procs: &dyn Procs,
    me: u32,
) -> Result<()> {
    let Some(entry) = ledger.entries()?.remove(name).filter(|e| e.is_lead()) else {
        return Ok(());
    };
    let in_pane = pane.is_some_and(|pane| {
        pane.socket.is_some() && entry.pane_id == pane.id && entry.socket.as_deref() == pane.socket
    });
    if in_pane {
        bail!(
            "{name} is the lead in this pane; retiring a lead is the shepherd's or the human's job"
        );
    }
    if live(procs, entry.process).is_some_and(|lead| is_ancestor(procs, lead.pid, me)) {
        bail!(
            "{name} is the lead running this loopherd; retiring a lead is the shepherd's or the human's job"
        );
    }
    Ok(())
}

/// Marks `name` retired at `now`. Touches only the ledger.
pub fn retire(ledger: &mut dyn Ledger, name: &str, now: SystemTime) -> Result<AgentRecord> {
    ledger.update(name, &mut |record| {
        if record.retired {
            bail!("{name} is {}", retired_since(record));
        }
        record.retired = true;
        record.retired_at = Some(humantime::format_rfc3339_seconds(now).to_string());
        Ok(())
    })
}

fn retired_since(record: &AgentRecord) -> String {
    let since = record.retired_at.as_deref().unwrap_or("an unknown time");
    format!("already retired (since {since})")
}

/// Retires `name` unless it already is, removes its notify marks from
/// `mailboxes`, then closes its tab and removes its worktree when safe, so a
/// failed or deferred close can be re-run. `own_pane` is the pane running
/// loopherd, whose tab is never closed. Pushes one line per action to `out`,
/// including before an error; a mark left in place is a warning line.
pub fn retire_and_close(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    name: &str,
    now: SystemTime,
    mailboxes: &Path,
    own_pane: Option<&str>,
    out: &mut Vec<String>,
) -> Result<()> {
    let mut others = ledger.entries()?;
    let record = match others.remove(name) {
        Some(record) if record.retired => {
            out.push(format!("{name} is {}", retired_since(&record)));
            record
        }
        _ => {
            let record = retire(ledger, name, now)?;
            out.push(format!("retired {name}"));
            record
        }
    };
    out.extend(forget_marks(mailboxes, name));
    let tab_open = match agent_tab(herdr, &record, &others, own_pane)? {
        TabFate::Gone => {
            out.push("tab already gone".into());
            None
        }
        TabFate::Ours(tab_id) => {
            herdr.close_tab(&tab_id)?;
            out.push(format!("closed tab {tab_id}"));
            None
        }
        TabFate::Kept { tab_id, reason } | TabFate::Taken { tab_id, reason } => {
            out.push(format!("kept tab {tab_id}: {reason}"));
            Some("its tab was kept")
        }
        TabFate::Busy(tab_id) => {
            out.push(format!("kept tab {tab_id}: {BUSY}"));
            Some("its tab was kept")
        }
        TabFate::NameHeld(tab_id) => {
            out.push(format!("kept tab {tab_id}: {NAME_HELD}"));
            Some("its tab was kept")
        }
        TabFate::Unidentified { tab_id, .. } => {
            out.push(format!("kept tab {tab_id}: {UNIDENTIFIED}"));
            Some("its tab was kept")
        }
        TabFate::Unmatched { line, .. } | TabFate::NoWorkspace { line, .. } => {
            out.push(line);
            Some("tab not confirmed closed")
        }
    };
    if let Some(path) = &record.worktree {
        if let Some(other) = other_user(ledger, name, path)? {
            out.push(format!("kept {}: in use by {other}", path.display()));
            return Ok(());
        }
        match remove_if_gone(&record.repo, path)? {
            Some(Gone::Unlisted) => {
                out.push(format!("worktree {} already gone", path.display()));
                return Ok(());
            }
            Some(Gone::Unchecked(reason)) => {
                out.push(format!("kept {}: {reason}", path.display()));
                return Ok(());
            }
            Some(Gone::Locked { reason }) => {
                let reason = reason.map(|r| format!(" ({r})")).unwrap_or_default();
                out.push(format!("kept {}: locked{reason}", path.display()));
                return Ok(());
            }
            Some(Gone::Removed { kept_branch }) => {
                out.push(format!("removed {} (already gone)", path.display()));
                if let Some((branch, reason)) = kept_branch {
                    out.push(format!("kept branch {branch}: {reason}"));
                }
                return Ok(());
            }
            None => {}
        }
        let removal = match tab_open {
            Some(reason) => Removal::Kept(reason.into()),
            None => remove_if_merged(&record.repo, path)?,
        };
        out.push(match removal {
            Removal::Removed => format!("removed {}", path.display()),
            Removal::Kept(reason) => format!("kept {}: {reason}", path.display()),
        });
    }
    Ok(())
}

/// The command that closes `record`'s tab, commented with why it may still
/// be open, found as `retire_and_close` finds it. `None` once herdr shows
/// neither its agent nor its pane, or shows its pane as provably another's.
/// `others` are the other ledger entries.
pub fn open_tab(
    herdr: &dyn Herdr,
    record: &AgentRecord,
    others: &BTreeMap<String, AgentRecord>,
) -> Result<Option<String>> {
    let name = &record.name;
    // Every tab but `Ours` is one `retire --close` keeps, so it is closed by hand.
    let close = |tab_id: &str, why: String, unless: Option<String>| {
        let unless = unless.map(|u| format!("; if {u}")).unwrap_or_default();
        Some(format!("herdr tab close {tab_id}  # {name}: {why}{unless}"))
    };
    let relabel = |tab_id: &str| {
        Some(format!(
            "that tab isn't {name}'s, label it otherwise: herdr tab rename {tab_id} <label>"
        ))
    };
    Ok(match agent_tab(herdr, record, others, None)? {
        TabFate::Gone | TabFate::Taken { .. } | TabFate::NoWorkspace { tab: None, .. } => None,
        TabFate::Ours(tab_id) => Some(format!(
            "loopherd retire --close {name}  # tab {tab_id} is open"
        )),
        TabFate::Kept { tab_id, reason } => {
            close(&tab_id, format!("tab {tab_id} is open: {reason}"), None)
        }
        TabFate::Busy(tab_id) => close(
            &tab_id,
            format!("tab {tab_id} is open: {BUSY}"),
            relabel(&tab_id),
        ),
        // Another session's agent most likely isn't the member, so unnaming it comes first.
        TabFate::NameHeld(tab_id) => Some(format!(
            "herdr agent rename {name} --clear  # {name}: tab {tab_id} is open: {NAME_HELD}; \
             if that agent is {name}, close its tab instead: herdr tab close {tab_id}"
        )),
        TabFate::Unidentified { tab_id, pane_id } => close(
            &tab_id,
            format!("tab {tab_id} is open: {UNIDENTIFIED}"),
            Some(format!(
                "that agent isn't {name}'s, name it: herdr agent rename {pane_id} <label>"
            )),
        ),
        TabFate::Unmatched { line, tab_id } => close(
            &tab_id,
            format!("not confirmed closed: {line}"),
            relabel(&tab_id),
        ),
        TabFate::NoWorkspace {
            line,
            tab: Some((label, tab_id)),
        } => close(
            &tab_id,
            format!(
                "not confirmed closed: {line}, but pane {} is in workspace {label}",
                record.pane_id
            ),
            relabel(&tab_id),
        ),
    })
}

const BUSY: &str = "pane is running a command";
const NAME_HELD: &str = "the name is held by another session";
const UNIDENTIFIED: &str = "its pane runs an agent herdr can't identify";

/// An active entry other than `name` whose worktree is `path`.
fn other_user(ledger: &dyn Ledger, name: &str, path: &Path) -> Result<Option<String>> {
    Ok(ledger
        .entries()?
        .into_values()
        .find(|r| {
            !r.retired && r.name != name && r.worktree.as_deref().is_some_and(|w| same_dir(w, path))
        })
        .map(|r| r.name))
}

enum TabFate {
    Gone,
    Ours(String),
    Kept {
        tab_id: String,
        reason: String,
    },
    /// The recorded pane is open but another entry's or agent's now.
    Taken {
        tab_id: String,
        reason: String,
    },
    /// The recorded pane is open with no agent, running a command.
    Busy(String),
    /// An agent from another session goes by the entry's name.
    NameHeld(String),
    /// The recorded pane runs an agent with nothing to tell it from the
    /// entry's own.
    Unidentified {
        tab_id: String,
        pane_id: String,
    },
    /// The recorded pane is open but no tab matched, and it may still be the
    /// entry's; holds the line to print.
    Unmatched {
        line: String,
        tab_id: String,
    },
    /// The entry's workspace isn't found; holds the line to print and the
    /// label and tab of the workspace that may hold its pane.
    NoWorkspace {
        line: String,
        tab: Option<(String, String)>,
    },
}

/// Finds the entry's tab through its live agent, else its recorded pane, and
/// checks its workspace label and cwd as revive does (D8). A recorded pane
/// that `others` or another agent now holds is kept.
fn agent_tab(
    herdr: &dyn Herdr,
    record: &AgentRecord,
    others: &BTreeMap<String, AgentRecord>,
    own_pane: Option<&str>,
) -> Result<TabFate> {
    let agents = herdr.agents()?;
    let live = by_name(record, &agents);
    let kept = |tab_id: &str, reason: String| {
        Ok(TabFate::Kept {
            tab_id: tab_id.to_string(),
            reason,
        })
    };
    if let Some(live) = live
        && session_conflict(live, record)
    {
        return Ok(TabFate::NameHeld(live.tab_id.clone()));
    }
    let pane_id = live.map_or(&record.pane_id, |a| &a.pane_id);
    let workspace = match &record.workspace_label {
        Some(label) => find_workspace(herdr, |w| &w.label == label)?,
        None => find_workspace(herdr, |w| w.id == record.workspace)?,
    };
    let shown = record.workspace_label.as_ref().unwrap_or(&record.workspace);
    let panes = match (&workspace, live) {
        (Some(ws), _) => herdr.panes_in(&ws.id)?,
        (None, Some(_)) => vec![],
        (None, None) => {
            let line = format!("no matching tab: workspace {shown} not found");
            let tab = match pane_elsewhere(herdr, record, &agents, others)? {
                Elsewhere::Nowhere => None,
                Elsewhere::NotIts { tab_id, reason } => {
                    return Ok(TabFate::Taken { tab_id, reason });
                }
                Elsewhere::Unidentified { tab_id } => {
                    let pane_id = record.pane_id.clone();
                    return Ok(TabFate::Unidentified { tab_id, pane_id });
                }
                Elsewhere::Maybe { label, tab_id } => Some((label, tab_id)),
            };
            return Ok(TabFate::NoWorkspace { line, tab });
        }
    };
    let pane = panes.iter().find(|p| &p.id == pane_id).cloned();
    if let Some(pane) = &pane
        && panes
            .iter()
            .any(|p| Some(p.id.as_str()) == own_pane && p.tab_id == pane.tab_id)
    {
        return kept(&pane.tab_id, "it is running this command".into());
    }
    if live.is_none()
        && let Some(pane) = &pane
    {
        let tab_id = pane.tab_id.clone();
        let ws_id = &workspace.as_ref().expect("panes came from it").id;
        match pane_use(herdr, record, ws_id, pane, &agents, others)? {
            PaneUse::Free => {}
            PaneUse::Held(reason) => return Ok(TabFate::Taken { tab_id, reason }),
            PaneUse::Busy => return Ok(TabFate::Busy(tab_id)),
            PaneUse::Unidentified => {
                let pane_id = pane.id.clone();
                return Ok(TabFate::Unidentified { tab_id, pane_id });
            }
        }
    }
    let cwd = record.worktree.as_ref().unwrap_or(&record.repo);
    match (pane, live) {
        (None, None) => Ok(match pane_elsewhere(herdr, record, &agents, others)? {
            Elsewhere::Nowhere => TabFate::Gone,
            Elsewhere::NotIts { tab_id, reason } => TabFate::Taken { tab_id, reason },
            Elsewhere::Unidentified { tab_id } => TabFate::Unidentified {
                tab_id,
                pane_id: record.pane_id.clone(),
            },
            Elsewhere::Maybe { label, tab_id } => TabFate::Unmatched {
                line: format!("no matching tab (pane {pane_id} is in workspace {label})"),
                tab_id,
            },
        }),
        (None, Some(live)) => kept(&live.tab_id, format!("not in workspace {shown}")),
        (Some(pane), _) if pane.is_in(cwd) => {
            // Closing a tab closes every pane in it, not just the agent's.
            match panes.iter().filter(|p| p.tab_id == pane.tab_id).count() {
                1 => Ok(TabFate::Ours(pane.tab_id)),
                n => kept(&pane.tab_id, format!("it has {n} panes")),
            }
        }
        (Some(pane), None) => Ok(TabFate::Unmatched {
            line: format!(
                "no matching tab (pane {} is in {})",
                pane.id,
                shown_cwd(&pane)
            ),
            tab_id: pane.tab_id,
        }),
        (Some(pane), Some(_)) => {
            let actual = shown_cwd(&pane);
            kept(
                &pane.tab_id,
                format!("its cwd is {actual}, not {}", cwd.display()),
            )
        }
    }
}

/// Where `record`'s pane is when it isn't in the entry's workspace.
enum Elsewhere {
    Nowhere,
    NotIts { tab_id: String, reason: String },
    Unidentified { tab_id: String },
    Maybe { label: String, tab_id: String },
}

fn pane_elsewhere(
    herdr: &dyn Herdr,
    record: &AgentRecord,
    agents: &[Agent],
    others: &BTreeMap<String, AgentRecord>,
) -> Result<Elsewhere> {
    for ws in herdr.workspaces()? {
        let panes = herdr.panes_in(&ws.id)?;
        let Some(pane) = panes.iter().find(|p| p.id == record.pane_id) else {
            continue;
        };
        let tab_id = pane.tab_id.clone();
        return Ok(
            match pane_use(herdr, record, &ws.id, pane, agents, others)? {
                PaneUse::Held(reason) => Elsewhere::NotIts { tab_id, reason },
                PaneUse::Unidentified => Elsewhere::Unidentified { tab_id },
                PaneUse::Free | PaneUse::Busy => Elsewhere::Maybe {
                    label: ws.label,
                    tab_id,
                },
            },
        );
    }
    Ok(Elsewhere::Nowhere)
}

fn shown_cwd(pane: &Pane) -> String {
    pane.cwd
        .as_ref()
        .map_or("unknown".into(), |c| c.display().to_string())
}

/// Whether the recorded pane may be someone else's after a restart reused
/// its id.
enum PaneUse {
    /// Safe to treat as the entry's.
    Free,
    /// Another entry recorded it or its session, it runs an agent with another
    /// name or session, or its tab isn't labeled with the entry's name as spawn and
    /// revive label it.
    Held(String),
    /// It runs an agent with no name and no session to tell it from the
    /// entry's own.
    Unidentified,
    /// Its shell is running a command.
    Busy,
}

/// Checks `pane` in workspace `ws_id`. The entry's own session vetoes the
/// label check, so a renamed tab it still runs in stays its.
fn pane_use(
    herdr: &dyn Herdr,
    record: &AgentRecord,
    ws_id: &str,
    pane: &Pane,
    agents: &[Agent],
    others: &BTreeMap<String, AgentRecord>,
) -> Result<PaneUse> {
    let pane_id = pane.id.as_str();
    if let Some(other) = member_in(others, pane_id, herdr.socket().as_deref()) {
        return Ok(PaneUse::Held(format!("pane belongs to {}", other.name)));
    }
    if let Some(agent) = agents.iter().find(|a| a.pane_id == pane_id) {
        let session_of =
            |e: &&AgentRecord| e.session_id.is_some() && e.session_id == agent.session_id;
        return Ok(match &agent.name {
            _ if same_session(agent, record) => PaneUse::Free,
            _ if let Some(other) = others.values().find(session_of) => {
                PaneUse::Held(format!("pane hosts {}'s session", other.name))
            }
            Some(name) => PaneUse::Held(format!("pane hosts agent {name}")),
            None if session_conflict(agent, record) => {
                PaneUse::Held("pane hosts another agent".into())
            }
            None => PaneUse::Unidentified,
        });
    }
    let tabs = herdr.tabs_in(ws_id)?;
    if let Some(tab) = tabs.iter().find(|t| t.id == pane.tab_id)
        && tab.label != record.name
    {
        return Ok(PaneUse::Held(format!(
            "it is labeled {}, not {}",
            tab.label, record.name
        )));
    }
    Ok(match herdr.at_shell_prompt(pane_id)? {
        true => PaneUse::Free,
        false => PaneUse::Busy,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::herdr::fake::{FAKE_SOCKET, FakeHerdr};
    use crate::ledger::MemLedger;
    use crate::process::{CLAUDE, FakeProcs};
    use crate::spawn::{SpawnRequest, spawn};
    use crate::worktree::testing::{
        commit, current_branch, git_in, has_branch, head, head_of, merge_on_origin,
        move_origin_default, push_head, repo_with_origin, worktree_list,
    };

    fn record(name: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            tab_id: None,
            session_id: Some("abc".into()),
            repo: "/src/repo".into(),
            worktree: None,
            branch: None,
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            released_at: None,
            herd: None,
            template: None,
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
            earlier: vec![],
        }
    }

    fn ledger_with(names: &[&str]) -> MemLedger {
        MemLedger(
            names.iter().map(|n| (n.to_string(), record(n))).collect(),
            vec![],
        )
    }

    fn halloween_noon() -> SystemTime {
        humantime::parse_rfc3339("2026-10-31T12:00:00Z").unwrap()
    }

    fn lead_running_as(pid: u32, start: u64) -> MemLedger {
        let mut ledger = ledger_with(&["lead-k"]);
        let lead = ledger.0.get_mut("lead-k").unwrap();
        lead.role = Some(crate::ledger::Role::Lead);
        lead.socket = Some(FAKE_SOCKET.into());
        lead.process = Some(crate::ledger::Process { pid, start });
        ledger
    }

    /// The pane's Claude 30 → shell 35 → loopherd 40.
    fn loopherd_under_the_pane_claude() -> FakeProcs {
        let mut procs = FakeProcs::pane();
        procs.add(35, "/bin/bash", 30, 350);
        procs.add(40, "/usr/bin/loopherd", 35, 400);
        procs
    }

    const LOOPHERD: u32 = 40;

    #[test]
    fn a_lead_retiring_itself_with_the_pane_env_unset_is_refused() {
        let ledger = lead_running_as(30, 300);

        let err = refuse_own_lead(
            &ledger,
            "lead-k",
            None,
            &loopherd_under_the_pane_claude(),
            LOOPHERD,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("lead-k is the lead running this loopherd"));
    }

    fn another_pane() -> OwnPane<'static> {
        OwnPane {
            id: "w9:p9",
            socket: Some(Path::new(FAKE_SOCKET)),
        }
    }

    #[test]
    fn a_lead_retiring_itself_with_the_pane_env_naming_another_pane_is_refused() {
        let ledger = lead_running_as(30, 300);

        let err = refuse_own_lead(
            &ledger,
            "lead-k",
            Some(&another_pane()),
            &loopherd_under_the_pane_claude(),
            LOOPHERD,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("lead-k is the lead running this loopherd"));
    }

    /// The shepherd's Claude 50 → shell 55 → loopherd 60, beside the lead's.
    fn loopherd_under_the_shepherd() -> FakeProcs {
        let mut procs = FakeProcs::pane();
        procs.add(50, CLAUDE, 20, 500);
        procs.add(55, "/bin/bash", 50, 550);
        procs.add(60, "/usr/bin/loopherd", 55, 600);
        procs
    }

    fn the_lead_pane() -> OwnPane<'static> {
        OwnPane {
            id: "w1:p1",
            socket: Some(Path::new(FAKE_SOCKET)),
        }
    }

    #[test]
    fn the_shepherd_may_retire_a_live_lead_it_does_not_run_under() {
        let ledger = lead_running_as(30, 300);

        let shepherd = refuse_own_lead(
            &ledger,
            "lead-k",
            Some(&another_pane()),
            &loopherd_under_the_shepherd(),
            60,
        );

        assert!(shepherd.is_ok());
    }

    #[test]
    fn a_reused_pid_with_another_start_time_leaves_only_the_pane_check() {
        let ledger = lead_running_as(30, 299);

        let procs = loopherd_under_the_pane_claude();

        let reused = refuse_own_lead(&ledger, "lead-k", None, &procs, LOOPHERD);
        let err = refuse_own_lead(&ledger, "lead-k", Some(&the_lead_pane()), &procs, LOOPHERD)
            .unwrap_err();

        assert!(reused.is_ok());
        assert!(format!("{err:#}").contains("lead-k is the lead in this pane"));
    }

    #[test]
    fn a_lead_with_no_recorded_process_is_refused_only_by_its_pane() {
        let mut ledger = lead_running_as(30, 300);
        ledger.0.get_mut("lead-k").unwrap().process = None;
        let procs = loopherd_under_the_pane_claude();

        let err = refuse_own_lead(&ledger, "lead-k", Some(&the_lead_pane()), &procs, LOOPHERD)
            .unwrap_err();
        let env_unset = refuse_own_lead(&ledger, "lead-k", None, &procs, LOOPHERD);
        let other_pane =
            refuse_own_lead(&ledger, "lead-k", Some(&another_pane()), &procs, LOOPHERD);

        assert!(format!("{err:#}").contains("lead-k is the lead in this pane"));
        assert!(env_unset.is_ok());
        assert!(other_pane.is_ok());
    }

    #[test]
    fn a_live_lead_detached_from_loopherd_is_still_refused_by_its_pane() {
        let ledger = lead_running_as(30, 300);

        let err = refuse_own_lead(
            &ledger,
            "lead-k",
            Some(&the_lead_pane()),
            &loopherd_under_the_shepherd(),
            60,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("lead-k is the lead in this pane"));
    }

    #[test]
    fn a_worker_running_loopherd_may_be_retired_by_it() {
        let mut ledger = lead_running_as(30, 300);
        ledger.0.get_mut("lead-k").unwrap().role = None;

        let worker = refuse_own_lead(
            &ledger,
            "lead-k",
            Some(&the_lead_pane()),
            &loopherd_under_the_pane_claude(),
            LOOPHERD,
        );

        assert!(worker.is_ok());
    }

    #[test]
    fn marks_the_entry_retired_with_the_time() {
        let mut ledger = ledger_with(&["a", "b"]);
        retire(&mut ledger, "a", halloween_noon()).unwrap();
        let a = &ledger.0["a"];
        assert!(a.retired);
        assert_eq!(a.retired_at.as_deref(), Some("2026-10-31T12:00:00Z"));
        assert_eq!(ledger.0["b"], record("b"));
    }

    #[test]
    fn an_unknown_name_fails_and_leaves_the_ledger_untouched() {
        let mut ledger = ledger_with(&["a"]);
        let err = retire(&mut ledger, "ghost", halloween_noon()).unwrap_err();
        assert_eq!(format!("{err:#}"), "ghost is not in the ledger");
        assert_eq!(ledger.0, ledger_with(&["a"]).0);
    }

    #[test]
    fn an_already_retired_name_fails_and_keeps_its_first_retirement() {
        let mut ledger = ledger_with(&["a"]);
        retire(&mut ledger, "a", halloween_noon()).unwrap();
        let before = ledger.0.clone();
        let later = halloween_noon() + Duration::from_secs(3600);
        let err = retire(&mut ledger, "a", later).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "a is already retired (since 2026-10-31T12:00:00Z)"
        );
        assert_eq!(ledger.0, before);
    }

    #[test]
    fn retiring_a_spawned_agent_leaves_its_tab_agent_and_worktree_alone() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            repo,
            name: "a".into(),
            worktree: Some("feature/existing".into()),
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            shell_timeout: crate::spawn::SHELL_TIMEOUT,
            claude_args: vec![],
            member_of: None,
        };
        spawn(&herdr, &mut ledger, &req, Some("w1")).unwrap();
        let (tabs, agents) = (herdr.tabs(), herdr.agents().unwrap());

        retire(&mut ledger, "a", halloween_noon()).unwrap();

        assert!(ledger.0["a"].retired);
        assert_eq!(herdr.tabs(), tabs);
        assert_eq!(herdr.agents().unwrap(), agents);
        let wt = root.path().join("proj/worktrees/feature-existing");
        assert_eq!(current_branch(&wt), "feature/existing");
    }

    fn spawned_in_worktree(root: &Path, branch: &str) -> (FakeHerdr, MemLedger, PathBuf) {
        let repo = repo_with_origin(root);
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            repo: repo.clone(),
            name: "a".into(),
            worktree: Some(branch.into()),
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            shell_timeout: crate::spawn::SHELL_TIMEOUT,
            claude_args: vec![],
            member_of: None,
        };
        spawn(&herdr, &mut ledger, &req, Some("w1")).unwrap();
        (herdr, ledger, repo)
    }

    fn close(herdr: &FakeHerdr, ledger: &mut MemLedger) -> Vec<String> {
        let mut out = vec![];
        retire_and_close(
            herdr,
            ledger,
            "a",
            halloween_noon(),
            Path::new("/nonexistent/reports"),
            None,
            &mut out,
        )
        .unwrap();
        out
    }

    #[test]
    fn close_closes_the_tab_and_removes_a_clean_merged_worktree_and_its_branch() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "closed tab w1:t1".into(),
                format!("removed {}", wt.display()),
            ]
        );
        assert!(ledger.0["a"].retired);
        assert!(herdr.tabs().is_empty());
        assert!(!wt.exists());
        assert!(!has_branch(&repo, "feat/new"));
        assert_eq!(current_branch(&repo), "main");
    }

    #[test]
    fn close_removes_a_worktree_merged_on_origin_while_the_repo_checkout_is_behind() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        commit(&wt, "work");
        push_head(&wt, "feat/new");
        merge_on_origin(root.path(), "feat/new");
        assert_ne!(head_of(&repo, "origin/main"), head(&wt));

        let out = close(&herdr, &mut ledger);

        assert_eq!(out.last().unwrap(), &format!("removed {}", wt.display()));
        assert!(!wt.exists());
        assert!(!has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_judges_merged_against_origins_current_default_not_a_stale_origin_head() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        move_origin_default(root.path(), "staging");
        commit(&wt, "work");
        push_head(&wt, "main");

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "feat/new is not merged into origin/staging");
    }

    fn kept_line(out: &[String], wt: &Path, reason: &str) {
        assert_eq!(
            out.last().unwrap(),
            &format!("kept {}: {reason}", wt.display())
        );
        assert!(wt.is_dir());
    }

    #[test]
    fn close_keeps_an_unmerged_worktree_and_its_branch() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        commit(&wt, "unpushed");

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "feat/new is not merged into origin/main");
        assert!(has_branch(&repo, "feat/new"));
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn close_keeps_a_worktree_with_uncommitted_changes() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        std::fs::write(wt.join("f"), "x").unwrap();
        git_in(&wt, &["add", "f"]);

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "uncommitted or untracked changes");
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_a_worktree_with_untracked_files() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        std::fs::write(wt.join("notes.txt"), "x").unwrap();

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "uncommitted or untracked changes");
        assert!(wt.join("notes.txt").exists());
    }

    #[test]
    fn close_removes_a_clean_detached_worktree_even_when_unmerged() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        commit(&wt, "unpushed");
        git_in(&wt, &["checkout", "--quiet", "--detach"]);

        let out = close(&herdr, &mut ledger);

        assert_eq!(out.last().unwrap(), &format!("removed {}", wt.display()));
        assert!(!wt.exists());
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_a_worktree_stopped_mid_rebase() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        commit(&wt, "one");
        commit(&wt, "two");
        git_in(
            &wt,
            &[
                "-c",
                "sequence.editor=sed -i 1s/^pick/edit/",
                "rebase",
                "--quiet",
                "-i",
                "HEAD~2",
            ],
        );

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "rebase in progress");
    }

    #[test]
    fn close_keeps_a_worktree_mid_bisect() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        for message in ["one", "two", "three"] {
            commit(&wt, message);
        }
        git_in(&wt, &["bisect", "start", "HEAD", "HEAD~3"]);

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "bisect in progress");
    }

    #[test]
    fn close_keeps_a_worktree_with_only_a_sequencer_left() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&wt)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap()
        };
        let commit_file = |name: &str, body: &str| {
            std::fs::write(wt.join(name), body).unwrap();
            git(&["add", name]);
            git(&["commit", "--quiet", "-m", name]);
        };
        git(&["checkout", "--quiet", "-b", "side"]);
        commit_file("f", "side");
        commit_file("g", "side");
        git(&["checkout", "--quiet", "--detach", "feat/new"]);
        commit_file("f", "here");
        let picked = git(&["cherry-pick", "side~1", "side"]);
        assert!(!picked.status.success(), "the first pick should conflict");
        git(&["reset", "--quiet", "--merge"]);
        let git_dir = repo.join(".git/worktrees/feat-new");
        assert!(git_dir.join("sequencer").is_dir());
        assert!(!git_dir.join("CHERRY_PICK_HEAD").exists());

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "cherry-pick or revert in progress");
    }

    /// Runs git in `wt` as a committer, returning the output whether or not it failed.
    fn try_git(wt: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap()
    }

    fn commit_file(wt: &Path, name: &str, body: &str) {
        std::fs::write(wt.join(name), body).unwrap();
        try_git(wt, &["add", name]);
        try_git(wt, &["commit", "--quiet", "-m", body]);
    }

    /// Leaves `wt` on its branch with `f` committed as "here", and a `side`
    /// branch whose one commit adds `f` as "side".
    fn diverge(wt: &Path) {
        try_git(wt, &["checkout", "--quiet", "-b", "side"]);
        commit_file(wt, "f", "side");
        try_git(wt, &["checkout", "--quiet", "-"]);
        commit_file(wt, "f", "here");
    }

    fn assert_conflicts(wt: &Path, args: &[&str]) {
        let out = try_git(wt, args);
        assert!(!out.status.success(), "git {args:?} should conflict");
    }

    #[test]
    fn close_reports_a_conflicted_merge_rather_than_its_uncommitted_changes() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        diverge(&wt);
        assert_conflicts(&wt, &["merge", "--quiet", "side"]);

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "merge in progress");
    }

    #[test]
    fn close_keeps_a_worktree_with_a_conflicted_cherry_pick() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        diverge(&wt);
        assert_conflicts(&wt, &["cherry-pick", "side"]);

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "cherry-pick in progress");
    }

    #[test]
    fn close_keeps_a_worktree_with_a_conflicted_revert() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        commit_file(&wt, "f", "one");
        commit_file(&wt, "f", "two");
        assert_conflicts(&wt, &["revert", "--no-edit", "HEAD~1"]);

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "revert in progress");
    }

    #[test]
    fn close_names_a_conflicted_am_as_am_not_rebase() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        diverge(&wt);
        let patch = root.path().join("side.patch");
        let formatted = try_git(&wt, &["format-patch", "-1", "--stdout", "side"]);
        std::fs::write(&patch, formatted.stdout).unwrap();
        assert_conflicts(&wt, &["am", "--quiet", patch.to_str().unwrap()]);

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "am in progress");
    }

    #[test]
    fn close_keeps_a_worktree_with_a_conflicted_apply_rebase() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        diverge(&wt);
        assert_conflicts(&wt, &["rebase", "--quiet", "--apply", "side"]);

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "rebase in progress");
    }

    #[test]
    fn close_never_removes_the_repo_checkout_even_if_the_ledger_names_it() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        herdr.close_tab("w1:t1").unwrap();
        ledger.0.get_mut("a").unwrap().worktree = Some(repo.clone());

        let out = close(&herdr, &mut ledger);

        kept_line(
            &out,
            &repo,
            &format!("not a linked worktree of {}", repo.display()),
        );
        assert_eq!(current_branch(&repo), "main");
    }

    #[test]
    fn close_without_a_worktree_leaves_the_repo_alone() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let herdr = FakeHerdr::new(&[("w1", "loopherd")]);
        let mut ledger = MemLedger::default();
        let req = SpawnRequest {
            repo: repo.clone(),
            name: "a".into(),
            worktree: None,
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            shell_timeout: crate::spawn::SHELL_TIMEOUT,
            claude_args: vec![],
            member_of: None,
        };
        spawn(&herdr, &mut ledger, &req, Some("w1")).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(out, ["retired a", "closed tab w1:t1"]);
        assert!(repo.join(".git").is_dir());
    }

    #[test]
    fn close_with_the_tab_already_gone_still_removes_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        herdr.close_tab("w1:t1").unwrap();
        let wt = root.path().join("proj/worktrees/feat-new");

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "tab already gone".into(),
                format!("removed {}", wt.display()),
            ]
        );
        assert!(!wt.exists());
    }

    #[test]
    fn close_keeps_a_worktree_and_branch_another_active_entry_uses() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        retire(&mut ledger, "a", halloween_noon()).unwrap();
        let req = SpawnRequest {
            repo: repo.clone(),
            name: "b".into(),
            worktree: Some("feat/new".into()),
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            shell_timeout: crate::spawn::SHELL_TIMEOUT,
            claude_args: vec![],
            member_of: None,
        };
        spawn(&herdr, &mut ledger, &req, Some("w1")).unwrap();
        let wt = root.path().join("proj/worktrees/feat-new");
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path().join("proj/worktrees"), &alias).unwrap();
        ledger.0.get_mut("b").unwrap().worktree = Some(alias.join("feat-new"));

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "a is already retired (since 2026-10-31T12:00:00Z)".to_string(),
                "closed tab w1:t1".into(),
                format!("kept {}: in use by b", wt.display()),
            ]
        );
        assert_eq!(current_branch(&wt), "feat/new");
        assert!(has_branch(&repo, "feat/new"));
    }

    /// Spawns `b` onto `a`'s worktree after retiring `a`; returns the repo and
    /// the worktree's real path.
    fn shared_with_b(root: &Path) -> (FakeHerdr, MemLedger, PathBuf, PathBuf) {
        let (herdr, mut ledger, repo) = spawned_in_worktree(root, "feat/new");
        retire(&mut ledger, "a", halloween_noon()).unwrap();
        let req = SpawnRequest {
            repo: repo.clone(),
            name: "b".into(),
            worktree: Some("feat/new".into()),
            workspace: None,
            prompt: None,
            ready_timeout: Duration::from_secs(60),
            shell_timeout: crate::spawn::SHELL_TIMEOUT,
            claude_args: vec![],
            member_of: None,
        };
        spawn(&herdr, &mut ledger, &req, Some("w1")).unwrap();
        (herdr, ledger, repo, root.join("proj/worktrees/feat-new"))
    }

    #[test]
    fn close_keeps_a_missing_shared_worktree_recorded_under_two_spellings() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo, wt) = shared_with_b(root.path());
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path().join("proj/worktrees"), &alias).unwrap();
        let aliased = alias.join("feat-new");
        ledger.0.get_mut("a").unwrap().worktree = Some(aliased.clone());
        std::fs::remove_dir_all(&wt).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out.last().unwrap(),
            &format!("kept {}: in use by b", aliased.display())
        );
        assert!(worktree_list(&repo).contains("feat-new"));
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_a_worktree_shared_with_an_entry_recorded_under_another_repo() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo, wt) = shared_with_b(root.path());
        ledger.0.get_mut("b").unwrap().repo = root.path().join("elsewhere");
        std::fs::remove_dir_all(&wt).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out.last().unwrap(),
            &format!("kept {}: in use by b", wt.display())
        );
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_does_not_count_the_closed_entry_as_a_sharer_of_its_missing_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path().join("proj/worktrees"), &alias).unwrap();
        let aliased = alias.join("feat-new");
        ledger.0.get_mut("a").unwrap().worktree = Some(aliased.clone());
        std::fs::remove_dir_all(root.path().join("proj/worktrees/feat-new")).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out.last().unwrap(),
            &format!("removed {} (already gone)", aliased.display())
        );
        assert!(!worktree_list(&repo).contains("feat-new"));
        assert!(!has_branch(&repo, "feat/new"));
    }

    /// An entry with no worktree whose cwd is a fresh directory.
    fn plain(cwd: &Path, pane_id: &str) -> MemLedger {
        let mut ledger = ledger_with(&["a"]);
        let a = ledger.0.get_mut("a").unwrap();
        a.repo = cwd.into();
        a.pane_id = pane_id.into();
        a.session_id = Some("sess-a".into());
        ledger
    }

    fn two_workspaces() -> FakeHerdr {
        FakeHerdr::new(&[("w1", "loopherd"), ("w2", "my-app")])
    }

    /// Retires and closes `a`, whose tab is open in `cwd`, with `mailboxes`.
    fn close_with_mailboxes(cwd: &Path, mailboxes: &Path) -> (MemLedger, Vec<String>) {
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "a", cwd, true);
        let mut ledger = plain(cwd, &pane);
        let mut out = vec![];
        retire_and_close(
            &herdr,
            &mut ledger,
            "a",
            halloween_noon(),
            mailboxes,
            None,
            &mut out,
        )
        .unwrap();
        (ledger, out)
    }

    fn write_marks(mailboxes: &Path, member: &str) {
        std::fs::create_dir_all(mailboxes).unwrap();
        std::fs::write(
            mailboxes.join(format!("{member}.notified")),
            "question t1\n",
        )
        .unwrap();
        std::fs::write(mailboxes.join(format!("{member}.blocked")), "").unwrap();
        std::fs::write(mailboxes.join(format!("{member}.block-recorded")), "").unwrap();
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn close_removes_the_members_notify_marks_and_leaves_other_files() {
        let cwd = tempfile::tempdir().unwrap();
        let mailboxes = cwd.path().join("reports");
        write_marks(&mailboxes, "a");
        write_marks(&mailboxes, "b");
        std::fs::write(mailboxes.join("a.jsonl"), "{}\n").unwrap();

        let (ledger, out) = close_with_mailboxes(cwd.path(), &mailboxes);

        assert_eq!(out, ["retired a", "closed tab w1:t1"]);
        assert!(ledger.0["a"].retired);
        assert_eq!(
            files_in(&mailboxes),
            ["a.jsonl", "b.block-recorded", "b.blocked", "b.notified"]
        );
    }

    #[test]
    fn close_without_notify_marks_retires_quietly() {
        let cwd = tempfile::tempdir().unwrap();
        let mailboxes = cwd.path().join("reports");

        let (ledger, out) = close_with_mailboxes(cwd.path(), &mailboxes);

        assert_eq!(out, ["retired a", "closed tab w1:t1"]);
        assert!(ledger.0["a"].retired);
        assert!(!mailboxes.exists());
    }

    #[test]
    fn a_mark_that_cannot_be_removed_is_a_warning_and_the_close_goes_on() {
        let cwd = tempfile::tempdir().unwrap();
        let mailboxes = cwd.path().join("reports");
        let stuck = mailboxes.join("a.notified");
        std::fs::create_dir_all(stuck.join("inside")).unwrap();
        std::fs::write(mailboxes.join("a.blocked"), "").unwrap();

        let (ledger, out) = close_with_mailboxes(cwd.path(), &mailboxes);

        assert_eq!(out.len(), 3, "{out:?}");
        assert!(
            out[1].starts_with(&format!("warning: remove {}: ", stuck.display())),
            "{out:?}"
        );
        assert_eq!(out[2], "closed tab w1:t1");
        assert!(ledger.0["a"].retired);
        assert!(!mailboxes.join("a.blocked").exists());
    }

    #[test]
    fn close_finds_the_tab_through_the_recorded_pane_when_the_agent_has_exited() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "a", cwd.path(), true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(
            close(&herdr, &mut ledger),
            ["retired a", "closed tab w1:t1"]
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn close_treats_a_pane_whose_cwd_is_a_symlink_to_the_entrys_dir_as_its_tab() {
        let cwd = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("link");
        std::os::unix::fs::symlink(cwd.path(), &link).unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "a", &link, true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(
            close(&herdr, &mut ledger),
            ["retired a", "closed tab w1:t1"]
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn close_leaves_a_reused_pane_id_in_another_tab_alone() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "other", elsewhere.path(), true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(
            close(&herdr, &mut ledger),
            ["retired a", "kept tab w1:t1: it is labeled other, not a"]
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn close_reports_no_matching_tab_for_a_recorded_pane_in_another_workspace() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "my-app"), ("w2", "loopherd")]);
        let pane = herdr.add_tab("w1", "a", cwd.path(), true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(
            close(&herdr, &mut ledger),
            [
                "retired a".to_string(),
                format!("no matching tab (pane {pane} is in workspace my-app)"),
            ]
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn close_keeps_the_worktree_when_the_exited_agents_shell_moved_into_a_subdirectory() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        let src = wt.join("src");
        std::fs::create_dir(&src).unwrap();
        let pane = restored_after_exit(&herdr, &mut ledger, &src);

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                format!("no matching tab (pane {pane} is in {})", src.display()),
                format!("kept {}: tab not confirmed closed", wt.display()),
            ]
        );
        assert!(wt.is_dir());
    }

    #[test]
    fn close_keeps_a_live_agents_tab_outside_the_recorded_workspace() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w2", "a", cwd.path(), false);
        let herdr = herdr.with_live_agent(Some("a"), Some("sess-a"), &pane);
        let mut ledger = plain(cwd.path(), "w1:p7");

        assert_eq!(
            close(&herdr, &mut ledger),
            ["retired a", "kept tab w2:t1: not in workspace loopherd"]
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn close_keeps_a_live_agents_tab_in_another_cwd_and_its_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        let moved = root.path().join("proj/proj");
        let pane = herdr.add_tab("w1", "a2", &moved, false);
        let herdr = herdr.with_live_agent(Some("a"), None, &pane);
        herdr.close_tab("w1:t1").unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                format!(
                    "kept tab w1:t2: its cwd is {}, not {}",
                    moved.display(),
                    wt.display()
                ),
                format!("kept {}: its tab was kept", wt.display()),
            ]
        );
        assert!(wt.is_dir());
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn close_keeps_a_tab_whose_name_is_held_by_another_session() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "a", cwd.path(), false);
        let herdr = herdr.with_live_agent(Some("a"), Some("someone-else"), &pane);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(
            close(&herdr, &mut ledger),
            [
                "retired a",
                "kept tab w1:t1: the name is held by another session"
            ]
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    /// Replaces the spawned tab with one whose agent has exited, its shell in
    /// `cwd`, and records that pane. Returns the pane ID.
    fn restored_after_exit(herdr: &FakeHerdr, ledger: &mut MemLedger, cwd: &Path) -> String {
        herdr.close_tab("w1:t1").unwrap();
        let pane = herdr.add_tab("w1", "a", cwd, true);
        ledger.0.get_mut("a").unwrap().pane_id = pane.clone();
        pane
    }

    #[test]
    fn close_with_the_workspace_gone_reports_no_matching_tab() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "renamed")]);
        let pane = herdr.add_tab("w1", "a", cwd.path(), true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(
            close(&herdr, &mut ledger),
            ["retired a", "no matching tab: workspace loopherd not found"]
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn close_with_the_workspace_gone_keeps_a_clean_merged_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        restored_after_exit(&herdr, &mut ledger, &wt);
        ledger.0.get_mut("a").unwrap().workspace_label = Some("renamed".into());

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "no matching tab: workspace renamed not found".into(),
                format!("kept {}: tab not confirmed closed", wt.display()),
            ]
        );
        assert!(wt.is_dir());
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn a_failed_tab_close_is_an_error_after_retiring_and_keeps_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (mut herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        herdr.fail_close_tab = true;
        let mut out = vec![];

        let err = retire_and_close(
            &herdr,
            &mut ledger,
            "a",
            halloween_noon(),
            Path::new("/nonexistent/reports"),
            None,
            &mut out,
        )
        .unwrap_err();

        assert_eq!(format!("{err:#}"), "tab close refused");
        assert_eq!(out, ["retired a"]);
        assert!(ledger.0["a"].retired);
        assert!(root.path().join("proj/worktrees/feat-new").is_dir());
    }

    #[test]
    fn close_retried_after_a_failed_tab_close_finishes_and_keeps_the_first_retire_time() {
        let root = tempfile::tempdir().unwrap();
        let (mut herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        herdr.fail_close_tab = true;
        retire_and_close(
            &herdr,
            &mut ledger,
            "a",
            halloween_noon(),
            Path::new("/nonexistent/reports"),
            None,
            &mut vec![],
        )
        .unwrap_err();
        herdr.fail_close_tab = false;
        let later = halloween_noon() + Duration::from_secs(3600);
        let mut out = vec![];

        retire_and_close(
            &herdr,
            &mut ledger,
            "a",
            later,
            Path::new("/nonexistent/reports"),
            None,
            &mut out,
        )
        .unwrap();

        assert_eq!(
            out,
            [
                "a is already retired (since 2026-10-31T12:00:00Z)".to_string(),
                "closed tab w1:t1".into(),
                format!("removed {}", wt.display()),
            ]
        );
        assert_eq!(
            ledger.0["a"].retired_at.as_deref(),
            Some("2026-10-31T12:00:00Z")
        );
        assert!(herdr.tabs().is_empty());
        assert!(!wt.exists());
    }

    #[test]
    fn close_on_an_entry_retired_earlier_closes_the_tab_and_removes_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        let earlier = halloween_noon() - Duration::from_secs(86400);
        retire(&mut ledger, "a", earlier).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "a is already retired (since 2026-10-30T12:00:00Z)".to_string(),
                "closed tab w1:t1".into(),
                format!("removed {}", wt.display()),
            ]
        );
        assert_eq!(
            ledger.0["a"].retired_at.as_deref(),
            Some("2026-10-30T12:00:00Z")
        );
        assert!(herdr.tabs().is_empty());
        assert!(!wt.exists());
    }

    #[test]
    fn close_rerun_after_cleanup_reports_the_tab_and_worktree_already_gone() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        close(&herdr, &mut ledger);

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "a is already retired (since 2026-10-31T12:00:00Z)".to_string(),
                "tab already gone".into(),
                format!("worktree {} already gone", wt.display()),
            ]
        );
    }

    #[test]
    fn close_removes_a_deleted_worktrees_record_and_deletes_its_merged_branch() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        std::fs::remove_dir_all(&wt).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "closed tab w1:t1".into(),
                format!("removed {} (already gone)", wt.display()),
            ]
        );
        assert!(!worktree_list(&repo).contains("feat-new"));
        assert!(!has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_removes_a_deleted_worktrees_record_but_keeps_its_unmerged_branch() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        commit(&wt, "unpushed");
        std::fs::remove_dir_all(&wt).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out[2..],
            [
                format!("removed {} (already gone)", wt.display()),
                "kept branch feat/new: feat/new is not merged into origin/main".into(),
            ]
        );
        assert!(!worktree_list(&repo).contains("feat-new"));
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_of_a_deleted_worktree_leaves_another_missing_worktrees_record() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let other = crate::worktree::ensure_worktree(&repo, "feat/other").unwrap();
        std::fs::remove_dir_all(root.path().join("proj/worktrees/feat-new")).unwrap();
        std::fs::remove_dir_all(&other).unwrap();

        close(&herdr, &mut ledger);

        let list = worktree_list(&repo);
        assert!(!list.contains("feat-new"));
        assert!(list.contains("feat-other"));
    }

    #[test]
    fn close_keeps_a_deleted_worktree_that_is_locked_with_its_reason() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        git_in(
            &repo,
            &[
                "worktree",
                "lock",
                "--reason",
                "on a USB drive",
                wt.to_str().unwrap(),
            ],
        );
        std::fs::remove_dir_all(&wt).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out[2..],
            [format!("kept {}: locked (on a USB drive)", wt.display())]
        );
        assert!(worktree_list(&repo).contains("feat-new"));
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_a_deleted_worktree_that_is_locked_without_a_reason() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        git_in(&repo, &["worktree", "lock", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&wt).unwrap();

        let out = close(&herdr, &mut ledger);

        assert_eq!(out[2..], [format!("kept {}: locked", wt.display())]);
        assert!(worktree_list(&repo).contains("feat-new"));
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_a_worktree_it_cannot_look_at() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let parent = root.path().join("proj/worktrees");
        let wt = parent.join("feat-new");
        let mode = |m| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(m)).unwrap();
        };
        mode(0o000);

        let out = close(&herdr, &mut ledger);

        mode(0o755);
        assert_eq!(
            out.last().unwrap(),
            &format!(
                "kept {}: cannot check it: Permission denied (os error 13)",
                wt.display()
            )
        );
        assert!(wt.is_dir());
        assert!(worktree_list(&repo).contains("feat-new"));
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_a_deleted_worktrees_record_when_the_fetch_fails() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        std::fs::remove_dir_all(root.path().join("proj/worktrees/feat-new")).unwrap();
        let missing = root.path().join("no-origin.git");
        git_in(
            &repo,
            &["remote", "set-url", "origin", missing.to_str().unwrap()],
        );

        retire_and_close(
            &herdr,
            &mut ledger,
            "a",
            halloween_noon(),
            Path::new("/nonexistent/reports"),
            None,
            &mut vec![],
        )
        .unwrap_err();

        assert!(worktree_list(&repo).contains("feat-new"));
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_the_tab_running_this_command() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "a", cwd.path(), true);
        let mut ledger = plain(cwd.path(), &pane);
        let mut out = vec![];

        retire_and_close(
            &herdr,
            &mut ledger,
            "a",
            halloween_noon(),
            Path::new("/nonexistent/reports"),
            Some(&pane),
            &mut out,
        )
        .unwrap();

        assert_eq!(
            out,
            ["retired a", "kept tab w1:t1: it is running this command"]
        );
        assert_eq!(herdr.tabs().len(), 1);
    }

    /// Commits `.gitignore` with `patterns` in the worktree and lands it on origin's main.
    fn merged_with_ignores(wt: &Path, patterns: &str) {
        std::fs::write(wt.join(".gitignore"), patterns).unwrap();
        git_in(wt, &["add", ".gitignore"]);
        commit(wt, "ignore");
        push_head(wt, "main");
    }

    fn write(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn close_keeps_a_merged_worktree_holding_ignored_files() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        merged_with_ignores(&wt, ".env\n");
        write(&wt.join(".env"));

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "ignored file .env");
        assert!(wt.join(".env").exists());
        assert!(has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_removes_a_merged_worktree_whose_only_ignored_files_are_build_dirs() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, repo) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        merged_with_ignores(&wt, "target/\nnode_modules/\n");
        write(&wt.join("target/debug/loopherd"));
        write(&wt.join("web/node_modules/pkg/index.js"));

        let out = close(&herdr, &mut ledger);

        assert_eq!(out.last().unwrap(), &format!("removed {}", wt.display()));
        assert!(!wt.exists());
        assert!(!has_branch(&repo, "feat/new"));
    }

    #[test]
    fn close_keeps_a_worktree_with_a_build_dir_and_another_ignored_file() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        merged_with_ignores(&wt, "target/\n.env\n");
        write(&wt.join("target/debug/loopherd"));
        write(&wt.join(".env"));

        let out = close(&herdr, &mut ledger);

        kept_line(&out, &wt, "ignored file .env");
        assert!(wt.join(".env").exists());
    }

    #[test]
    fn close_keeps_a_tab_split_with_another_pane_and_its_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        herdr.split("w1:t1", root.path());

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "kept tab w1:t1: it has 2 panes".into(),
                format!("kept {}: its tab was kept", wt.display()),
            ]
        );
        assert_eq!(herdr.tabs().len(), 1);
        assert!(wt.is_dir());
    }

    /// A worktree entry whose agent is gone and whose pane id a restart gave
    /// to a new single-pane tab in the same worktree. Returns that pane.
    fn reused_after_restart(
        herdr: &FakeHerdr,
        ledger: &mut MemLedger,
        wt: &Path,
        at_prompt: bool,
    ) -> String {
        herdr.close_tab("w1:t1").unwrap();
        let pane = herdr.add_tab("w1", "a", wt, at_prompt);
        ledger.0.get_mut("a").unwrap().pane_id = pane.clone();
        pane
    }

    #[test]
    fn close_keeps_a_reused_pane_recorded_by_another_entry_and_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, false);
        let mut b = record("b");
        b.pane_id = pane;
        b.socket = Some(FAKE_SOCKET.into());
        ledger.0.insert("b".into(), b);

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "kept tab w1:t2: pane belongs to b".into(),
                format!("kept {}: its tab was kept", wt.display()),
            ]
        );
        assert_eq!(herdr.tabs().len(), 1);
        assert!(wt.is_dir());
    }

    #[test]
    fn close_ignores_an_entry_recording_the_same_pane_id_in_another_herdr() {
        for socket in [Some("/run/other/herdr.sock"), None] {
            let root = tempfile::tempdir().unwrap();
            let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
            let wt = root.path().join("proj/worktrees/feat-new");
            let pane = reused_after_restart(&herdr, &mut ledger, &wt, true);
            let mut b = record("b");
            b.pane_id = pane;
            b.socket = socket.map(PathBuf::from);
            ledger.0.insert("b".into(), b);

            let out = close(&herdr, &mut ledger);

            assert_eq!(out[1], "closed tab w1:t2", "{socket:?}");
        }
    }

    #[test]
    fn close_keeps_a_pane_a_socketless_entry_recorded_when_herdr_names_no_socket() {
        let root = tempfile::tempdir().unwrap();
        let (mut herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        herdr.socket = None;
        let wt = root.path().join("proj/worktrees/feat-new");
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, true);
        let mut b = record("b");
        b.pane_id = pane;
        ledger.0.insert("b".into(), b);

        let out = close(&herdr, &mut ledger);

        assert_eq!(out[1], "kept tab w1:t2: pane belongs to b");
    }

    #[test]
    fn close_ignores_a_retired_entry_that_recorded_the_same_pane() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, true);
        let mut old = record("old");
        old.pane_id = pane;
        old.retired = true;
        ledger.0.insert("old".into(), old);

        let out = close(&herdr, &mut ledger);

        assert_eq!(out[1], "closed tab w1:t2");
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn close_keeps_a_reused_pane_hosting_a_differently_named_agent_and_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, false);
        let herdr = herdr.with_live_agent(Some("stranger"), None, &pane);

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "kept tab w1:t2: pane hosts agent stranger".into(),
                format!("kept {}: its tab was kept", wt.display()),
            ]
        );
        assert_eq!(herdr.tabs().len(), 1);
        assert!(wt.is_dir());
    }

    fn kept_reused(out: &[String], wt: &Path, reason: &str) {
        assert_eq!(
            out,
            [
                "retired a".to_string(),
                format!("kept tab w1:t2: {reason}"),
                format!("kept {}: its tab was kept", wt.display()),
            ]
        );
    }

    #[test]
    fn close_keeps_a_reused_pane_hosting_an_unnamed_agent_from_another_session() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        ledger.0.get_mut("a").unwrap().session_id = Some("sess-a".into());
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, false);
        let herdr = herdr.with_live_agent(None, Some("sess-user"), &pane);

        kept_reused(&close(&herdr, &mut ledger), &wt, "pane hosts another agent");
        assert_eq!(herdr.tabs().len(), 1);
        assert!(wt.is_dir());
    }

    #[test]
    fn close_keeps_a_reused_pane_hosting_an_agent_it_cant_identify() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        ledger.0.get_mut("a").unwrap().session_id = None;
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, false);
        let herdr = herdr.with_live_agent(None, None, &pane);

        kept_reused(&close(&herdr, &mut ledger), &wt, UNIDENTIFIED);
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn close_closes_the_recorded_pane_hosting_its_own_unnamed_session() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        ledger.0.get_mut("a").unwrap().session_id = Some("sess-a".into());
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, false);
        let herdr = herdr.with_live_agent(None, Some("sess-a"), &pane);

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "closed tab w1:t2".into(),
                format!("removed {}", wt.display()),
            ]
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn close_keeps_a_reused_pane_whose_shell_is_running_a_command() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        reused_after_restart(&herdr, &mut ledger, &wt, false);

        kept_reused(
            &close(&herdr, &mut ledger),
            &wt,
            "pane is running a command",
        );
        assert_eq!(herdr.tabs().len(), 1);
        assert!(wt.is_dir());
    }

    #[test]
    fn close_closes_the_recorded_pane_idle_at_a_shell_prompt() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        reused_after_restart(&herdr, &mut ledger, &wt, true);

        let out = close(&herdr, &mut ledger);

        assert_eq!(
            out,
            [
                "retired a".to_string(),
                "closed tab w1:t2".into(),
                format!("removed {}", wt.display()),
            ]
        );
        assert!(herdr.tabs().is_empty());
    }

    #[test]
    fn close_leaves_another_tab_alone_that_reused_the_pane_id_at_the_entrys_cwd() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        herdr.close_tab("w1:t1").unwrap();
        let pane = herdr.add_tab("w1", "1", &wt, true);
        ledger.0.get_mut("a").unwrap().pane_id = pane;

        kept_reused(&close(&herdr, &mut ledger), &wt, "it is labeled 1, not a");
        assert_eq!(herdr.tabs().len(), 1);
    }
}
