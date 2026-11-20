//! `loopherd release`: let `feed` spawn a retired member's name again.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::github::{PrLookup, PullRequests};
use crate::herdr::Herdr;
use crate::ledger::{AgentRecord, Ledger, Outcome};
use crate::report::{Body, Entry, append};
use crate::retire::open_tab;
use crate::sandbox::Guard;
use crate::watch::forget_member;
use crate::worktree::{Fate, Plan, RemoteDelete, plan_clear, same_dir};

/// Marks the retired `name` released, so the next `feed` may spawn a member
/// by that name, after clearing its worktree and branch per `fate` so the
/// new member starts afresh. Refuses, changing nothing, a live entry, a
/// lead, one whose tab may still be open, a checkout it would lose work
/// from, one whose ledger entry `guard` refuses, or (unless dropping) an origin branch an open PR is on. Marks the
/// entry released last, only once origin's branch is gone, so a failed
/// delete leaves it unreleased for a rerun to finish. Rerunning it on a
/// released entry clears and forgets again. Pushes one line per action to
/// `out`.
#[allow(clippy::too_many_arguments)]
pub fn release(
    herdr: &dyn Herdr,
    prs: &dyn PullRequests,
    ledger: &mut dyn Ledger,
    guard: &Guard,
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
    if record.may_be_lead() {
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
    if let Some(why) = guard.path_refusal(&record)? {
        bail!("refused: {why}");
    }
    if guard.for_lead() && fate == Fate::Drop {
        bail!("refused: --drop-branch deletes commits, so it's the human's, not a lead's");
    }
    let in_use = |path: &Path| {
        others
            .values()
            .find(|r| !r.retired && r.worktree.as_deref().is_some_and(|w| same_dir(w, path)))
            .map(|r| r.name.clone())
    };
    let branch = record.branch.as_deref().filter(|b| !b.is_empty());
    let remote = branch.map(|b| format!("origin/{b}")).unwrap_or_default();
    let plan = plan_clear(
        &record.repo,
        record.worktree.as_deref(),
        branch,
        fate,
        &in_use,
    )?;
    let clearing = match plan {
        Plan::Clear(clearing) => clearing,
        other => {
            report_unchanged(other, &remote, out)?;
            return mark_released(ledger, &record, &others, now, mailboxes, out);
        }
    };
    if let (Some(branch), Some(_)) = (branch, &clearing.remote) {
        check_open_pr(prs, &record.repo, branch, fate, out)?;
    }
    clearing.remove_local()?;
    let short = |head: &str| head[..head.len().min(12)].to_string();
    for (path, head) in &clearing.worktrees {
        out.push(format!("removed {} (at {})", path.display(), short(head)));
    }
    if let (Some(branch), Some(head)) = (branch, &clearing.head) {
        out.push(format!("deleted branch {branch} (was {})", short(head)));
    }
    if clearing.dropped > 0 {
        out.push(format!(
            "dropped {} commit{} not on {} or another origin branch",
            clearing.dropped,
            plural(clearing.dropped),
            clearing.base
        ));
    }
    if let Some(head) = &clearing.remote {
        let deleted = clearing.delete_remote().with_context(|| {
            format!(
                "couldn't delete {remote} (was {}); fix the cause and rerun the same release \
                 command to retry. {name} stays unreleased, so feed won't spawn it, until \
                 that rerun deletes it",
                short(head)
            )
        })?;
        if deleted == RemoteDelete::Moved {
            bail!(
                "origin kept {remote}: someone pushed to it since release counted it at {}; \
                 check who, or rerun with --keep-branch to start the new member from it",
                short(head)
            );
        }
        out.push(format!("deleted {remote} (was {})", short(head)));
    }
    mark_released(ledger, &record, &others, now, mailboxes, out)
}

/// Refuses deleting `branch` on origin while an open PR is on it, unless
/// dropping it, which names the PR it closes.
fn check_open_pr(
    prs: &dyn PullRequests,
    repo: &Path,
    branch: &str,
    fate: Fate,
    out: &mut Vec<String>,
) -> Result<()> {
    let lookup = prs
        .open_on(repo, branch)
        .with_context(|| format!("refused: can't ask GitHub for an open PR on {branch}"))?;
    match lookup {
        PrLookup::None => {}
        PrLookup::Open(pr) if fate == Fate::Drop => out.push(format!(
            "origin/{branch} is the head of open PR #{} ({}); deleting it closes the PR",
            pr.number, pr.url
        )),
        PrLookup::Open(pr) => bail!(
            "refused: origin/{branch} is the head of open PR #{} ({}); deleting it would \
             close the PR. Pass --keep-branch to start the new member from it, or \
             --drop-branch to delete it anyway",
            pr.number,
            pr.url
        ),
        PrLookup::Unknown(why) => out.push(format!(
            "warning: can't check for an open PR on origin/{branch}: {why}"
        )),
    }
    Ok(())
}

fn mark_released(
    ledger: &mut dyn Ledger,
    record: &AgentRecord,
    others: &BTreeMap<String, AgentRecord>,
    now: SystemTime,
    mailboxes: &Path,
    out: &mut Vec<String>,
) -> Result<()> {
    let name = &record.name;
    ledger.update(name, &mut |r| {
        if !r.retired {
            bail!("refused: {name} is no longer retired");
        }
        if r.released_at.is_none() {
            r.released_at = Some(humantime::format_rfc3339_seconds(now).to_string());
            r.outcome = Some(Outcome::Released);
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

/// Reports a plan that leaves the checkout as it is, refusing the ones that
/// would lose work.
fn report_unchanged(plan: Plan, remote: &str, out: &mut Vec<String>) -> Result<()> {
    match plan {
        Plan::Absent | Plan::Clear(_) => {}
        Plan::Unchecked(why) => out.push(format!(
            "warning: couldn't ask origin for {remote} ({why}); \
             if it's there, rerun release once origin answers"
        )),
        Plan::Kept {
            local,
            remote: kept_remote,
        } => out.push(match (local, kept_remote) {
            (true, false) => "kept its worktree and branch: the new member starts from them".into(),
            (false, _) => format!("kept {remote}: the new member starts from it"),
            (true, true) => {
                format!("kept its worktree, branch and {remote}: the new member starts from them")
            }
        }),
        Plan::Ahead { count, base } => bail!(
            "refused: its worktree, branch{} hold {count} commit{} not on {base}; pass \
             --keep-branch to start the new member from them, or --drop-branch to delete them",
            if remote.is_empty() {
                String::new()
            } else {
                format!(" or {remote}")
            },
            plural(count)
        ),
        Plan::Dirty(reason) => {
            bail!("refused: {reason}; clean it up, or pass --keep-branch to start from it")
        }
        Plan::Taken(reason) => bail!("refused: {reason}"),
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
    use crate::github::Pr;
    use crate::github::fake::FakePrs;
    use crate::herd::Paths;
    use crate::herdr::fake::FakeHerdr;
    use crate::ledger::{MemLedger, Role};
    use crate::report::history;
    use crate::worktree::ensure_worktree;
    use crate::worktree::testing::{
        commit, git_in, has_branch, head, head_of, push_head, repo_with_origin,
    };

    fn record(name: &str, repo: PathBuf, retired: bool) -> AgentRecord {
        AgentRecord {
            name: name.into(),
            pane_id: "w1:p1".into(),
            env_pane_id: None,
            env_pane_shell: None,
            tab_id: None,
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
            outcome: None,
            released_at: None,
            herd: Some("prs".into()),
            template: Some("pr".into()),
            params: None,
            socket: None,
            process: None,
            role: None,
            after_merge: None,
            unsandboxed: None,
            earlier: vec![],
            unknown: Default::default(),
        }
    }

    struct Setup {
        root: tempfile::TempDir,
        repo: PathBuf,
        herdr: FakeHerdr,
        prs: Box<dyn PullRequests>,
        ledger: MemLedger,
        caller: Option<String>,
    }

    impl Setup {
        fn new() -> Setup {
            let root = tempfile::tempdir().unwrap();
            let repo = repo_with_origin(root.path());
            let herds = root.path().join("loopherd/herds");
            std::fs::create_dir_all(&herds).unwrap();
            std::fs::write(herds.join("prs.toml"), format!("repo = {repo:?}\n")).unwrap();
            Setup {
                root,
                repo,
                herdr: FakeHerdr::new(&[("w1", "loopherd")]),
                prs: Box::new(FakePrs(Ok(PrLookup::None))),
                ledger: MemLedger::default(),
                caller: None,
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
            let guard = Guard::new(
                Paths::under(&self.root.path().join("loopherd")),
                self.caller.clone(),
            );
            release(
                &self.herdr,
                self.prs.as_ref(),
                &mut self.ledger,
                &guard,
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
    fn refuses_an_entry_its_herd_file_disowns_or_a_lead_outside_its_herd_and_clears_nothing() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/x");
        s.ledger.0.get_mut("zpr-1").unwrap().repo = s.root.path().join("forged");

        let err = s.release("zpr-1", Fate::Drop).unwrap_err();

        assert!(
            err.to_string()
                .starts_with("refused: zpr-1's ledger entry names repo"),
            "{err}"
        );
        assert!(wt.exists());

        s.ledger.0.get_mut("zpr-1").unwrap().repo = s.repo.clone();
        let sandbox = s.root.path().join("loopherd/sandbox");
        std::fs::create_dir(&sandbox).unwrap();
        std::fs::write(
            sandbox.join("lead-other.lead.json"),
            r#"{"herd": "other", "args": []}"#,
        )
        .unwrap();
        s.caller = Some("lead-other".into());

        let err = s.release("zpr-1", Fate::Drop).unwrap_err();

        assert_eq!(
            err.to_string(),
            "refused: zpr-1 isn't a member of lead-other's herd, and lead-other runs this loopherd"
        );
        assert!(wt.exists());
        assert!(has_branch(&s.repo, "feat/x"));
    }

    #[test]
    fn refuses_a_branch_whose_worktree_would_be_a_leads_and_a_leads_drop_branch() {
        let mut s = Setup::new();
        s.retired_on("zpr-1", "feat/x");
        let sandbox = s.root.path().join("loopherd/sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(
            sandbox.join("lead-prs.lead.json"),
            r#"{"herd": "prs", "args": []}"#,
        )
        .unwrap();
        let lead_wt = s.root.path().join("proj/worktrees/lead-prs");
        let entry = s.ledger.0.get_mut("zpr-1").unwrap();
        entry.worktree = None;
        entry.branch = Some("lead-prs".into());

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "refused: zpr-1's ledger entry names lead lead-prs's worktree {}",
                lead_wt.display()
            )
        );

        s.ledger.0.get_mut("zpr-1").unwrap().branch = Some("feat/x".into());
        s.caller = Some("lead-prs".into());

        let err = s.release("zpr-1", Fate::Drop).unwrap_err();

        assert_eq!(
            err.to_string(),
            "refused: --drop-branch deletes commits, so it's the human's, not a lead's"
        );
        assert!(has_branch(&s.repo, "feat/x"));
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
        assert_eq!(released.outcome, Some(Outcome::Released));
        let (entries, _) = history(&s.mailboxes(), "zpr-1").unwrap();
        assert_eq!(
            entries.last().unwrap().body,
            Body::Note {
                text: "released: feed may spawn zpr-1 again".into()
            }
        );
    }

    #[test]
    fn releasing_overwrites_the_outcome_retire_recorded() {
        let mut s = Setup::new();
        let mut closed = record("zpr-1", s.repo.clone(), true);
        closed.outcome = Some(Outcome::Closed);
        s.add(closed);

        s.release("zpr-1", Fate::Clear).unwrap();

        assert_eq!(s.ledger.0["zpr-1"].outcome, Some(Outcome::Released));
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

        assert!(
            out.contains(&"zpr-1 is already released (since 2026-11-17T12:00:00Z)".into()),
            "{out:?}"
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
    fn refuses_a_member_whose_role_is_from_another_version() {
        let mut s = Setup::new();
        let mut unknown = record("lead-prs", s.repo.clone(), true);
        unknown.role = Some(Role::Unknown("reviewer".into()));
        s.add(unknown);

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
    fn refuses_while_its_pane_runs_an_agent_herdr_cant_identify() {
        let mut s = Setup::new();
        let pane = s.herdr.add_tab("w1", "zpr-1", &s.repo.clone(), true);
        s.herdr =
            std::mem::replace(&mut s.herdr, FakeHerdr::new(&[])).with_live_agent(None, None, &pane);
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.pane_id = pane.clone();
        s.add(r);

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("herdr tab close w1:t1  # zpr-1: "), "{msg}");
        assert!(
            msg.contains(&format!("herdr agent rename {pane} <label>")),
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
            out[1..],
            [
                format!("deleted branch feat/zpr-1 (was {})", &unpushed[..12]),
                "dropped 1 commit not on origin/main or another origin branch".into(),
                "released zpr-1: feed may spawn it again".into(),
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
            out[1..],
            [
                format!("deleted branch feat/zpr-1 (was {})", &pushed[..12]),
                "dropped 1 commit not on origin/main or another origin branch".into(),
                format!("deleted origin/feat/zpr-1 (was {})", &pushed[..12]),
                "released zpr-1: feed may spawn it again".into(),
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

    fn origin(s: &Setup) -> PathBuf {
        s.root.path().join("origin.git")
    }

    /// A retired `zpr-1` on `feat/zpr-1`, pushed with nothing new, so a
    /// plain release deletes it on origin.
    fn pushed_on_the_base(s: &mut Setup) -> PathBuf {
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        push_head(&wt, "feat/zpr-1");
        wt
    }

    fn pr_7() -> PrLookup {
        PrLookup::Open(Pr {
            number: 7,
            url: "https://github.com/o/r/pull/7".into(),
        })
    }

    #[test]
    fn refuses_to_delete_the_head_of_an_open_pr_and_names_it() {
        let mut s = Setup::new();
        let wt = pushed_on_the_base(&mut s);
        s.prs = Box::new(FakePrs(Ok(pr_7())));

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: origin/feat/zpr-1 is the head of open PR #7 \
             (https://github.com/o/r/pull/7); deleting it would close the PR. Pass \
             --keep-branch to start the new member from it, or --drop-branch to delete it anyway"
        );
        assert!(wt.exists());
        assert!(has_branch(&s.repo, "feat/zpr-1"));
        assert!(has_branch(&origin(&s), "feat/zpr-1"));
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    #[test]
    fn drop_branch_deletes_an_open_prs_head_and_names_the_pr() {
        let mut s = Setup::new();
        pushed_on_the_base(&mut s);
        s.prs = Box::new(FakePrs(Ok(pr_7())));

        let out = s.release("zpr-1", Fate::Drop).unwrap();

        assert_eq!(
            out[0],
            "origin/feat/zpr-1 is the head of open PR #7 (https://github.com/o/r/pull/7); \
             deleting it closes the PR"
        );
        assert!(!has_branch(&origin(&s), "feat/zpr-1"));
    }

    #[test]
    fn without_a_github_cli_or_account_it_says_so_and_goes_on() {
        let mut s = Setup::new();
        pushed_on_the_base(&mut s);
        s.prs = Box::new(FakePrs(Ok(PrLookup::Unknown(
            "no GitHub CLI (gh) on PATH".into(),
        ))));

        let out = s.release("zpr-1", Fate::Clear).unwrap();

        assert_eq!(
            out[0],
            "warning: can't check for an open PR on origin/feat/zpr-1: no GitHub CLI (gh) on PATH"
        );
        assert!(!has_branch(&origin(&s), "feat/zpr-1"));
    }

    #[test]
    fn refuses_when_github_fails_to_answer_and_changes_nothing() {
        let mut s = Setup::new();
        let wt = pushed_on_the_base(&mut s);
        s.prs = Box::new(FakePrs(Err("gh pr list: HTTP 502".into())));

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            "refused: can't ask GitHub for an open PR on feat/zpr-1: gh pr list: HTTP 502"
        );
        assert!(wt.exists());
        assert!(has_branch(&origin(&s), "feat/zpr-1"));
    }

    #[test]
    fn asks_github_only_when_it_would_delete_an_origin_branch() {
        let mut s = Setup::new();
        s.retired_on("zpr-1", "feat/zpr-1");
        s.prs = Box::new(FakePrs(Err("gh must not be asked".into())));

        s.release("zpr-1", Fate::Clear).unwrap();
    }

    #[test]
    fn a_failed_local_step_leaves_origin_and_the_ledger_untouched() {
        let mut s = Setup::new();
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        let sub = origin(&s);
        git_in(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                sub.to_str().unwrap(),
                "sub",
            ],
        );
        commit(&wt, "add a submodule");
        push_head(&wt, "feat/zpr-1");

        let err = s.release("zpr-1", Fate::Drop).unwrap_err();

        assert!(format!("{err:#}").contains("submodule"), "{err:#}");
        assert!(has_branch(&origin(&s), "feat/zpr-1"));
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    /// Answers no open PR, after pushing a new commit to origin's branch,
    /// as a member still running elsewhere might.
    struct MovesTheBranch(PathBuf);

    impl PullRequests for MovesTheBranch {
        fn open_on(&self, _repo: &Path, branch: &str) -> Result<PrLookup> {
            commit(&self.0, "pushed meanwhile");
            push_head(&self.0, branch);
            Ok(PrLookup::None)
        }
    }

    #[test]
    fn leaves_an_origin_branch_that_moved_since_it_was_counted_and_says_to_rerun() {
        let mut s = Setup::new();
        let wt = pushed_on_the_base(&mut s);
        let counted = head(&wt);
        let elsewhere = s.root.path().join("elsewhere");
        git_in(
            s.root.path(),
            &[
                "clone",
                "--quiet",
                "--branch",
                "feat/zpr-1",
                origin(&s).to_str().unwrap(),
                elsewhere.to_str().unwrap(),
            ],
        );
        s.prs = Box::new(MovesTheBranch(elsewhere.clone()));

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        assert!(
            format!("{err:#}")
                == format!(
                    "origin kept origin/feat/zpr-1: someone pushed to it since release counted \
                     it at {}; check who, or rerun with --keep-branch to start the new member \
                     from it",
                    &counted[..12]
                ),
            "{err:#}"
        );
        assert_eq!(head_of(&origin(&s), "feat/zpr-1"), head(&elsewhere));
        assert!(!wt.exists());
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    /// Installs a pre-push hook in the member's repo that prints `say` on
    /// stderr and `say (stdout)` on stdout, touches `ran`, and exits `code`.
    fn pre_push_hook(s: &Setup, say: &str, code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let hooks = s.root.path().join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-push");
        let ran = s.root.path().join("ran");
        let _ = std::fs::remove_file(&ran);
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\necho '{say}' >&2\necho '{say} (stdout)'\ntouch '{}'\nexit {code}\n",
                ran.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git_in(
            &s.repo,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        ran
    }

    #[test]
    fn a_pre_push_hook_that_refuses_keeps_the_origin_branch_and_says_why_after_the_rest() {
        let mut s = Setup::new();
        let wt = pushed_on_the_base(&mut s);
        pre_push_hook(&s, "no deletes on fridays", 1);

        let err = s.release("zpr-1", Fate::Clear).unwrap_err();

        let shown = format!("{err:#}");
        assert!(
            shown.starts_with(&format!(
                "couldn't delete origin/feat/zpr-1 (was {}); fix the cause and rerun the same \
                 release command to retry. zpr-1 stays unreleased, so feed won't spawn it, \
                 until that rerun deletes it: the push was refused, perhaps by a pre-push hook: ",
                &head(&s.repo)[..12]
            )),
            "{shown}"
        );
        assert!(shown.contains("no deletes on fridays\n"), "{shown}");
        assert!(
            shown.contains("stdout: no deletes on fridays (stdout)"),
            "{shown}"
        );
        assert!(has_branch(&origin(&s), "feat/zpr-1"));
        assert!(!wt.exists());
        assert!(!has_branch(&s.repo, "feat/zpr-1"));
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
    }

    /// The names feed would spawn now for task `1` of herd `prs` on `s.repo`.
    fn feed_would_spawn(s: &Setup) -> Vec<String> {
        let herds = s.root.path().join("herds");
        std::fs::create_dir_all(&herds).unwrap();
        std::fs::write(
            herds.join("prs.toml"),
            format!(
                "repo = {:?}\n[template.pr]\nparams = [\"n\"]\nname = \"zpr-{{n}}\"\n\
                 prompt = \"go\"\n[queue]\ntemplate = \"pr\"\nsource = \"true\"\nmax = 1\n",
                s.repo.display().to_string()
            ),
        )
        .unwrap();
        let paths = crate::herd::Paths {
            herds,
            home: PathBuf::from("/home/me"),
            feed_lock: s.root.path().join("feed.lock"),
            paused: s.root.path().join("paused"),
        };
        let queue = crate::herd::queue(&paths, "prs").unwrap();
        crate::feed::claimable(&s.ledger, &paths, &queue, "prs", 0, "1\n").unwrap()
    }

    /// A retired `zpr-1` whose `feat/zpr-1` holds a pushed commit not on the
    /// base, whose origin delete a pre-push hook refused under `--drop-branch`.
    /// Returns the dropped commit.
    fn refused_drop(s: &mut Setup) -> String {
        let wt = s.retired_on("zpr-1", "feat/zpr-1");
        commit(&wt, "to be dropped");
        push_head(&wt, "feat/zpr-1");
        let dropped = head(&wt);
        pre_push_hook(s, "no deletes on fridays", 1);
        s.release("zpr-1", Fate::Drop).unwrap_err();
        dropped
    }

    #[test]
    fn a_refused_origin_delete_leaves_the_name_unclaimable_until_a_rerun_deletes_it() {
        let mut s = Setup::new();

        let dropped = refused_drop(&mut s);

        assert_eq!(head_of(&origin(&s), "feat/zpr-1"), dropped);
        assert_eq!(feed_would_spawn(&s), Vec::<String>::new());
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);

        let ran = pre_push_hook(&s, "ok", 0);
        let out = s.release("zpr-1", Fate::Drop).unwrap();

        assert_eq!(
            out[out.len() - 2..],
            [
                format!("deleted origin/feat/zpr-1 (was {})", &dropped[..12]),
                "released zpr-1: feed may spawn it again".into(),
            ]
        );
        assert!(ran.exists());
        assert!(!has_branch(&origin(&s), "feat/zpr-1"));
        assert_eq!(feed_would_spawn(&s), ["zpr-1"]);
    }

    #[test]
    fn a_rerun_that_cant_ask_origin_refuses_while_the_leftover_branch_was_last_seen_there() {
        let mut s = Setup::new();
        refused_drop(&mut s);
        cut_off_origin(&s);

        let err = s.release("zpr-1", Fate::Drop).unwrap_err();

        let shown = format!("{err:#}");
        assert!(
            shown.starts_with("refused: can't ask origin for origin/feat/zpr-1 ("),
            "{shown}"
        );
        assert!(
            shown.ends_with(
                "), and it was there when last fetched; rerun once origin answers, or pass \
                 --keep-branch to start the new member from it"
            ),
            "{shown}"
        );
        assert_eq!(s.ledger.0["zpr-1"].released_at, None);
        git_in(
            &s.repo,
            &["remote", "set-url", "origin", origin(&s).to_str().unwrap()],
        );
        assert_eq!(feed_would_spawn(&s), Vec::<String>::new());
    }

    #[test]
    fn a_pre_push_hook_that_passes_lets_release_delete_the_origin_branch() {
        let mut s = Setup::new();
        pushed_on_the_base(&mut s);
        let ran = pre_push_hook(&s, "ok", 0);

        let out = s.release("zpr-1", Fate::Clear).unwrap();

        assert!(
            out[out.len() - 2].starts_with("deleted origin/feat/zpr-1 (was "),
            "{out:?}"
        );
        assert!(!has_branch(&origin(&s), "feat/zpr-1"));
        assert!(ran.exists());
    }

    fn cut_off_origin(s: &Setup) {
        git_in(
            &s.repo,
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );
    }

    #[test]
    fn keep_branch_works_offline() {
        let mut s = Setup::new();
        let wt = pushed_on_the_base(&mut s);
        commit(&wt, "unpushed");
        cut_off_origin(&s);

        let out = s.release("zpr-1", Fate::Keep).unwrap();

        assert_eq!(
            out[0],
            "kept its worktree, branch and origin/feat/zpr-1: the new member starts from them"
        );
    }

    #[test]
    fn with_nothing_to_clear_it_works_offline_and_says_what_it_couldnt_check() {
        let mut s = Setup::new();
        let mut r = record("zpr-1", s.repo.clone(), true);
        r.branch = Some("feat/zpr-1".into());
        s.add(r);
        cut_off_origin(&s);

        let out = s.release("zpr-1", Fate::Clear).unwrap();

        assert!(
            out[0].starts_with("warning: couldn't ask origin for origin/feat/zpr-1 ("),
            "{out:?}"
        );
        assert!(
            out[0].ends_with("; if it's there, rerun release once origin answers"),
            "{out:?}"
        );
        assert_eq!(out[1], "released zpr-1: feed may spawn it again");
    }
}
