//! `loopherd retire`: mark a ledger entry finished so revive leaves it alone.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Result, anyhow, bail};

use crate::herdr::{Agent, Herdr};
use crate::ledger::{AgentRecord, Ledger};
use crate::revive::find_workspace;
use crate::worktree::{Gone, Removal, remove_if_gone, remove_if_merged};

/// Marks `name` retired at `now`. Touches only the ledger.
pub fn retire(ledger: &mut dyn Ledger, name: &str, now: SystemTime) -> Result<AgentRecord> {
    let mut entries = ledger.entries()?;
    let record = entries
        .get_mut(name)
        .ok_or_else(|| anyhow!("{name} is not in the ledger"))?;
    if record.retired {
        bail!("{name} is {}", retired_since(record));
    }
    record.retired = true;
    record.retired_at = Some(humantime::format_rfc3339_seconds(now).to_string());
    ledger.record(record.clone())?;
    Ok(record.clone())
}

fn retired_since(record: &AgentRecord) -> String {
    let since = record.retired_at.as_deref().unwrap_or("an unknown time");
    format!("already retired (since {since})")
}

/// Retires `name` unless it already is, then closes its tab and removes its
/// worktree when safe, so a failed or deferred close can be re-run.
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
    let tab_kept = match agent_tab(herdr, &record, &others, own_pane)? {
        TabFate::Gone => {
            out.push("tab already gone".into());
            false
        }
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
            Some(Gone::Removed { kept_branch }) => {
                out.push(format!("removed {} (already gone)", path.display()));
                if let Some((branch, reason)) = kept_branch {
                    out.push(format!("kept branch {branch}: {reason}"));
                }
                return Ok(());
            }
            None => {}
        }
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

/// Falls back to an exact comparison when either can't be canonicalized.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

enum TabFate {
    Gone,
    Ours(String),
    Kept { tab_id: String, reason: String },
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
    if live.is_none()
        && let Some(pane) = &pane
        && let Some(reason) = reused_by(herdr, record, &pane.id, &agents, others)?
    {
        return kept(&pane.tab_id, reason);
    }
    let cwd = record.worktree.as_ref().unwrap_or(&record.repo);
    match (pane, live) {
        (None, None) => Ok(TabFate::Gone),
        (None, Some(live)) => kept(&live.tab_id, format!("not in workspace {shown}")),
        (Some(pane), _) if pane.is_in(cwd) => {
            // Closing a tab closes every pane in it, not just the agent's.
            match panes.iter().filter(|p| p.tab_id == pane.tab_id).count() {
                1 => Ok(TabFate::Ours(pane.tab_id)),
                n => kept(&pane.tab_id, format!("it has {n} panes")),
            }
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

/// Why the recorded pane may be someone else's after a restart reused its id:
/// another entry recorded it, it runs an agent from another session, or its
/// shell is busy. `None` means it is safe to treat as the entry's.
fn reused_by(
    herdr: &dyn Herdr,
    record: &AgentRecord,
    pane_id: &str,
    agents: &[Agent],
    others: &BTreeMap<String, AgentRecord>,
) -> Result<Option<String>> {
    if let Some(other) = others.values().find(|r| !r.retired && r.pane_id == pane_id) {
        return Ok(Some(format!("pane belongs to {}", other.name)));
    }
    if let Some(agent) = agents.iter().find(|a| a.pane_id == pane_id) {
        let ours = agent.session_id.is_some() && agent.session_id == record.session_id;
        return Ok(match (ours, &agent.name) {
            (true, _) => None,
            (false, Some(name)) => Some(format!("pane hosts agent {name}")),
            (false, None) => Some("pane hosts another agent".into()),
        });
    }
    Ok(match herdr.at_shell_prompt(pane_id)? {
        true => None,
        false => Some("pane is running a command".into()),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::MemLedger;
    use crate::spawn::{SpawnRequest, spawn};
    use crate::worktree::testing::{
        commit, current_branch, git_in, has_branch, head, head_of, merge_on_origin,
        move_origin_default, push_head, repo_with_origin, worktree_list,
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
    fn close_leaves_a_reused_pane_id_in_another_cwd_alone() {
        let cwd = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let herdr = two_workspaces();
        let pane = herdr.add_tab("w1", "other", elsewhere.path(), true);
        let mut ledger = plain(cwd.path(), &pane);

        assert_eq!(
            close(&herdr, &mut ledger),
            ["retired a", "tab already gone"]
        );
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

        assert_eq!(
            close(&herdr, &mut ledger),
            ["retired a", "tab already gone"]
        );
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
            None,
            &mut vec![],
        )
        .unwrap_err();
        herdr.fail_close_tab = false;
        let later = halloween_noon() + Duration::from_secs(3600);
        let mut out = vec![];

        retire_and_close(&herdr, &mut ledger, "a", later, None, &mut out).unwrap();

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
        let pane = herdr.add_tab("w1", "b", wt, at_prompt);
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
    fn close_keeps_a_reused_pane_hosting_an_agent_when_the_entry_has_no_session() {
        let root = tempfile::tempdir().unwrap();
        let (herdr, mut ledger, _) = spawned_in_worktree(root.path(), "feat/new");
        let wt = root.path().join("proj/worktrees/feat-new");
        ledger.0.get_mut("a").unwrap().session_id = None;
        let pane = reused_after_restart(&herdr, &mut ledger, &wt, false);
        let herdr = herdr.with_live_agent(None, None, &pane);

        kept_reused(&close(&herdr, &mut ledger), &wt, "pane hosts another agent");
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
}
