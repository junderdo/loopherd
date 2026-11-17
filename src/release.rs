//! `loopherd release`: let `feed` spawn a retired member's name again.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::report::{Body, Entry, append};
use crate::retire::open_tab;
use crate::watch::forget_member;
use crate::worktree::{Cleared, Fate, clear_checkout, same_dir};

/// Marks the retired `name` released, so the next `feed` may spawn a member
/// by that name, after clearing its worktree and branch per `fate` so the
/// new member starts afresh. Refuses, changing nothing, a live entry, a
/// lead, one whose tab may still be open, or a checkout it would lose work
/// from. Rerunning it on a released entry clears and forgets again. Pushes
/// one line per action to `out`.
pub fn release(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    name: &str,
    fate: Fate,
    now: SystemTime,
    mailboxes: &Path,
    out: &mut Vec<String>,
) -> Result<()> {
    let mut others = ledger.entries()?;
    let Some(record) = others.remove(name) else {
        bail!("{name} is not in the ledger");
    };
    if !record.retired {
        bail!("refused: {name} is not retired; only a retired member's task can be fed again");
    }
    if record.is_lead() {
        bail!("refused: {name} is a lead, which feed never spawns; use `loopherd lead`");
    }
    if let Some(socket) = &record.socket {
        match herdr.socket() {
            None => bail!(
                "refused: can't ask herdr whether {name}'s tab is closed: \
                 herdr didn't say which session it runs"
            ),
            Some(here) if &here != socket => bail!(
                "refused: {name} ran on herdr session {}, not this one; run it there",
                socket.display()
            ),
            Some(_) => {}
        }
    }
    if let Some(fix) = open_tab(herdr, &record, &others)
        .with_context(|| format!("refused: can't ask herdr whether {name}'s tab is closed"))?
    {
        bail!("refused: {name}'s tab may still be open; close it first:\n  {fix}");
    }
    let in_use = |path: &Path| {
        others
            .values()
            .find(|r| !r.retired && r.worktree.as_deref().is_some_and(|w| same_dir(w, path)))
            .map(|r| r.name.clone())
    };
    let branch = record.branch.as_deref().filter(|b| !b.is_empty());
    let cleared = clear_checkout(
        &record.repo,
        record.worktree.as_deref(),
        branch,
        fate,
        &in_use,
    )?;
    report_cleared(cleared, branch, out)?;
    ledger.update(name, &mut |r| {
        if !r.retired {
            bail!("refused: {name} is no longer retired");
        }
        if r.released_at.is_none() {
            r.released_at = Some(humantime::format_rfc3339_seconds(now).to_string());
        }
        Ok(())
    })?;
    match &record.released_at {
        Some(at) => out.push(format!("{name} is already released (since {at})")),
        None => {
            out.push(format!("released {name}: feed may spawn it again"));
            let text = format!("released: feed may spawn {name} again");
            let note = Entry::new(now, Body::Note { text });
            if let Err(e) = append(mailboxes, name, &note) {
                out.push(format!("warning: note to {name}'s mailbox: {e:#}"));
            }
        }
    }
    let lead = others
        .values()
        .find(|r| r.is_lead() && !r.retired && r.herd.is_some() && r.herd == record.herd);
    if let Some(lead) = lead
        && let Err(e) = forget_member(mailboxes, &lead.name, name)
    {
        out.push(format!(
            "warning: {}'s watch won't announce {name} claimable: {e:#}; rerun to retry",
            lead.name
        ));
    }
    Ok(())
}

