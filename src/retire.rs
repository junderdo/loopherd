//! `loopherd retire`: mark a ledger entry finished so revive leaves it alone.

use std::time::SystemTime;

use anyhow::{Result, anyhow, bail};

use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, Ledger};
use crate::revive::find_workspace;
use crate::worktree::{Removal, remove_if_merged};

/// Marks `name` retired at `now`. Touches only the ledger.
pub fn retire(ledger: &mut dyn Ledger, name: &str, now: SystemTime) -> Result<AgentRecord> {
    let mut entries = ledger.entries()?;
    let record = entries
        .get_mut(name)
        .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
    if record.retired {
        bail!(
            "{name} is already retired (since {})",
            record.retired_at.as_deref().unwrap_or("an unknown time")
        );
    }
    record.retired = true;
    record.retired_at = Some(humantime::format_rfc3339_seconds(now).to_string());
    ledger.record(record.clone())?;
    Ok(record.clone())
}

/// Retires `name`, then closes its tab and removes its worktree when safe.
/// `own_pane` is the pane running loopherd, whose tab is never closed.
/// Pushes one line per action to `out`, including before an error.
pub fn retire_and_close(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    name: &str,
    now: SystemTime,
    own_pane: Option<&str>,
    out: &mut Vec<String>,
) -> Result<()> {
    let record = retire(ledger, name, now)?;
    out.push(format!("retired {name}"));
    let tab_kept = match agent_tab(herdr, &record, own_pane)? {
        TabFate::Gone => false,
        TabFate::Ours(tab_id) => {
            herdr.close_tab(&tab_id)?;
            out.push(format!("closed tab {tab_id}"));
            false
        }
        TabFate::Kept { tab_id, reason } => {
            out.push(format!("kept tab {tab_id}: {reason}"));
            true
        }
    };
    if let Some(path) = &record.worktree {
        let removal = match tab_kept {
            true => Removal::Kept("its tab was kept".into()),
            false => remove_if_merged(&record.repo, path)?,
        };
        out.push(match removal {
            Removal::Removed => format!("removed {}", path.display()),
            Removal::Kept(reason) => format!("kept {}: {reason}", path.display()),
        });
    }
    Ok(())
}

enum TabFate {
    Gone,
    Ours(String),
    Kept { tab_id: String, reason: String },
}

/// Finds the entry's tab through its live agent, else its recorded pane, and
/// checks its workspace label and cwd as revive does (D8).
fn agent_tab(herdr: &dyn Herdr, record: &AgentRecord, own_pane: Option<&str>) -> Result<TabFate> {
    let agents = herdr.agents()?;
    let live = agents
        .iter()
        .find(|a| a.name.as_deref() == Some(record.name.as_str()));
    let kept = |tab_id: &str, reason: String| {
        Ok(TabFate::Kept {
            tab_id: tab_id.to_string(),
            reason,
        })
    };
    if let Some(live) = live
        && live.session_id.is_some()
        && record.session_id.is_some()
        && live.session_id != record.session_id
    {
        return kept(&live.tab_id, "the name is held by another session".into());
    }
    let pane_id = live.map_or(&record.pane_id, |a| &a.pane_id);
    let workspace = match &record.workspace_label {
        Some(label) => find_workspace(herdr, |w| &w.label == label)?,
        None => find_workspace(herdr, |w| w.id == record.workspace)?,
    };
    let shown = record.workspace_label.as_ref().unwrap_or(&record.workspace);
    let panes = match &workspace {
        Some(ws) => herdr.panes_in(&ws.id)?,
        None => vec![],
    };
    let pane = panes.iter().find(|p| &p.id == pane_id).cloned();
    if let Some(pane) = &pane
        && panes
            .iter()
            .any(|p| Some(p.id.as_str()) == own_pane && p.tab_id == pane.tab_id)
    {
        return kept(&pane.tab_id, "it is running this command".into());
    }
    let cwd = record.worktree.as_ref().unwrap_or(&record.repo);
    match (pane, live) {
        (None, None) => Ok(TabFate::Gone),
        (None, Some(live)) => kept(&live.tab_id, format!("not in workspace {shown}")),
        (Some(pane), _) if pane.cwd.as_deref() == Some(cwd.as_path()) => {
            Ok(TabFate::Ours(pane.tab_id))
        }
        // The recorded pane ID was reused by another pane after a restart.
        (Some(_), None) => Ok(TabFate::Gone),
        (Some(pane), Some(_)) => {
            let actual = pane
                .cwd
                .map_or("unknown".into(), |c| c.display().to_string());
            kept(
                &pane.tab_id,
                format!("its cwd is {actual}, not {}", cwd.display()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::MemLedger;
    use crate::spawn::{SpawnRequest, spawn};
    use crate::worktree::testing::{
        commit, current_branch, git_in, has_branch, push_head, repo_with_origin,
    };

    fn record(name: &str) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            session_id: Some("abc".into()),
            repo: "/src/repo".into(),
            worktree: None,
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-10-31T00:00:00Z".into(),
            retired: false,
            retired_at: None,
            herd: None,
            template: None,
        }
    }

    fn ledger_with(names: &[&str]) -> MemLedger {
        MemLedger(names.iter().map(|n| (n.to_string(), record(n))).collect())
    }

    fn halloween_noon() -> SystemTime {
        humantime::parse_rfc3339("2026-10-31T12:00:00Z").unwrap()
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
            claude_args: vec![],
            member_of: None,
        };
        spawn(&herdr, &mut ledger, &req, Some("w1")).unwrap();
        (herdr, ledger, repo)
    }

    fn close(herdr: &FakeHerdr, ledger: &mut MemLedger) -> Vec<String> {
        let mut out = vec![];
        retire_and_close(herdr, ledger, "a", halloween_noon(), None, &mut out).unwrap();
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
        push_head(&wt, "main");

        let out = close(&herdr, &mut ledger);

        assert_eq!(out.last().unwrap(), &format!("removed {}", wt.display()));
        assert!(!wt.exists());
        assert!(!has_branch(&repo, "feat/new"));
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
            ["retired a".to_string(), format!("removed {}", wt.display())]
        );
        assert!(!wt.exists());
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
        FakeHerdr::new(&[("w1", "loopherd"), ("w2", "zagops")])
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
    fn close_leaves_a_reused_pane_id_in_another_cwd_alone() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "other", elsewhere.path(), true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(close(&herdr, &mut ledger), ["retired a"]);
        assert_eq!(herdr.tabs().len(), 1);
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

    #[test]
    fn close_with_the_workspace_gone_treats_the_tab_as_gone() {
        let cwd = tempfile::tempdir().unwrap();
        let herdr = FakeHerdr::new(&[("w1", "renamed")]);
        let pane = herdr.add_tab("w1", "a", cwd.path(), true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(close(&herdr, &mut ledger), ["retired a"]);
        assert_eq!(herdr.tabs().len(), 1);
    }

    #[test]
    fn a_failed_tab_close_is_an_error_after_retiring_and_keeps_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let (mut herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        herdr.fail_close_tab = true;
        let mut out = vec![];

        let err = retire_and_close(&herdr, &mut ledger, "a", halloween_noon(), None, &mut out)
            .unwrap_err();

        assert_eq!(format!("{err:#}"), "tab close refused");
        assert_eq!(out, ["retired a"]);
        assert!(ledger.0["a"].retired);
        assert!(root.path().join("proj/worktrees/feat-new").is_dir());
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
}
