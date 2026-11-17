//! `loopherd release`: let `feed` spawn a retired member's name again.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::herdr::Herdr;
use crate::ledger::Ledger;
use crate::report::{Body, Entry, append};
use crate::retire::open_tab;
use crate::watch::forget_member;
use crate::worktree::{Cleared, clear_branch};

/// What to do with a released member's local branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchFate {
    /// Delete it, with its worktree, unless it has commits not on the base.
    Clear,
    /// Leave it and its worktree for the new member to start from.
    Keep,
    /// Delete it, with its worktree, even with commits not on the base.
    Drop,
}

/// Marks the retired `name` released, so the next `feed` may spawn a member
/// by that name, after clearing its branch per `fate` so the new member
/// starts afresh. Refuses, changing nothing, a live entry, one whose tab may
/// still be open, or a branch it would lose work from. Pushes one line per
/// action to `out`.
pub fn release(
    herdr: &dyn Herdr,
    ledger: &mut dyn Ledger,
    name: &str,
    fate: BranchFate,
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
    if let Some(at) = &record.released_at {
        out.push(format!("{name} is already released (since {at})"));
        return Ok(());
    }
    if let Some(socket) = &record.socket
        && herdr.socket().as_ref() != Some(socket)
    {
        bail!(
            "refused: {name} ran on herdr session {}, not this one; run it there",
            socket.display()
        );
    }
    if let Some(why) = open_tab(herdr, &record, &others)
        .with_context(|| format!("refused: can't ask herdr whether {name}'s tab is closed"))?
    {
        bail!("refused: {name}'s {why}; close it first with `loopherd retire --close {name}`");
    }
    let branch = record.branch.as_deref().filter(|b| !b.is_empty());
    if let Some(branch) = branch {
        if let Some(user) = others
            .values()
            .find(|r| !r.retired && r.repo == record.repo && r.branch.as_deref() == Some(branch))
        {
            bail!("refused: {branch} is in use by {}", user.name);
        }
        clear(&record.repo, branch, fate, out)?;
    }
    ledger.update(name, &mut |r| {
        if !r.retired {
            bail!("refused: {name} is no longer retired");
        }
        r.released_at = Some(humantime::format_rfc3339_seconds(now).to_string());
        Ok(())
    })?;
    out.push(format!("released {name}: feed may spawn it again"));
    let note = Entry::new(
        now,
        Body::Note {
            text: format!("released: feed may spawn {name} again"),
        },
    );
    if let Err(e) = append(mailboxes, name, &note) {
        out.push(format!("warning: note to {name}'s mailbox: {e:#}"));
    }
    let lead = others
        .values()
        .find(|r| r.is_lead() && !r.retired && r.herd.is_some() && r.herd == record.herd);
    if let Some(lead) = lead
        && let Err(e) = forget_member(mailboxes, &lead.name, name)
    {
        out.push(format!(
            "warning: {}'s watch won't announce {name} claimable: {e:#}",
            lead.name
        ));
    }
    Ok(())
}