fn report_cleared(cleared: Cleared, branch: Option<&str>, out: &mut Vec<String>) -> Result<()> {
    let short = |head: &str| head[..head.len().min(12)].to_string();
    let remote = branch.map(|b| format!("origin/{b}")).unwrap_or_default();
    match cleared {
        Cleared::Absent => {}
        Cleared::Kept {
            local,
            remote: kept_remote,
        } => out.push(match (local, kept_remote) {
            (true, false) => "kept its worktree and branch: the new member starts from them".into(),
            (false, _) => format!("kept {remote}: the new member starts from it"),
            (true, true) => {
                format!("kept its worktree, branch and {remote}: the new member starts from them")
            }
        }),
        Cleared::Removed {
            worktrees,
            branch: head,
            remote: remote_head,
            dropped,
            base,
        } => {
            for (path, head) in worktrees {
                out.push(format!("removed {} (at {})", path.display(), short(&head)));
            }
            if let (Some(branch), Some(head)) = (branch, head) {
                out.push(format!("deleted branch {branch} (was {})", short(&head)));
            }
            if let Some(head) = remote_head {
                out.push(format!("deleted {remote} (was {})", short(&head)));
            }
            if dropped > 0 {
                out.push(format!(
                    "dropped {dropped} commit{} not on {base} or another origin branch",
                    plural(dropped)
                ));
            }
        }
        Cleared::Ahead { count, base } => bail!(
            "refused: its worktree, branch{} hold {count} commit{} not on {base}; pass \
             --keep-branch to start the new member from them, or --drop-branch to delete them",
            if remote.is_empty() {
                String::new()
            } else {
                format!(" or {remote}")
            },
            plural(count)
        ),
        Cleared::Dirty(reason) => {
            bail!("refused: {reason}; clean it up, or pass --keep-branch to start from it")
        }
        Cleared::Taken(reason) => bail!("refused: {reason}"),
    }
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger, Role};
    use crate::report::history;
    use crate::worktree::ensure_worktree;
    use crate::worktree::testing::{commit, git_in, has_branch, head, push_head, repo_with_origin};

    fn record(name: &str, repo: PathBuf, retired: bool) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            session_id: Some("abc".into()),
            repo,
            worktree: None,
            branch: Some(String::new()),
            workspace: "w1".into(),
            workspace_label: Some("loopherd".into()),
            claude_args: vec![],
            prompt: None,
            prompt_sent: true,
            created: "2026-11-16T00:00:00Z".into(),
            retired,
            retired_at: retired.then(|| "2026-11-16T01:00:00Z".into()),
            released_at: None,
            herd: Some("prs".into()),
            template: Some("pr".into()),
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
        }
    }

    struct Setup {
        root: tempfile::TempDir,
        repo: PathBuf,
        herdr: FakeHerdr,
        ledger: MemLedger,
    }

    impl Setup {
        fn new() -> Setup {
            let root = tempfile::tempdir().unwrap();
            let repo = repo_with_origin(root.path());
            Setup {
                root,
                repo,
                herdr: FakeHerdr::new(&[("w1", "loopherd")]),
                ledger: MemLedger::default(),
            }
        }

        fn add(&mut self, record: AgentRecord) {
            self.ledger.0.insert(record.name.clone(), record);
        }

        /// A retired `name` whose worktree is on its own `branch`.
        fn retired_on(&mut self, name: &str, branch: &str) -> PathBuf {
            let wt = ensure_worktree(&self.repo, branch).unwrap();
            let mut r = record(name, self.repo.clone(), true);
            r.worktree = Some(wt.clone());
            r.branch = Some(branch.into());
            self.add(r);
            wt
        }

        fn mailboxes(&self) -> PathBuf {
            self.root.path().join("reports")
        }

        fn release(&mut self, name: &str, fate: Fate) -> Result<Vec<String>> {
            let mut out = vec![];
            let now = humantime::parse_rfc3339("2026-11-17T12:00:00Z").unwrap();
            let mailboxes = self.mailboxes();
            release(
                &self.herdr,
                &mut self.ledger,
                name,
                fate,
                now,
                &mailboxes,
                &mut out,
            )?;
            Ok(out)
        }
    }

    #[test]
    fn marks_a_retired_entry_released_and_notes_it_in_its_mailbox() {
        let mut s = Setup::new();
        s.add(record("zpr-1", s.repo.clone(), true));

        let out = s.release("zpr-1", Fate::Clear).unwrap();

        assert_eq!(out, ["released zpr-1: feed may spawn it again"]);
        let released = &s.ledger.0["zpr-1"];
        assert_eq!(
            released.released_at.as_deref(),
            Some("2026-11-17T12:00:00Z")
        );
        assert!(released.retired);
        let (entries, _) = history(&s.mailboxes(), "zpr-1").unwrap();
        assert_eq!(
            entries.last().unwrap().body,
            Body::Note {
                text: "released: feed may spawn zpr-1 again".into()
            }
        );
    }

    #[test]
    fn tells_its_herds_lead_watch_to_forget_the_name() {
        let mut s = Setup::new();
        s.add(record("zpr-1", s.repo.clone(), true));
        let mut lead = record("lead-prs", s.repo.clone(), false);
        lead.role = Some(Role::Lead);
        s.add(lead);
        let fired = s.mailboxes().join("watch-lead-prs.fired");
        std::fs::create_dir_all(s.mailboxes()).unwrap();
        std::fs::write(&fired, "lead t\nzpr-1 claimable\n").unwrap();

        s.release("zpr-1", Fate::Clear).unwrap();

        assert_eq!(
            std::fs::read_to_string(&fired).unwrap(),
            "lead t\nzpr-1 claimable\nreleased: zpr-1\n"
        );
    }

    #[test]
    fn refuses_a_live_entry_by_name_and_changes_nothing() {
        let mut s = Setup::new();
        s.add(record("zpr-1", s.repo.clone(), false));
        let before = s.ledger.0.clone();

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: zpr-1 is not retired; only a retired member's task can be fed again"
        );
        assert_eq!(s.ledger.0, before);
    }

    #[test]
    fn refuses_an_unknown_name() {
        let mut s = Setup::new();

        let err = s.release("ghost", Fate::Clear).unwrap_err();

        assert_eq!(format!("{err:#}"), "ghost is not in the ledger");
    }

    #[test]
    fn an_already_released_entry_keeps_its_first_release() {
        let mut s = Setup::new();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.released_at = Some("2026-11-16T12:00:00Z".into());
        s.add(r);

        let out = s.release("zpr-1", Fate::Clear).unwrap();

        assert_eq!(
            out,
            ["zpr-1 is already released (since 2026-11-16T12:00:00Z)"]
        );
        assert_eq!(
            s.ledger.0["zpr-1"].released_at.as_deref(),
            Some("2026-11-16T12:00:00Z")
        );
    }

    #[test]
    fn a_rerun_with_another_fate_clears_what_the_first_kept() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "unpushed");
        s.release("zpr-1", Fate::Keep).unwrap();

        let out = s.release("zpr-1", Fate::Drop).unwrap();

        assert_eq!(
            out.last().unwrap(),
            "zpr-1 is already released (since 2026-11-17T12:00:00Z)"
        );
        assert!(!wt.exists());
        assert!(!has_branch(&s.repo, "feat/zpr-1"));
    }

    #[test]
    fn refuses_a_lead() {
        let mut s = Setup::new();
        let mut lead = record("lead-prs", s.repo.clone(), true);
        lead.role = Some(Role::Lead);
        s.add(lead);

        let err = s.release("lead-prs", Fate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: lead-prs is a lead, which feed never spawns; use `loopherd lead`"
        );
    }

    #[test]
    fn clears_a_detached_worktree_so_the_new_member_gets_a_fresh_one() {
        let mut s = Setup::new();
        let wt = crate::worktree::ensure_detached_worktree(&s.repo, "zpr-1").unwrap();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.worktree = Some(wt.clone());
        s.add(r);
        let start = head(&wt);

        let out = s.release("zpr-1", Fate::Clear).unwrap();

        assert_eq!(
            out[0],
            format!("removed {} (at {})", wt.display(), &start[..12])
        );
        assert!(!wt.exists());
    }

    #[test]
    fn refuses_while_herdr_still_shows_its_agent() {
        let mut s = Setup::new();
        s.add(record("zpr-1", s.repo.clone(), true));
        s.herdr = FakeHerdr::new(&[("w1", "loopherd")]).with_agent("zpr-1");

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        let msg = format!("{err:#}");
        assert_eq!(
            msg,
            "refused: zpr-1's tab may still be open; close it first:\n  \
             herdr tab close w9:t9  # zpr-1: tab w9:t9 is open: not in workspace loopherd"
        );
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn refuses_an_entry_from_another_herdr_session() {
        let mut s = Setup::new();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.socket = Some("/run/other/herdr.sock".into());
        s.add(r);

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: zpr-1 ran on herdr session /run/other/herdr.sock, not this one; run it there"
        );
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn refuses_a_socketed_entry_as_unaskable_when_herdr_cant_be_reached() {
        let mut s = Setup::new();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.socket = Some("/run/other/herdr.sock".into());
        s.add(r);
        s.herdr.set_unreachable(true);
        s.herdr.socket = None;

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.starts_with("refused: can't ask herdr whether zpr-1's tab is closed: "),
            "{msg}"
        );
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn removes_a_branch_with_nothing_new_and_its_worktree_so_the_next_member_starts_afresh() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        let start = head(&wt);

        let out = s.release("zpr-1", Fate::Clear).unwrap();

        assert_eq!(
            out,
            [
                format!("removed {} (at {})", wt.display(), &start[..12]),
                format!("deleted branch feat/zpr-1 (was {})", &start[..12]),
                "released zpr-1: feed may spawn it again".into(),
            ]
        );
        assert!(!has_branch(&s.repo, "feat/zpr-1"));
    }

    #[test]
    fn refuses_a_branch_with_unpushed_commits_unless_told_to_keep_or_drop_them() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "unpushed");

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: its worktree, branch or origin/feat/zpr-1 hold 1 commit not on origin/main; pass \
             --keep-branch to start the new member from them, or --drop-branch to delete them"
        );
        assert!(has_branch(&s.repo, "feat/zpr-1"));
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn keep_branch_leaves_the_branch_and_worktree_for_the_new_member() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "unpushed");

        let out = s.release("zpr-1", Fate::Keep).unwrap();

        assert_eq!(
            out,
            [
                "kept its worktree and branch: the new member starts from them",
                "released zpr-1: feed may spawn it again",
            ]
        );
        assert!(wt.exists());
        assert!(has_branch(&s.repo, "feat/zpr-1"));
    }

    #[test]
    fn drop_branch_deletes_unpushed_commits_and_says_which_head_they_were_at() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "unpushed");
        let unpushed = head(&wt);

        let out = s.release("zpr-1", Fate::Drop).unwrap();

        assert_eq!(
            out[1..3],
            [
                format!("deleted branch feat/zpr-1 (was {})", &unpushed[..12]),
                "dropped 1 commit not on origin/main or another origin branch".into(),
            ]
        );
        assert!(!has_branch(&s.repo, "feat/zpr-1"));
        assert!(s.ledger.0["zpr-1"].released_at.is_some());
    }

    #[test]
    fn refuses_a_worktree_a_live_entry_is_in() {
        let mut s = Setup::new();
        s.retired_on("zpr-1", "feat/zpr-1");
        let wt = s.ledger.0["zpr-1"].worktree.clone();
        let mut live = record("other", s.repo.clone(), false);
        live.worktree = wt.clone();
        s.add(live);

        let err = s.release("zpr-1", Fate::Drop).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            format!("refused: {} is in use by other", wt.unwrap().display())
        );
        assert!(has_branch(&s.repo, "feat/zpr-1"));
    }

    #[test]
    fn keep_branch_with_nothing_to_keep_says_nothing_about_keeping() {
        let mut s = Setup::new();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.branch = Some("feat/zpr-1".into());
        s.add(r);

        let out = s.release("zpr-1", Fate::Keep).unwrap();

        assert_eq!(out, ["released zpr-1: feed may spawn it again"]);
    }

    #[test]
    fn drop_branch_deletes_the_branch_on_origin_too_and_says_where_it_was() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "pushed");
        push_head(&wt, "feat/zpr-1");
        let pushed = head(&wt);

        let out = s.release("zpr-1", Fate::Drop).unwrap();

        assert_eq!(
            out[1..4],
            [
                format!("deleted branch feat/zpr-1 (was {})", &pushed[..12]),
                format!("deleted origin/feat/zpr-1 (was {})", &pushed[..12]),
                "dropped 1 commit not on origin/main or another origin branch".into(),
            ]
        );
        assert!(!has_branch(&s.root.path().join("origin.git"), "feat/zpr-1"));
    }

    #[test]
    fn keep_branch_with_only_an_origin_branch_says_the_new_member_starts_from_it() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "pushed");
        push_head(&wt, "feat/zpr-1");
        git_in(&s.repo, &["worktree", "remove", wt.to_str().unwrap()]);
        git_in(&s.repo, &["branch", "--quiet", "-D", "feat/zpr-1"]);

        let out = s.release("zpr-1", Fate::Keep).unwrap();

        assert_eq!(
            out[0],
            "kept origin/feat/zpr-1: the new member starts from it"
        );
    }
}