fn clear(repo: &Path, branch: &str, fate: BranchFate, out: &mut Vec<String>) -> Result<()> {
    if fate == BranchFate::Keep {
        out.push(format!(
            "kept branch {branch}: the new member starts from it"
        ));
        return Ok(());
    }
    match clear_branch(repo, branch, fate == BranchFate::Drop)? {
        Cleared::Absent => {}
        Cleared::Removed {
            head,
            ahead,
            base,
            worktree,
        } => {
            if let Some(path) = worktree {
                out.push(format!("removed {}", path.display()));
            }
            let short = &head[..head.len().min(12)];
            out.push(match ahead {
                0 => format!("deleted branch {branch} (was {short})"),
                n => format!(
                    "deleted branch {branch} (was {short}), dropping {n} commit{} not on {base}",
                    if n == 1 { "" } else { "s" }
                ),
            });
        }
        Cleared::Ahead { count, base } => bail!(
            "refused: {branch} has {count} commit{} not on {base}; pass --keep-branch to start \
             the new member from it, or --drop-branch to delete it",
            if count == 1 { "" } else { "s" }
        ),
        Cleared::Kept(reason) => {
            bail!("refused: {reason}; clean it up, or pass --keep-branch to start from it")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{AgentRecord, MemLedger, Role};
    use crate::report::history;
    use crate::worktree::ensure_worktree;
    use crate::worktree::testing::{commit, has_branch, head, repo_with_origin};

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

        fn release(&mut self, name: &str, fate: BranchFate) -> Result<Vec<String>> {
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

        let out = s.release("zpr-1", BranchFate::Clear).unwrap();

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

        s.release("zpr-1", BranchFate::Clear).unwrap();

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

        let err = s.release("zpr-1", BranchFate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: zpr-1 is not retired; only a retired member's task can be fed again"
        );
        assert_eq!(s.ledger.0, before);
    }

    #[test]
    fn refuses_an_unknown_name() {
        let mut s = Setup::new();

        let err = s.release("ghost", BranchFate::Clear).unwrap_err();

        assert_eq!(format!("{err:#}"), "ghost is not in the ledger");
    }

    #[test]
    fn an_already_released_entry_keeps_its_first_release() {
        let mut s = Setup::new();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.released_at = Some("2026-11-16T12:00:00Z".into());
        s.add(r);

        let out = s.release("zpr-1", BranchFate::Clear).unwrap();

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
    fn refuses_while_herdr_still_shows_its_agent() {
        let mut s = Setup::new();
        s.add(record("zpr-1", s.repo.clone(), true));
        s.herdr = FakeHerdr::new(&[("w1", "loopherd")]).with_agent("zpr-1");

        let err = s.release("zpr-1", BranchFate::Clear).unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.starts_with("refused: zpr-1's tab "), "{msg}");
        assert!(
            msg.ends_with("close it first with `loopherd retire --close zpr-1`"),
            "{msg}"
        );
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn refuses_an_entry_from_another_herdr_session() {
        let mut s = Setup::new();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.socket = Some("/run/other/herdr.sock".into());
        s.add(r);

        let err = s.release("zpr-1", BranchFate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: zpr-1 ran on herdr session /run/other/herdr.sock, not this one; run it there"
        );
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn removes_a_branch_with_nothing_new_and_its_worktree_so_the_next_member_starts_afresh() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        let start = head(&wt);

        let out = s.release("zpr-1", BranchFate::Clear).unwrap();

        assert_eq!(
            out,
            [
                format!("removed {}", wt.display()),
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

        let err = s.release("zpr-1", BranchFate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: feat/zpr-1 has 1 commit not on origin/main; pass --keep-branch to start \
             the new member from it, or --drop-branch to delete it"
        );
        assert!(has_branch(&s.repo, "feat/zpr-1"));
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn keep_branch_leaves_the_branch_and_worktree_for_the_new_member() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "unpushed");

        let out = s.release("zpr-1", BranchFate::Keep).unwrap();

        assert_eq!(
            out,
            [
                "kept branch feat/zpr-1: the new member starts from it",
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

        let out = s.release("zpr-1", BranchFate::Drop).unwrap();

        assert_eq!(
            out[1],
            format!(
                "deleted branch feat/zpr-1 (was {}), dropping 1 commit not on origin/main",
                &unpushed[..12]
            )
        );
        assert!(!has_branch(&s.repo, "feat/zpr-1"));
        assert!(s.ledger.0["zpr-1"].released_at.is_some());
    }

    #[test]
    fn refuses_a_branch_a_live_entry_is_on() {
        let mut s = Setup::new();
        s.retired_on("zpr-1", "feat/zpr-1");
        let mut live = record("other", s.repo.clone(), false);
        live.branch = Some("feat/zpr-1".into());
        s.add(live);

        let err = s.release("zpr-1", BranchFate::Drop).unwrap_err();

        assert_eq!(format!("{err:#}"), "refused: feat/zpr-1 is in use by other");
        assert!(has_branch(&s.repo, "feat/zpr-1"));
    }
}
