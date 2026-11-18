//! Git worktrees for spawned agents.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::shell::run_within;

/// Where the worktree for `branch` lives: `<parent>/worktrees/<slug>` when the
/// repo sits at `<project>/<project>/`, otherwise `<repo>-worktrees/<slug>`.
pub fn worktree_path(repo: &Path, branch: &str) -> PathBuf {
    let slug = branch.replace('/', "-");
    let name = repo.file_name().unwrap_or_default();
    let parent = repo.parent().unwrap_or(Path::new("/"));
    if parent.file_name() == Some(name) {
        parent.join("worktrees").join(slug)
    } else {
        let mut dir = name.to_os_string();
        dir.push("-worktrees");
        parent.join(dir).join(slug)
    }
}

/// Where `ensure_worktree` would put `branch`, without touching git state.
pub fn planned_worktree(repo: &Path, branch: &str) -> Result<PathBuf> {
    Ok(find_worktree(repo, branch)?.unwrap_or_else(|| worktree_path(repo, branch)))
}

/// The main checkout of the repo containing `dir`, even from a linked worktree.
pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    let common = git_common_dir(dir)
        .with_context(|| format!("{} is not in a git repo; pass --repo", dir.display()))?;
    checkout_above(&common)
}

/// [`repo_root`] failing with git's own reason and no hint.
pub fn checkout_root(dir: &Path) -> Result<PathBuf> {
    checkout_above(&git_common_dir(dir)?)
}

fn git_common_dir(dir: &Path) -> Result<PathBuf> {
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(common.trim()))
}

fn checkout_above(common: &Path) -> Result<PathBuf> {
    let root = common
        .parent()
        .with_context(|| format!("no checkout above {}", common.display()))?;
    root.canonicalize()
        .with_context(|| format!("repo: {}", root.display()))
}

/// Returns a worktree checked out on `branch`, reusing an existing one.
///
/// A branch on origin is checked out; otherwise the branch is created from
/// origin's default branch.
pub fn ensure_worktree(repo: &Path, branch: &str) -> Result<PathBuf> {
    if let Some(existing) = find_worktree(repo, branch)? {
        return Ok(existing);
    }
    fetch(repo)?;
    let path = worktree_path(repo, branch);
    let path_str = path.to_str().context("worktree path is not UTF-8")?;
    let remote = format!("origin/{branch}");
    if has_ref(repo, &format!("refs/heads/{branch}")) {
        git(repo, &["worktree", "add", "--quiet", path_str, branch])?;
    } else if has_ref(repo, &format!("refs/remotes/{remote}")) {
        git(
            repo,
            &[
                "worktree", "add", "--quiet", "--track", "-b", branch, path_str, &remote,
            ],
        )?;
    } else {
        let base = default_branch(repo)?;
        git(
            repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--no-track",
                "-b",
                branch,
                path_str,
                &base,
            ],
        )?;
    }
    Ok(path)
}

/// What [`remove_if_merged`] did with a worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    Removed,
    Kept(String),
}

/// Removes the linked worktree at `path` when it's clean, with no ignored
/// files outside build directories, no rebase, merge or bisect in progress, and
/// either detached or merged into origin's default branch, then
/// deletes its branch. Never forces.
pub fn remove_if_merged(repo: &Path, path: &Path) -> Result<Removal> {
    let path_str = path.to_str().context("worktree path is not UTF-8")?;
    if !is_linked_worktree(repo, path)? {
        let reason = format!("not a linked worktree of {}", repo.display());
        return Ok(Removal::Kept(reason));
    }
    if let Some(op) = operation_in_progress(path)? {
        return Ok(Removal::Kept(format!("{op} in progress")));
    }
    if let Some(reason) = unclean(path)? {
        return Ok(Removal::Kept(reason));
    }
    let head = git(path, &["rev-parse", "HEAD"])?.trim().to_string();
    let branch = checked_out_branch(path)?;
    if let Some(branch) = &branch
        && let Some(reason) = unmerged(repo, branch, &head)?
    {
        return Ok(Removal::Kept(reason));
    }
    git(repo, &["worktree", "remove", path_str])?;
    if let Some(branch) = &branch {
        delete_branch(repo, branch, &head)?;
    }
    Ok(Removal::Removed)
}

/// What [`remove_if_gone`] did with a worktree whose directory is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gone {
    /// git doesn't list it either.
    Unlisted,
    /// Its git record was removed; `kept_branch` names a branch left in place
    /// and why.
    Removed {
        kept_branch: Option<(String, String)>,
    },
    /// The directory couldn't be checked, so it was left alone.
    Unchecked(String),
    /// git has it locked, so its record and branch were left alone.
    Locked { reason: Option<String> },
}

/// When the directory at `path` is missing, removes `repo`'s record of it and
/// deletes its branch if merged into origin's default branch. `None` when the
/// directory is there.
pub fn remove_if_gone(repo: &Path, path: &Path) -> Result<Option<Gone>> {
    match path.symlink_metadata() {
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Ok(Some(Gone::Unchecked(format!("cannot check it: {e}")))),
    }
    let Some(entry) = listed_worktree(repo, path)? else {
        return Ok(Some(Gone::Unlisted));
    };
    if let Some(reason) = entry.locked {
        return Ok(Some(Gone::Locked { reason }));
    }
    // Decide on the branch first: a failed fetch must leave the record for a rerun.
    let kept_branch = match &entry.branch {
        Some(branch) => unmerged(repo, branch, &entry.head)?.map(|r| (branch.clone(), r)),
        None => None,
    };
    // Unlike `worktree prune`, this touches only this record and refuses a locked one.
    git(repo, &["worktree", "remove", &entry.path])?;
    if let (Some(branch), None) = (&entry.branch, &kept_branch) {
        delete_branch(repo, branch, &entry.head)?;
    }
    Ok(Some(Gone::Removed { kept_branch }))
}

/// What [`plan_clear`] does with a branch, and worktrees, a member leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// Remove them, unless they hold commits not on the base.
    Clear,
    /// Leave them for the next member to start from.
    Keep,
    /// Remove them even with commits not on the base.
    Drop,
}

/// What [`plan_clear`] would do with a member's checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// There is no worktree, local branch or origin branch to clear.
    Absent,
    /// There is nothing local, no tracking ref says origin has the branch,
    /// and origin couldn't be asked, for this reason.
    Unchecked(String),
    /// Leave them, as [`Fate::Keep`] asked: a worktree or local branch
    /// when `local`, origin's branch (as last fetched) when `remote`.
    Kept { local: bool, remote: bool },
    /// Remove them, local steps first: see [`Clearing`].
    Clear(Clearing),
    /// Change nothing: they hold `count` commits not on `base`.
    Ahead { count: usize, base: String },
    /// Change nothing: a worktree has work that isn't committed.
    Dirty(String),
    /// Change nothing: something else is on it or in the way.
    Taken(String),
}

/// The worktrees, local branch and origin branch [`plan_clear`] found to
/// remove, each at the head it counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clearing {
    repo: PathBuf,
    branch: Option<String>,
    pub worktrees: Vec<(PathBuf, String)>,
    pub head: Option<String>,
    pub remote: Option<String>,
    /// Commits on neither `base` nor any other origin branch.
    pub dropped: usize,
    pub base: String,
}

impl Clearing {
    /// Removes the worktrees, then the local branch, leaving origin alone.
    pub fn remove_local(&self) -> Result<()> {
        for (path, _) in &self.worktrees {
            let path = path.to_str().context("worktree path is not UTF-8")?;
            git(&self.repo, &["worktree", "remove", path])?;
        }
        if let (Some(branch), Some(head)) = (&self.branch, &self.head) {
            delete_branch(&self.repo, branch, head)?;
        }
        Ok(())
    }

    /// Deletes origin's branch, unless it moved past [`Clearing::remote`].
    pub fn delete_remote(&self) -> Result<RemoteDelete> {
        let (Some(branch), Some(head)) = (&self.branch, &self.remote) else {
            return Ok(RemoteDelete::Deleted);
        };
        let lease = format!("--force-with-lease=refs/heads/{branch}:{head}");
        let refspec = format!(":refs/heads/{branch}");
        // No --no-verify: the repo's pre-push hooks get a say in deletes too.
        match remote_git(&self.repo, &["push", "--quiet", &lease, "origin", &refspec]) {
            Ok(_) => Ok(RemoteDelete::Deleted),
            Err(e) if lease_refused(&e) => Ok(RemoteDelete::Moved),
            Err(e) if push_refused(&e) => {
                Err(e.context("the push was refused, perhaps by a pre-push hook"))
            }
            Err(e) if e.is::<TimedOut>() => {
                Err(e.context("origin didn't answer, or a pre-push hook took too long"))
            }
            Err(e) => Err(e),
        }
    }
}

/// What [`Clearing::delete_remote`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDelete {
    Deleted,
    /// Origin kept the branch: it moved since it was counted, so the lease refused.
    Moved,
}

/// Whether git got as far as refusing the ref update, as a pre-push hook or
/// origin's own rules do, rather than failing to reach origin.
fn push_refused(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RemoteFailed>()
        .is_some_and(|failed| failed.message.contains("failed to push some refs"))
}

fn lease_refused(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RemoteFailed>()
        .is_some_and(|failed| failed.message.contains("(stale info)"))
}

/// Plans clearing the worktree at `worktree`, the one at the path
/// [`ensure_worktree`] makes for `branch`, the local `branch`, and origin's
/// `branch`, per `fate`, so [`ensure_worktree`] next makes them afresh,
/// changing nothing. `in_use` names the live member using a worktree. Never
/// plans removing a worktree with changes, mid-operation or locked, nor
/// touching the main checkout, one in use, another worktree on the branch,
/// or origin's default branch, whatever the fate. [`Fate::Keep`] never asks origin.
pub fn plan_clear(
    repo: &Path,
    worktree: Option<&Path>,
    branch: Option<&str>,
    fate: Fate,
    in_use: &dyn Fn(&Path) -> Option<String>,
) -> Result<Plan> {
    let branch_head = match branch {
        Some(b) => git(repo, &["rev-parse", "--verify", &format!("refs/heads/{b}")])
            .ok()
            .map(|h| h.trim().to_string()),
        None => None,
    };
    let spawn_path = branch.map(|b| worktree_path(repo, b));
    let ours = |path: &Path| {
        worktree
            .into_iter()
            .chain(spawn_path.as_deref())
            .any(|p| same_dir(p, path))
    };
    if let Some(b) = branch
        && let Some(path) = find_worktree(repo, b)?
        && !ours(&path)
    {
        let shown = path.display();
        return Ok(Plan::Taken(match listed_worktree(repo, &path)? {
            None => format!("{b} is checked out in the main checkout {shown}"),
            Some(_) => format!("{b} is checked out in {shown}, a worktree loopherd didn't make"),
        }));
    }
    let mut worktrees: Vec<ListedWorktree> = vec![];
    for path in worktree.iter().copied().chain(spawn_path.as_deref()) {
        if worktrees.iter().any(|w| same_dir(Path::new(&w.path), path)) {
            continue;
        }
        let Some(listed) = listed_worktree(repo, path)? else {
            let in_the_way = std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_some());
            if Some(path) == spawn_path.as_deref() && in_the_way {
                return Ok(Plan::Taken(format!(
                    "{} is in the way and isn't a worktree of {}",
                    path.display(),
                    repo.display()
                )));
            }
            continue;
        };
        if let Some(user) = in_use(path) {
            return Ok(Plan::Taken(format!(
                "{} is in use by {user}",
                path.display()
            )));
        }
        worktrees.push(listed);
    }
    let local = !worktrees.is_empty() || branch_head.is_some();
    if fate == Fate::Keep {
        let elsewhere = worktrees
            .iter()
            .find(|w| w.branch.as_deref() != branch && Path::new(&w.path).exists());
        if let (Some(b), Some(w)) = (branch, elsewhere) {
            return Ok(Plan::Taken(format!(
                "{} isn't on {b}, so the new member can't start there; \
                 rerun without --keep-branch to clear it",
                w.path
            )));
        }
        // ensure_worktree fetches, then starts from this tracking ref; a fetch never prunes
        // it, so it already tells whether there's one to start from.
        let remote = branch.is_some_and(|b| has_ref(repo, &format!("refs/remotes/origin/{b}")));
        return Ok(match local || remote {
            true => Plan::Kept { local, remote },
            false => Plan::Absent,
        });
    }
    let remote_head = match (branch, local) {
        (None, _) => None,
        (Some(b), true) => remote_branch_head(repo, b)?,
        (Some(b), false) => match remote_branch_head(repo, b) {
            Ok(head) => head,
            // A refused delete leaves this tracking ref, so its branch may still be on origin.
            Err(e) if has_ref(repo, &format!("refs/remotes/origin/{b}")) => {
                return Ok(Plan::Taken(format!(
                    "can't ask origin for origin/{b} ({e:#}), and it was there when last \
                     fetched; rerun once origin answers, or pass --keep-branch to start the \
                     new member from it"
                )));
            }
            Err(e) => return Ok(Plan::Unchecked(format!("{e:#}"))),
        },
    };
    if !local && remote_head.is_none() {
        return Ok(Plan::Absent);
    }
    for w in &worktrees {
        let path = Path::new(&w.path);
        if let Some(reason) = &w.locked {
            let why = reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            return Ok(Plan::Taken(format!("{} is locked{why}", w.path)));
        }
        if !path.exists() {
            continue;
        }
        if let Some(op) = operation_in_progress(path)? {
            return Ok(Plan::Dirty(format!("{} has a {op} in progress", w.path)));
        }
        if let Some(reason) = unclean(path)? {
            return Ok(Plan::Dirty(format!("{} has {reason}", w.path)));
        }
    }
    let heads: Vec<&str> = worktrees
        .iter()
        .map(|w| w.head.as_str())
        .chain(branch_head.as_deref())
        .chain(remote_head.as_deref())
        .filter(|h| !h.is_empty())
        .collect();
    fetch(repo)?;
    let base = default_branch(repo)?;
    if let Some(b) = branch
        && base.strip_prefix("origin/") == Some(b)
    {
        return Ok(Plan::Taken(format!(
            "{b} is origin's default branch, which release never deletes; \
             pass --keep-branch to release it without clearing"
        )));
    }
    let count = |not: &[&str]| -> Result<usize> {
        let mut args = vec!["rev-list", "--count"];
        args.extend(&heads);
        args.push("--not");
        args.extend(not);
        git(repo, &args)?
            .trim()
            .parse()
            .context("git rev-list --count")
    };
    let ahead = if heads.is_empty() {
        0
    } else {
        count(&[&base])?
    };
    if ahead > 0 && fate == Fate::Clear {
        return Ok(Plan::Ahead { count: ahead, base });
    }
    let dropped = match (ahead, branch) {
        (0, _) => 0,
        (_, None) => ahead,
        (_, Some(b)) => {
            let exclude = format!("--exclude=origin/{b}");
            count(&[&base, &exclude, "--remotes=origin"])?
        }
    };
    Ok(Plan::Clear(Clearing {
        repo: repo.to_path_buf(),
        branch: branch.map(str::to_string),
        worktrees: worktrees
            .into_iter()
            .map(|w| (PathBuf::from(w.path), w.head))
            .collect(),
        head: branch_head,
        remote: remote_head,
        dropped,
        base,
    }))
}

/// The head of `branch` on origin, fetched into origin/`branch`, if origin
/// has it. Asks origin directly, since a plain fetch never prunes a
/// tracking ref whose branch was deleted.
fn remote_branch_head(repo: &Path, branch: &str) -> Result<Option<String>> {
    let name = format!("refs/heads/{branch}");
    let listed = remote_git(repo, &["ls-remote", "--heads", "origin", &name])?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    let tracking = format!("refs/remotes/origin/{branch}");
    remote_git(
        repo,
        &["fetch", "--quiet", "origin", &format!("+{name}:{tracking}")],
    )?;
    Ok(Some(
        git(repo, &["rev-parse", &tracking])?.trim().to_string(),
    ))
}

/// Why `branch` at `head` isn't merged into origin's default branch, if it isn't.
fn unmerged(repo: &Path, branch: &str, head: &str) -> Result<Option<String>> {
    fetch(repo)?;
    let base = default_branch(repo)?;
    Ok((!is_ancestor(repo, head, &base)?).then(|| format!("{branch} is not merged into {base}")))
}

fn delete_branch(repo: &Path, branch: &str, head: &str) -> Result<()> {
    // `branch -d` judges against the checkout's HEAD, which may lag origin;
    // callers check merging, and the old value guards the delete.
    git(
        repo,
        &["update-ref", "-d", &format!("refs/heads/{branch}"), head],
    )?;
    Ok(())
}

struct ListedWorktree {
    path: String,
    head: String,
    branch: Option<String>,
    /// `Some` when locked, holding git's reason if it has one.
    locked: Option<Option<String>>,
}

/// `repo`'s linked worktree entry at `path`, which may no longer exist.
fn listed_worktree(repo: &Path, path: &Path) -> Result<Option<ListedWorktree>> {
    let list = git(repo, &["worktree", "list", "--porcelain"])?;
    for block in list.split("\n\n").skip(1) {
        let mut lines = block.lines();
        let Some(listed) = lines.next().and_then(|l| l.strip_prefix("worktree ")) else {
            continue;
        };
        if !same_dir(Path::new(listed), path) {
            continue;
        }
        let mut entry = ListedWorktree {
            path: listed.into(),
            head: String::new(),
            branch: None,
            locked: None,
        };
        for line in lines {
            if let Some(head) = line.strip_prefix("HEAD ") {
                entry.head = head.into();
            } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                entry.branch = Some(branch.into());
            } else if line == "locked" {
                entry.locked = Some(None);
            } else if let Some(reason) = line.strip_prefix("locked ") {
                entry.locked = Some(Some(reason.into()));
            }
        }
        return Ok(Some(entry));
    }
    Ok(None)
}

/// Whether `a` and `b` name the same directory, even when it no longer exists
/// and the two reach it through different symlinked parents.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => with_real_parent(a) == with_real_parent(b),
    }
}

fn with_real_parent(path: &Path) -> PathBuf {
    match (path.parent().map(Path::canonicalize), path.file_name()) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

/// Markers in a worktree's git dir that git leaves while an operation is
/// stopped part way.
const IN_PROGRESS: [(&str, &str); 8] = [
    ("rebase-merge", "rebase"),
    // git am also uses rebase-apply, marking itself with this file.
    ("rebase-apply/applying", "am"),
    ("rebase-apply", "rebase"),
    ("MERGE_HEAD", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
    // Outlives the *_HEAD files between steps of a multi-commit pick or revert.
    ("sequencer", "cherry-pick or revert"),
    ("BISECT_LOG", "bisect"),
];

fn operation_in_progress(path: &Path) -> Result<Option<&'static str>> {
    let git_dir = PathBuf::from(git(path, &["rev-parse", "--absolute-git-dir"])?.trim());
    Ok(IN_PROGRESS
        .iter()
        .find(|(marker, _)| git_dir.join(marker).exists())
        .map(|(_, op)| *op))
}

/// Ignored directories that are regenerated, so removing them loses nothing.
const BUILD_DIRS: [&str; 2] = ["target", "node_modules"];

/// Why the worktree at `path` isn't clean: changes, or an ignored file outside
/// [`BUILD_DIRS`].
fn unclean(path: &Path) -> Result<Option<String>> {
    // -z leaves paths unquoted. Rename entries carry a second path, but any
    // change returns before that matters.
    let status = git(path, &["status", "--porcelain", "-z", "--ignored=matching"])?;
    for entry in status.split('\0').filter(|e| !e.is_empty()) {
        let Some(ignored) = entry.strip_prefix("!! ") else {
            return Ok(Some("uncommitted or untracked changes".into()));
        };
        if !in_build_dir(ignored) {
            return Ok(Some(format!("ignored file {ignored}")));
        }
    }
    Ok(None)
}

/// `entry` is a path git reports, with a trailing `/` for a directory.
fn in_build_dir(entry: &str) -> bool {
    let (dirs, _file) = entry.rsplit_once('/').unwrap_or(("", entry));
    dirs.split('/').any(|dir| BUILD_DIRS.contains(&dir))
}

/// `path` is one of `repo`'s worktrees other than its main checkout.
fn is_linked_worktree(repo: &Path, path: &Path) -> Result<bool> {
    let Ok(path) = path.canonicalize() else {
        return Ok(false);
    };
    let list = git(repo, &["worktree", "list", "--porcelain"])?;
    Ok(list
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .skip(1)
        .any(|linked| Path::new(linked).canonicalize().ok().as_ref() == Some(&path)))
}

/// The branch checked out at `path`, or `None` when HEAD is detached.
fn checked_out_branch(path: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .context("run git")?;
    match out.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )),
        // --quiet reports "not a symbolic ref" as exit 1 and nothing else.
        Some(1) => Ok(None),
        _ => bail!(
            "git symbolic-ref HEAD: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

fn is_ancestor(repo: &Path, commit: &str, of: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", commit, of])
        .status()
        .context("run git")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base --is-ancestor {commit} {of} failed"),
    }
}

/// Returns a worktree at `slug`'s path detached at origin's default branch,
/// reusing a worktree already there. Creates no local branch.
pub fn ensure_detached_worktree(repo: &Path, slug: &str) -> Result<PathBuf> {
    let path = worktree_path(repo, slug);
    if is_worktree(repo, &path)? {
        return Ok(path);
    }
    fetch(repo)?;
    let base = default_branch(repo)?;
    let path_str = path.to_str().context("worktree path is not UTF-8")?;
    git(
        repo,
        &["worktree", "add", "--quiet", "--detach", path_str, &base],
    )?;
    Ok(path)
}

fn is_worktree(repo: &Path, path: &Path) -> Result<bool> {
    let Ok(wanted) = path.canonicalize() else {
        return Ok(false);
    };
    let list = git(repo, &["worktree", "list", "--porcelain"])?;
    Ok(list
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|p| Path::new(p).canonicalize().is_ok_and(|p| p == wanted)))
}

fn find_worktree(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let list = git(repo, &["worktree", "list", "--porcelain"])?;
    let wanted = format!("branch refs/heads/{branch}");
    let mut current = None;
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
        } else if line == wanted {
            return Ok(current);
        }
    }
    Ok(None)
}

/// Origin's default branch as `origin/<branch>`, warning once per run when it
/// falls back or the local origin/HEAD is stale.
pub fn default_branch(repo: &Path) -> Result<String> {
    let (branch, warning) = default_branch_and_warning(repo)?;
    if let Some(warning) = warning {
        warn_once(warning);
    }
    Ok(branch)
}

/// Prints `warning` to stderr unless this run already has.
fn warn_once(warning: String) {
    static SHOWN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let mut shown = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    if !shown.contains(&warning) {
        eprintln!("{warning}");
        shown.insert(warning);
    }
}

/// Origin's default branch as the remote reports it, falling back to the local
/// origin/HEAD, which git never updates after the clone.
fn default_branch_and_warning(repo: &Path) -> Result<(String, Option<String>)> {
    let remote = match remote_head(repo)? {
        RemoteHead::Branch(remote) => remote,
        RemoteHead::NoBranch => {
            let local = local_default_branch(repo)?;
            let warning = format!(
                "warning: origin's HEAD names no branch (detached or unborn); \
                 using the local origin/HEAD, {local}"
            );
            return Ok((local, Some(warning)));
        }
        RemoteHead::Unreachable { error, hint } => {
            let local = local_default_branch(repo)?;
            let mut warning = format!(
                "warning: couldn't ask origin for its default branch ({error}); \
                 using the local origin/HEAD, {local}"
            );
            if let Some(hint) = hint {
                warning = format!("{warning}\n{hint}");
            }
            return Ok((local, Some(warning)));
        }
    };
    require_fetched(repo, &remote)?;
    let local = git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    );
    let warning = local
        .ok()
        .map(|local| local.trim().to_string())
        .filter(|local| *local != remote)
        .map(|local| {
            format!(
                "warning: origin's default branch is {remote}, but the local \
                 origin/HEAD points at {local}; fix it with \
                 `git -C {} remote set-head origin --auto`",
                repo.display()
            )
        });
    Ok((remote, warning))
}

/// Errors unless the clone's fetch refspec brought `remote` down.
fn require_fetched(repo: &Path, remote: &str) -> Result<()> {
    if has_ref(repo, &format!("refs/remotes/{remote}")) {
        return Ok(());
    }
    let branch = remote.strip_prefix("origin/").unwrap_or(remote);
    let refspecs = git(repo, &["config", "--get-all", "remote.origin.fetch"]).unwrap_or_default();
    let refspecs = refspecs.lines().collect::<Vec<_>>().join(", ");
    bail!(
        "origin's default branch is {branch}, but this clone's fetch refspec \
         ({refspecs}) doesn't fetch it; add it with \
         `git -C {} remote set-branches --add origin {branch}`",
        repo.display()
    )
}

enum RemoteHead {
    Branch(String),
    /// Detached or unborn.
    NoBranch,
    Unreachable {
        /// The first line of why ls-remote failed.
        error: String,
        hint: Option<&'static str>,
    },
}

fn remote_head(repo: &Path) -> Result<RemoteHead> {
    match remote_git(repo, &["ls-remote", "--symref", "origin", "HEAD"]) {
        Ok(out) => Ok(parse_head_symref(&out).map_or(RemoteHead::NoBranch, RemoteHead::Branch)),
        Err(e) if e.is::<TimedOut>() => Err(e),
        Err(e) => Ok(RemoteHead::Unreachable {
            error: e.to_string().lines().next().unwrap_or_default().to_string(),
            hint: e.downcast_ref::<RemoteFailed>().and_then(|f| f.hint),
        }),
    }
}

/// The `origin/<branch>` that `ls-remote --symref` output names as HEAD.
fn parse_head_symref(out: &str) -> Option<String> {
    out.lines()
        .filter_map(|l| l.strip_prefix("ref: refs/heads/")?.split_once('\t'))
        .find(|(_, name)| *name == "HEAD")
        .map(|(branch, _)| format!("origin/{branch}"))
}

fn local_default_branch(repo: &Path) -> Result<String> {
    let head = git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .or_else(|_| {
        remote_git(repo, &["remote", "set-head", "origin", "--auto"])?;
        git(
            repo,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
    })
    .context("find origin's default branch")?;
    Ok(head.trim().to_string())
}

fn has_ref(repo: &Path, name: &str) -> bool {
    git(repo, &["show-ref", "--verify", "--quiet", name]).is_ok()
}

/// Overrides [`REMOTE_TIMEOUT`], in seconds.
const TIMEOUT_VAR: &str = "LOOPHERD_GIT_TIMEOUT";

/// How long a call to origin may take before loopherd gives up on it.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(120);

/// The most [`TIMEOUT_VAR`] may ask for. It keeps the deadline well inside
/// what `Instant` can hold, so computing it can't overflow.
const MAX_REMOTE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Fetches origin, retrying once if another fetch in the repo won the race
/// to update a ref; the retry finds that ref current.
fn fetch(repo: &Path) -> Result<()> {
    let args = ["fetch", "--quiet", "origin"];
    match remote_git(repo, &args) {
        Err(e) if lost_ref_lock_race(&e) => remote_git(repo, &args)?,
        result => result?,
    };
    Ok(())
}

fn lost_ref_lock_race(err: &anyhow::Error) -> bool {
    err.downcast_ref::<RemoteFailed>()
        .is_some_and(|failed| failed.message.contains("cannot lock ref"))
}

fn remote_git(repo: &Path, args: &[&str]) -> Result<String> {
    remote_git_within(repo, args, remote_timeout()?)
}

/// How long loopherd lets git, a queue's source or a done check run.
pub fn remote_timeout() -> Result<Duration> {
    let Some(secs) = std::env::var_os(TIMEOUT_VAR) else {
        return Ok(REMOTE_TIMEOUT);
    };
    let secs = secs.to_string_lossy();
    match secs.parse().map(Duration::from_secs) {
        Ok(t) if !t.is_zero() && t <= MAX_REMOTE_TIMEOUT => Ok(t),
        _ => bail!(
            "{TIMEOUT_VAR} must be a whole number of seconds from 1 to {}, not '{secs}'",
            MAX_REMOTE_TIMEOUT.as_secs()
        ),
    }
}

/// A remote call that didn't finish in time.
#[derive(Debug)]
struct TimedOut(String);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TimedOut {}

/// Runs git against origin with no credential prompt, killing it and its
/// helpers after `timeout`.
fn remote_git_within(repo: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Untranslated, so the messages matched on below read the same everywhere.
        .env("LC_ALL", "C");
    let batch_mode = !has_own_ssh_setting(repo);
    if batch_mode {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let Some(out) = run_within(&mut command, timeout).context("run git")? else {
        return Err(TimedOut(format!(
            "git {}: origin didn't answer within {}; \
             check your credentials, ssh agent and network, or raise the \
             limit with {TIMEOUT_VAR}=<seconds>",
            args.join(" "),
            humantime::format_duration(timeout)
        ))
        .into());
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        let hint = (batch_mode && ssh_refused(stderr)).then_some(SSH_PROMPT_HINT);
        let mut message = format!("git {}: {}", args.join(" "), last_lines(stderr));
        // A pre-push hook's stdout lands on git's stdout.
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            message = format!("{message}\nstdout: {}", last_lines(stdout.trim()));
        }
        return Err(RemoteFailed { message, hint }.into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The most lines of a failed git call's output an error keeps, so a hook
/// that runs a test suite doesn't flood it.
const KEPT_LINES: usize = 20;

fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let cut = lines.len().saturating_sub(KEPT_LINES);
    let kept = lines[cut..].join("\n");
    match cut {
        0 => kept,
        n => format!("[{n} earlier lines cut]\n{kept}"),
    }
}

const SSH_PROMPT_HINT: &str = "hint: if ssh needed a passphrase or host-key prompt, set \
                               SSH_ASKPASS_REQUIRE=prefer (with SSH_ASKPASS and a display) \
                               to allow a GUI one";

/// Whether git's stderr looks like an ssh auth or host-key failure, which a
/// blocked prompt can cause.
fn ssh_refused(stderr: &str) -> bool {
    [
        "Permission denied",
        "Host key verification failed",
        "passphrase",
    ]
    .iter()
    .any(|sign| stderr.contains(sign))
}

/// A remote call that ran and failed.
#[derive(Debug)]
struct RemoteFailed {
    message: String,
    hint: Option<&'static str>,
}

impl std::fmt::Display for RemoteFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        if let Some(hint) = self.hint {
            write!(f, "\n{hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RemoteFailed {}

/// Whether the user chose how git runs ssh or how ssh prompts, which BatchMode
/// must not override.
fn has_own_ssh_setting(repo: &Path) -> bool {
    ["GIT_SSH_COMMAND", "GIT_SSH"]
        .iter()
        .any(|var| std::env::var_os(var).is_some())
        || askpass_will_prompt()
        || !core_ssh_command_unset(repo)
}

/// Whether ssh will ask through an askpass program rather than the terminal.
/// ssh matches `SSH_ASKPASS_REQUIRE` case-insensitively but untrimmed.
fn askpass_will_prompt() -> bool {
    let Some(require) = std::env::var_os("SSH_ASKPASS_REQUIRE") else {
        return false;
    };
    let require = require.to_string_lossy();
    if require.eq_ignore_ascii_case("force") {
        return true;
    }
    // Without a display, prefer falls back to the terminal.
    require.eq_ignore_ascii_case("prefer")
        && non_empty_var("SSH_ASKPASS")
        && (non_empty_var("DISPLAY") || non_empty_var("WAYLAND_DISPLAY"))
}

fn non_empty_var(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// Only a clean "not set" counts: a config git can't read may hold one.
fn core_ssh_command_unset(repo: &Path) -> bool {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "--get", "core.sshCommand"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(status, Ok(s) if s.code() == Some(1))
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// A repo at `<root>/proj/proj` cloned from a bare origin with `main` and
    /// `feature/existing` branches.
    pub fn repo_with_origin(root: &Path) -> PathBuf {
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        let repo = root.join("proj/proj");
        run(
            root,
            &["init", "--quiet", "--bare", "-b", "main", path(&origin)],
        );
        run(root, &["init", "--quiet", "-b", "main", path(&seed)]);
        run(
            &seed,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "root",
            ],
        );
        run(&seed, &["branch", "feature/existing"]);
        run(
            &seed,
            &["push", "--quiet", path(&origin), "main", "feature/existing"],
        );
        std::fs::create_dir_all(repo.parent().unwrap()).unwrap();
        run(root, &["clone", "--quiet", path(&origin), path(&repo)]);
        repo
    }

    /// Points the bare origin of [`repo_with_origin`] at a new default
    /// `branch`, one commit past `main`, leaving the clone's origin/HEAD stale.
    pub fn move_origin_default(root: &Path, branch: &str) {
        let seed = root.join("seed");
        run(&seed, &["checkout", "--quiet", "-b", branch]);
        commit(&seed, "on the new default");
        run(
            &seed,
            &["push", "--quiet", path(&root.join("origin.git")), branch],
        );
        let head = format!("refs/heads/{branch}");
        run(&root.join("origin.git"), &["symbolic-ref", "HEAD", &head]);
    }

    /// Points the bare origin's HEAD straight at `main`'s commit.
    pub fn detach_origin_head(root: &Path) {
        let origin = root.join("origin.git");
        let main = head_of(&origin, "main");
        run(&origin, &["update-ref", "--no-deref", "HEAD", &main]);
    }

    /// Points the bare origin's HEAD at a branch that doesn't exist.
    pub fn unborn_origin_head(root: &Path) {
        let origin = root.join("origin.git");
        run(&origin, &["symbolic-ref", "HEAD", "refs/heads/gone"]);
    }

    /// Like [`repo_with_origin`], but a `--single-branch` clone of `main`.
    pub fn single_branch_repo(root: &Path) -> PathBuf {
        let full = repo_with_origin(root);
        let repo = root.join("narrow/narrow");
        std::fs::create_dir_all(repo.parent().unwrap()).unwrap();
        let origin = root.join("origin.git");
        run(
            root,
            &[
                "clone",
                "--quiet",
                "--single-branch",
                path(&origin),
                path(&repo),
            ],
        );
        std::fs::remove_dir_all(full).unwrap();
        repo
    }

    fn path(p: &Path) -> &str {
        p.to_str().unwrap()
    }

    fn run(dir: &Path, args: &[&str]) {
        git(dir, args).unwrap();
    }

    pub fn git_in(dir: &Path, args: &[&str]) {
        run(dir, args);
    }

    pub fn current_branch(dir: &Path) -> String {
        git(dir, &["branch", "--show-current"])
            .unwrap()
            .trim()
            .to_string()
    }

    pub fn commit(dir: &Path, message: &str) {
        run(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                message,
            ],
        );
    }

    /// Pushes `dir`'s HEAD to origin's `branch`, as a merged PR would land.
    pub fn push_head(dir: &Path, branch: &str) {
        run(
            dir,
            &["push", "--quiet", "origin", &format!("HEAD:{branch}")],
        );
    }

    /// Fast-forwards origin's `main` to its `branch` from a separate clone, as
    /// a PR merged on the forge would, leaving the repo's `origin/main` stale.
    pub fn merge_on_origin(root: &Path, branch: &str) {
        let other = root.join("other-clone");
        let origin = root.join("origin.git");
        run(root, &["clone", "--quiet", path(&origin), path(&other)]);
        let refspec = format!("origin/{branch}:main");
        run(&other, &["push", "--quiet", "origin", &refspec]);
    }

    pub fn has_branch(repo: &Path, branch: &str) -> bool {
        has_ref(repo, &format!("refs/heads/{branch}"))
    }

    pub fn head(dir: &Path) -> String {
        head_of(dir, "HEAD")
    }

    pub fn head_of(dir: &Path, rev: &str) -> String {
        git(dir, &["rev-parse", rev]).unwrap().trim().to_string()
    }

    pub fn branches(repo: &Path) -> String {
        git(repo, &["branch", "--list", "--format=%(refname)"]).unwrap()
    }

    pub fn worktree_list(repo: &Path) -> String {
        git(repo, &["worktree", "list", "--porcelain"]).unwrap()
    }

    pub fn upstream(dir: &Path) -> Option<String> {
        git(dir, &["rev-parse", "--abbrev-ref", "@{upstream}"])
            .ok()
            .map(|s| s.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::testing::*;
    use super::*;

    #[test]
    fn a_failed_calls_output_keeps_its_last_lines_and_says_how_many_it_cut() {
        let log: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();

        let kept = last_lines(&log.join("\n"));

        assert!(
            kept.starts_with("[5 earlier lines cut]\nline 6\n"),
            "{kept}"
        );
        assert!(kept.ends_with("\nline 25"), "{kept}");
        assert_eq!(last_lines("one\ntwo"), "one\ntwo");
    }

    #[test]
    fn container_layout_puts_worktrees_beside_the_main_checkout() {
        assert_eq!(
            worktree_path(Path::new("/p/loopherd/loopherd"), "feat/LH-9-spawn"),
            PathBuf::from("/p/loopherd/worktrees/feat-LH-9-spawn")
        );
    }

    #[test]
    fn other_layouts_use_a_sibling_worktrees_directory() {
        assert_eq!(
            worktree_path(Path::new("/src/herdr"), "fix/a/b"),
            PathBuf::from("/src/herdr-worktrees/fix-a-b")
        );
    }

    #[test]
    fn checks_out_a_branch_that_exists_on_origin() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feature/existing").unwrap();
        assert_eq!(wt, root.path().join("proj/worktrees/feature-existing"));
        assert_eq!(current_branch(&wt), "feature/existing");
        assert_eq!(upstream(&wt).as_deref(), Some("origin/feature/existing"));
    }

    #[test]
    fn creates_a_new_branch_from_origins_default_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/new").unwrap();
        assert_eq!(current_branch(&wt), "feat/new");
        assert_eq!(upstream(&wt), None);
        let base = git(&wt, &["merge-base", "HEAD", "origin/main"]).unwrap();
        let head = git(&wt, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(base, head);
    }

    fn nobody(_: &Path) -> Option<String> {
        None
    }

    /// Plans clearing `feat/x` and carries the plan out.
    fn clear(repo: &Path, worktree: Option<&Path>, fate: Fate) -> Plan {
        let plan = plan_clear(repo, worktree, Some("feat/x"), fate, &nobody).unwrap();
        if let Plan::Clear(clearing) = &plan {
            clearing.remove_local().unwrap();
            clearing.delete_remote().unwrap();
        }
        plan
    }

    #[test]
    fn clearing_a_branch_that_was_never_made_finds_nothing() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());

        assert_eq!(clear(&repo, None, Fate::Clear), Plan::Absent);
    }

    #[test]
    fn clearing_a_branch_with_nothing_new_removes_it_and_its_worktree() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        let start = head(&wt);

        let cleared = clear(&repo, None, Fate::Clear);

        assert_eq!(
            cleared,
            Plan::Clear(Clearing {
                repo: repo.clone(),
                branch: Some("feat/x".into()),
                worktrees: vec![(wt.clone(), start.clone())],
                head: Some(start),
                remote: None,
                dropped: 0,
                base: "origin/main".into(),
            })
        );
        assert!(!wt.exists());
        assert!(!has_branch(&repo, "feat/x"));
    }

    #[test]
    fn clearing_a_branch_with_commits_not_on_the_base_changes_nothing_unless_dropped() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        commit(&wt, "unpushed");

        let kept = clear(&repo, None, Fate::Clear);

        assert_eq!(
            kept,
            Plan::Ahead {
                count: 1,
                base: "origin/main".into()
            }
        );
        assert!(wt.exists());
        assert!(has_branch(&repo, "feat/x"));

        let dropped = clear(&repo, None, Fate::Drop);

        assert!(
            matches!(dropped, Plan::Clear(Clearing { dropped: 1, .. })),
            "{dropped:?}"
        );
        assert!(!has_branch(&repo, "feat/x"));
    }

    #[test]
    fn keeping_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();

        assert_eq!(
            clear(&repo, Some(&wt), Fate::Keep),
            Plan::Kept {
                local: true,
                remote: false
            }
        );
        assert!(wt.exists());
        assert!(has_branch(&repo, "feat/x"));
    }

    #[test]
    fn clearing_a_branch_whose_worktree_has_changes_changes_nothing_even_when_dropped() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        std::fs::write(wt.join("notes"), "draft").unwrap();

        let cleared = clear(&repo, None, Fate::Drop);

        assert_eq!(
            cleared,
            Plan::Dirty(format!(
                "{} has uncommitted or untracked changes",
                wt.display()
            ))
        );
        assert!(wt.join("notes").exists());
        assert!(has_branch(&repo, "feat/x"));
    }

    #[test]
    fn a_branch_checked_out_in_the_main_checkout_is_taken_even_when_kept() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        git_in(&repo, &["checkout", "--quiet", "-b", "feat/x"]);

        for fate in [Fate::Keep, Fate::Drop] {
            assert_eq!(
                clear(&repo, None, fate),
                Plan::Taken(format!(
                    "feat/x is checked out in the main checkout {}",
                    repo.display()
                ))
            );
        }
        assert!(has_branch(&repo, "feat/x"));
    }

    #[test]
    fn a_worktree_a_live_member_uses_is_taken() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();

        let cleared = plan_clear(&repo, None, Some("feat/x"), Fate::Drop, &|_| {
            Some("lh-9".into())
        })
        .unwrap();

        assert_eq!(
            cleared,
            Plan::Taken(format!("{} is in use by lh-9", wt.display()))
        );
        assert!(wt.exists());
    }

    #[test]
    fn clears_the_recorded_worktree_even_when_it_left_the_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        git_in(&wt, &["checkout", "--quiet", "--detach"]);
        git_in(&repo, &["branch", "--quiet", "-D", "feat/x"]);

        let cleared = clear(&repo, Some(&wt), Fate::Clear);

        assert!(
            matches!(&cleared, Plan::Clear(Clearing { head: None, worktrees, .. }) if worktrees.len() == 1),
            "{cleared:?}"
        );
        assert!(!wt.exists());
        ensure_worktree(&repo, "feat/x").unwrap();
    }

    #[test]
    fn counts_a_detached_worktrees_own_commits() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_detached_worktree(&repo, "lead-x").unwrap();
        commit(&wt, "detached work");

        let cleared = plan_clear(&repo, Some(&wt), None, Fate::Clear, &nobody).unwrap();

        assert_eq!(
            cleared,
            Plan::Ahead {
                count: 1,
                base: "origin/main".into()
            }
        );
    }

    #[test]
    fn clearing_a_branch_whose_worktree_dir_is_gone_removes_its_record_and_the_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        std::fs::remove_dir_all(&wt).unwrap();

        let cleared = clear(&repo, Some(&wt), Fate::Clear);

        assert!(
            matches!(cleared, Plan::Clear(Clearing { dropped: 0, .. })),
            "{cleared:?}"
        );
        assert!(!worktree_list(&repo).contains("feat/x"));
        assert!(!has_branch(&repo, "feat/x"));
    }

    fn on_origin(root: &Path, branch: &str) -> bool {
        has_ref(&root.join("origin.git"), &format!("refs/heads/{branch}"))
    }

    /// A `feat/x` pushed with one commit not on the base, then removed locally.
    fn pushed_only(root: &Path, repo: &Path) -> String {
        let wt = ensure_worktree(repo, "feat/x").unwrap();
        commit(&wt, "pushed");
        push_head(&wt, "feat/x");
        let pushed = head(&wt);
        git_in(repo, &["worktree", "remove", path_str(&wt)]);
        git_in(repo, &["branch", "--quiet", "-D", "feat/x"]);
        assert!(on_origin(root, "feat/x"));
        pushed
    }

    fn path_str(p: &Path) -> &str {
        p.to_str().unwrap()
    }

    #[test]
    fn an_origin_branch_with_commits_not_on_the_base_changes_nothing_unless_dropped() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let pushed = pushed_only(root.path(), &repo);

        assert_eq!(
            clear(&repo, None, Fate::Clear),
            Plan::Ahead {
                count: 1,
                base: "origin/main".into()
            }
        );
        assert!(on_origin(root.path(), "feat/x"));

        let dropped = clear(&repo, None, Fate::Drop);

        assert_eq!(
            dropped,
            Plan::Clear(Clearing {
                repo: repo.clone(),
                branch: Some("feat/x".into()),
                worktrees: vec![],
                head: None,
                remote: Some(pushed),
                dropped: 1,
                base: "origin/main".into(),
            })
        );
        assert!(!on_origin(root.path(), "feat/x"));
        assert_eq!(clear(&repo, None, Fate::Clear), Plan::Absent);
    }

    #[test]
    fn keeping_an_origin_branch_says_the_new_member_starts_from_it() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        pushed_only(root.path(), &repo);

        assert_eq!(
            clear(&repo, None, Fate::Keep),
            Plan::Kept {
                local: false,
                remote: true
            }
        );
        assert!(on_origin(root.path(), "feat/x"));
    }

    #[test]
    fn an_origin_branch_already_on_the_base_is_cleared() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        push_head(&wt, "feat/x");

        let cleared = clear(&repo, None, Fate::Clear);

        assert!(
            matches!(
                &cleared,
                Plan::Clear(Clearing {
                    remote: Some(_),
                    dropped: 0,
                    ..
                })
            ),
            "{cleared:?}"
        );
        assert!(!on_origin(root.path(), "feat/x"));
        assert!(!has_branch(&repo, "feat/x"));
    }

    #[test]
    fn origins_default_branch_is_taken_whatever_the_fate_and_by_its_name_on_origin() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let origin = root.path().join("origin.git");
        git_in(&origin, &["branch", "trunk", "main"]);
        git_in(&origin, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
        git_in(&repo, &["switch", "--quiet", "-c", "elsewhere"]);

        for fate in [Fate::Clear, Fate::Drop] {
            let plan = plan_clear(&repo, None, Some("trunk"), fate, &nobody).unwrap();

            assert_eq!(
                plan,
                Plan::Taken(
                    "trunk is origin's default branch, which release never deletes; \
                     pass --keep-branch to release it without clearing"
                        .into()
                ),
                "{fate:?}"
            );
        }
        assert_eq!(
            plan_clear(&repo, None, Some("trunk"), Fate::Keep, &nobody).unwrap(),
            Plan::Kept {
                local: false,
                remote: true
            }
        );
        let main = plan_clear(&repo, None, Some("main"), Fate::Clear, &nobody).unwrap();
        assert!(matches!(main, Plan::Clear(_)), "{main:?}");
    }

    #[test]
    fn commits_another_origin_branch_holds_are_not_counted_dropped() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        commit(&wt, "saved elsewhere");
        push_head(&wt, "backup");
        commit(&wt, "only here");

        let cleared = clear(&repo, None, Fate::Drop);

        assert!(
            matches!(&cleared, Plan::Clear(Clearing { dropped: 1, .. })),
            "{cleared:?}"
        );
    }

    #[test]
    fn keeping_nothing_finds_nothing() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());

        assert_eq!(clear(&repo, None, Fate::Keep), Plan::Absent);
    }

    #[test]
    fn a_locked_worktree_is_taken_and_nothing_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let recorded = ensure_detached_worktree(&repo, "old").unwrap();
        let wt = ensure_worktree(&repo, "feat/x").unwrap();
        git_in(
            &repo,
            &["worktree", "lock", "--reason", "on a stick", path_str(&wt)],
        );

        let cleared = clear(&repo, Some(&recorded), Fate::Drop);

        assert_eq!(
            cleared,
            Plan::Taken(format!("{} is locked (on a stick)", wt.display()))
        );
        assert!(recorded.exists());
        assert!(has_branch(&repo, "feat/x"));
    }

    #[test]
    fn another_worktree_on_the_branch_is_taken_and_left() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let review = root.path().join("my-review");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feat/x",
                path_str(&review),
            ],
        );

        let cleared = clear(&repo, None, Fate::Drop);

        assert_eq!(
            cleared,
            Plan::Taken(format!(
                "feat/x is checked out in {}, a worktree loopherd didn't make",
                review.display()
            ))
        );
        assert!(review.exists());
    }

    #[test]
    fn a_clean_leftover_at_the_spawn_path_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let leftover = worktree_path(&repo, "feat/x");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path_str(&leftover),
            ],
        );

        let cleared = clear(&repo, None, Fate::Clear);

        assert!(
            matches!(&cleared, Plan::Clear(Clearing { worktrees, .. }) if worktrees.len() == 1),
            "{cleared:?}"
        );
        assert!(!leftover.exists());
        ensure_worktree(&repo, "feat/x").unwrap();
    }

    #[test]
    fn a_leftover_with_changes_at_the_spawn_path_is_refused_and_named() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let leftover = worktree_path(&repo, "feat/x");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path_str(&leftover),
            ],
        );
        std::fs::write(leftover.join("notes"), "draft").unwrap();

        for fate in [Fate::Clear, Fate::Drop] {
            assert_eq!(
                clear(&repo, None, fate),
                Plan::Dirty(format!(
                    "{} has uncommitted or untracked changes",
                    leftover.display()
                ))
            );
        }
        assert!(leftover.join("notes").exists());
    }

    #[test]
    fn a_leftover_off_the_branch_at_the_spawn_path_is_taken_when_kept() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let leftover = worktree_path(&repo, "feat/x");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path_str(&leftover),
            ],
        );

        let cleared = clear(&repo, None, Fate::Keep);

        assert!(
            matches!(&cleared, Plan::Taken(why) if why.contains("isn't on feat/x")),
            "{cleared:?}"
        );
        assert!(leftover.exists());
    }

    #[test]
    fn a_directory_in_the_way_at_the_spawn_path_is_taken_and_left() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let leftover = worktree_path(&repo, "feat/x");
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("notes"), "draft").unwrap();

        for fate in [Fate::Clear, Fate::Keep, Fate::Drop] {
            assert_eq!(
                clear(&repo, Some(&leftover), fate),
                Plan::Taken(format!(
                    "{} is in the way and isn't a worktree of {}",
                    leftover.display(),
                    repo.display()
                ))
            );
        }
        assert!(leftover.join("notes").exists());
    }

    #[test]
    fn an_unreachable_origin_warns_and_names_the_fallback() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        git_in(&repo, &["remote", "set-url", "origin", "/nowhere"]);

        let (branch, warning) = default_branch_and_warning(&repo).unwrap();

        assert_eq!(branch, "origin/main");
        let warning = warning.unwrap();
        assert!(
            warning.starts_with(
                "warning: couldn't ask origin for its default branch \
                 (git ls-remote --symref origin HEAD: fatal: "
            ),
            "{warning}"
        );
        assert!(
            warning.ends_with("); using the local origin/HEAD, origin/main"),
            "{warning}"
        );
        assert_eq!(warning.lines().count(), 1, "{warning}");
    }

    #[test]
    fn a_stale_origin_head_warns_with_both_branches_and_the_fix() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        move_origin_default(root.path(), "staging");
        git_in(&repo, &["fetch", "--quiet", "origin"]);
        let (branch, warning) = default_branch_and_warning(&repo).unwrap();
        assert_eq!(branch, "origin/staging");
        assert_eq!(
            warning.unwrap(),
            format!(
                "warning: origin's default branch is origin/staging, but the local \
                 origin/HEAD points at origin/main; fix it with \
                 `git -C {} remote set-head origin --auto`",
                repo.display()
            )
        );
    }

    #[test]
    fn a_current_origin_head_gives_no_warning() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let (branch, warning) = default_branch_and_warning(&repo).unwrap();
        assert_eq!(branch, "origin/main");
        assert_eq!(warning, None);
    }

    #[test]
    fn the_symref_parser_takes_only_heads_symref() {
        let out = "ref: refs/heads/other\trefs/remotes/x\nref: refs/heads/main\tHEAD\nabc\tHEAD\n";
        assert_eq!(parse_head_symref(out).as_deref(), Some("origin/main"));
        assert_eq!(parse_head_symref("ref: refs/heads/other\tnot-head\n"), None);
    }

    #[test]
    fn a_branch_spawn_starts_from_origins_moved_default() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        move_origin_default(root.path(), "staging");
        let wt = ensure_worktree(&repo, "feat/new").unwrap();
        assert_eq!(head(&wt), head_of(&repo, "origin/staging"));
    }

    #[test]
    fn a_detached_origin_head_warns_and_names_the_fallback() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        detach_origin_head(root.path());
        let (branch, warning) = default_branch_and_warning(&repo).unwrap();
        assert_eq!(branch, "origin/main");
        assert_eq!(
            warning.unwrap(),
            "warning: origin's HEAD names no branch (detached or unborn); \
             using the local origin/HEAD, origin/main"
        );
    }

    #[test]
    fn an_unborn_origin_head_warns_and_names_the_fallback() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        unborn_origin_head(root.path());
        let (branch, warning) = default_branch_and_warning(&repo).unwrap();
        assert_eq!(branch, "origin/main");
        assert!(warning.unwrap().contains("origin/HEAD, origin/main"));
    }

    #[test]
    fn a_single_branch_clone_names_the_unfetched_default_and_its_refspec() {
        let root = tempfile::tempdir().unwrap();
        let repo = single_branch_repo(root.path());
        move_origin_default(root.path(), "staging");

        let err = ensure_worktree(&repo, "feat/new").unwrap_err();

        assert_eq!(
            format!("{err:#}"),
            format!(
                "origin's default branch is staging, but this clone's fetch refspec \
                 (+refs/heads/main:refs/remotes/origin/main) doesn't fetch it; add it with \
                 `git -C {} remote set-branches --add origin staging`",
                repo.display()
            )
        );
        assert!(!has_branch(&repo, "feat/new"));
    }

    /// Points origin at an `ext::` command, which git runs in place of a transport.
    fn origin_runs(repo: &Path, command: &str) {
        git_in(repo, &["config", "protocol.ext.allow", "always"]);
        git_in(
            repo,
            &["remote", "set-url", "origin", &format!("ext::{command}")],
        );
    }

    #[test]
    fn a_hung_remote_call_times_out_with_a_clear_error() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        origin_runs(&repo, "sleep 30");
        let start = std::time::Instant::now();

        let err = remote_git_within(
            &repo,
            &["fetch", "--quiet", "origin"],
            std::time::Duration::from_millis(300),
        )
        .unwrap_err();

        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(
            format!("{err:#}"),
            "git fetch --quiet origin: origin didn't answer within 300ms; \
             check your credentials, ssh agent and network, or raise the limit \
             with LOOPHERD_GIT_TIMEOUT=<seconds>"
        );
    }

    #[test]
    fn remote_calls_never_prompt_for_credentials() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let env = root.path().join("env");
        origin_runs(&repo, &format!("sh -c env>{};false", env.display()));

        assert!(fetch(&repo).is_err());

        let env = std::fs::read_to_string(env).unwrap();
        assert!(env.lines().any(|l| l == "GIT_TERMINAL_PROMPT=0"), "{env}");
    }

    const LOST_RACE: &str = "error: cannot lock ref 'refs/remotes/origin/main': \
                             is at 1111111 but expected 2222222";

    /// Points origin at a transport that fails its first `failures` calls
    /// with `stderr`, then serves the real origin. Returns its call log.
    fn origin_fails_first(root: &Path, repo: &Path, failures: usize, stderr: &str) -> PathBuf {
        let calls = root.join("calls");
        let message = root.join("flaky-stderr");
        std::fs::write(&message, format!("{stderr}\n")).unwrap();
        let script = root.join("flaky-origin");
        std::fs::write(
            &script,
            format!(
                "echo call >> {calls}\n\
                 if [ $(wc -l < {calls}) -le {failures} ]; then\n  \
                 cat {message} >&2\n  exit 1\nfi\n\
                 exec git upload-pack {origin}\n",
                calls = calls.display(),
                message = message.display(),
                origin = root.join("origin.git").display(),
            ),
        )
        .unwrap();
        origin_runs(repo, &format!("sh {}", script.display()));
        calls
    }

    fn call_count(calls: &Path) -> usize {
        std::fs::read_to_string(calls).unwrap().lines().count()
    }

    #[test]
    fn a_fetch_that_lost_a_ref_lock_race_retries_once() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let calls = origin_fails_first(root.path(), &repo, 1, LOST_RACE);

        fetch(&repo).unwrap();

        assert_eq!(call_count(&calls), 2);
    }

    #[test]
    fn a_ref_lock_failure_that_lasts_fails_after_one_retry() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let calls = origin_fails_first(root.path(), &repo, 2, LOST_RACE);

        let err = fetch(&repo).unwrap_err();

        assert!(format!("{err:#}").contains("cannot lock ref"), "{err:#}");
        assert_eq!(call_count(&calls), 2);
    }

    #[test]
    fn a_fetch_failure_that_isnt_a_lost_race_is_not_retried() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        for stderr in [
            "fatal: repository not found",
            "fatal: couldn't find remote ref refs/heads/nope",
        ] {
            let calls = origin_fails_first(root.path(), &repo, 2, stderr);
            let _ = std::fs::remove_file(&calls);

            let err = fetch(&repo).unwrap_err();

            assert!(format!("{err:#}").contains(stderr), "{err:#}");
            assert_eq!(call_count(&calls), 1, "{stderr}");
        }
    }

    const CHILD_REPO: &str = "LOOPHERD_TEST_CHILD_REPO";

    /// Runs only as the child of [`fetch_in_child`], which sets its repo.
    #[test]
    fn fetches_origin() {
        if let Some(repo) = std::env::var_os(CHILD_REPO) {
            match fetch(Path::new(&repo)) {
                Ok(()) => eprintln!("fetched"),
                Err(e) => eprintln!("fetch failed: {e:#}"),
            }
        }
    }

    /// Runs only as the child of [`default_branch_in_child`], which sets its repo.
    #[test]
    fn looks_up_origins_default_branch() {
        if let Some(repo) = std::env::var_os(CHILD_REPO) {
            match default_branch_and_warning(Path::new(&repo)) {
                Ok((branch, warning)) => eprintln!("default branch: {branch}, {warning:?}"),
                Err(e) => eprintln!("default branch failed: {e:#}"),
            }
        }
    }

    fn fetch_in_child(repo: &Path, env: &[(&str, &str)]) -> String {
        in_child("fetches_origin", repo, env)
    }

    fn default_branch_in_child(repo: &Path, env: &[(&str, &str)]) -> String {
        in_child("looks_up_origins_default_branch", repo, env)
    }

    /// Runs the child-only test `test` against `repo` in a process whose
    /// environment and global git config are clean apart from `env`.
    fn in_child(test: &str, repo: &Path, env: &[(&str, &str)]) -> String {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "--nocapture",
                &format!("worktree::tests::{test}"),
            ])
            .env(CHILD_REPO, repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for var in [
            "GIT_SSH_COMMAND",
            "GIT_SSH",
            "SSH_ASKPASS_REQUIRE",
            "SSH_ASKPASS",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "LOOPHERD_GIT_TIMEOUT",
        ] {
            child.env_remove(var);
        }
        let out = child.envs(env.iter().copied()).output().unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// The environment origin's transport sees when `repo` fetches in a child
    /// with `env`.
    fn transport_env(root: &Path, repo: &Path, env: &[(&str, &str)]) -> Vec<String> {
        let dump = root.join("transport-env");
        origin_runs(repo, &format!("sh -c env>{};false", dump.display()));
        fetch_in_child(repo, env);
        std::fs::read_to_string(dump)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("GIT_SSH"))
            .map(String::from)
            .collect()
    }

    const SSH_HINT: &str = "hint: if ssh needed a passphrase or host-key prompt, set \
                            SSH_ASKPASS_REQUIRE=prefer (with SSH_ASKPASS and a display) \
                            to allow a GUI one";

    /// Points origin at a transport that writes `stderr` and fails.
    fn origin_fails_with(root: &Path, repo: &Path, stderr: &str) {
        let script = root.join("failing-origin");
        std::fs::write(&script, format!("printf '%s\\n' '{stderr}' >&2\nexit 1\n")).unwrap();
        origin_runs(repo, &format!("sh {}", script.display()));
    }

    #[test]
    fn an_ssh_failure_under_batch_mode_hints_how_to_allow_a_prompt() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        for stderr in [
            "git@example.com: Permission denied (publickey).",
            "Host key verification failed.",
            "Load key \"/home/me/.ssh/id_ed25519\": incorrect passphrase supplied",
        ] {
            origin_fails_with(root.path(), &repo, stderr);

            let out = fetch_in_child(&repo, &[]);

            assert!(out.contains(&format!("{stderr}\n")), "{out}");
            assert!(out.contains(&format!("\n{SSH_HINT}\n")), "{out}");
        }
    }

    #[test]
    fn a_failure_that_isnt_ssh_gets_no_hint() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        origin_fails_with(root.path(), &repo, "fatal: repository not found");

        let out = fetch_in_child(&repo, &[]);

        assert!(out.contains("fatal: repository not found"), "{out}");
        assert!(!out.contains("hint:"), "{out}");
    }

    #[test]
    fn a_users_own_ssh_or_askpass_setting_gets_no_hint() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        origin_fails_with(root.path(), &repo, "Permission denied (publickey).");
        let cases: [&[(&str, &str)]; 4] = [
            &[("GIT_SSH_COMMAND", "my-ssh")],
            &[("GIT_SSH", "my-ssh")],
            &[("SSH_ASKPASS_REQUIRE", "force")],
            &[
                ("SSH_ASKPASS_REQUIRE", "prefer"),
                ("SSH_ASKPASS", "my-askpass"),
                ("DISPLAY", ":0"),
            ],
        ];
        for env in cases {
            let out = fetch_in_child(&repo, env);
            assert!(out.contains("Permission denied"), "{env:?}: {out}");
            assert!(!out.contains("hint:"), "{env:?}: {out}");
        }

        git_in(&repo, &["config", "core.sshCommand", "my-ssh"]);
        let out = fetch_in_child(&repo, &[]);
        assert!(out.contains("Permission denied"), "{out}");
        assert!(!out.contains("hint:"), "{out}");
    }

    #[test]
    fn a_timeout_gets_no_hint() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let script = root.path().join("hung-origin");
        std::fs::write(&script, "echo 'Permission denied' >&2\nsleep 30\n").unwrap();
        origin_runs(&repo, &format!("sh {}", script.display()));

        let out = fetch_in_child(&repo, &[("LOOPHERD_GIT_TIMEOUT", "1")]);

        assert!(out.contains("origin didn't answer within 1s"), "{out}");
        assert!(!out.contains("hint:"), "{out}");
    }

    #[test]
    fn an_ssh_failure_asking_for_the_default_branch_hints_in_the_warning() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        origin_fails_with(root.path(), &repo, "Permission denied (publickey).");

        let out = default_branch_in_child(&repo, &[]);

        assert!(
            out.contains(&format!(
                "warning: couldn't ask origin for its default branch \
                 (git ls-remote --symref origin HEAD: Permission denied (publickey).); \
                 using the local origin/HEAD, origin/main\\n{SSH_HINT}\")"
            )),
            "{out}"
        );
    }

    #[test]
    fn an_ssh_failure_setting_origins_head_hints_in_the_error() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        git_in(&repo, &["remote", "set-head", "origin", "--delete"]);
        origin_fails_with(root.path(), &repo, "Permission denied (publickey).");

        let out = default_branch_in_child(&repo, &[]);

        assert!(
            out.contains(
                "default branch failed: find origin's default branch: \
                 git remote set-head origin --auto: Permission denied (publickey).\n"
            ),
            "{out}"
        );
        assert!(out.contains(&format!("\n{SSH_HINT}\n")), "{out}");
    }

    #[test]
    fn remote_calls_make_ssh_fail_instead_of_prompting() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        assert_eq!(
            transport_env(root.path(), &repo, &[]),
            ["GIT_SSH_COMMAND=ssh -o BatchMode=yes"]
        );
    }

    #[test]
    fn a_users_git_ssh_command_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        assert_eq!(
            transport_env(root.path(), &repo, &[("GIT_SSH_COMMAND", "my-ssh")]),
            ["GIT_SSH_COMMAND=my-ssh"]
        );
    }

    #[test]
    fn a_users_git_ssh_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        assert_eq!(
            transport_env(root.path(), &repo, &[("GIT_SSH", "my-ssh")]),
            ["GIT_SSH=my-ssh"]
        );
    }

    #[test]
    fn a_users_ssh_askpass_require_keeps_its_prompt() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        assert_eq!(
            transport_env(root.path(), &repo, &[("SSH_ASKPASS_REQUIRE", "force")]),
            [] as [&str; 0]
        );
    }

    #[test]
    fn askpass_require_ignores_case_like_ssh() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let cases: [&[(&str, &str)]; 2] = [
            &[("SSH_ASKPASS_REQUIRE", "FORCE")],
            &[
                ("SSH_ASKPASS_REQUIRE", "Prefer"),
                ("SSH_ASKPASS", "my-askpass"),
                ("WAYLAND_DISPLAY", "wayland-0"),
            ],
        ];
        for env in cases {
            assert_eq!(
                transport_env(root.path(), &repo, env),
                [] as [&str; 0],
                "{env:?}"
            );
        }
    }

    #[test]
    fn a_preferred_askpass_keeps_its_prompt() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        assert_eq!(
            transport_env(
                root.path(),
                &repo,
                &[
                    ("SSH_ASKPASS_REQUIRE", "prefer"),
                    ("SSH_ASKPASS", "my-askpass"),
                    ("DISPLAY", ":0"),
                ]
            ),
            [] as [&str; 0]
        );
    }

    #[test]
    fn an_askpass_require_that_wont_prompt_still_gets_batch_mode() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let cases: [&[(&str, &str)]; 9] = [
            &[("SSH_ASKPASS_REQUIRE", "never")],
            &[("SSH_ASKPASS_REQUIRE", "")],
            &[("SSH_ASKPASS_REQUIRE", "bogus")],
            &[("SSH_ASKPASS_REQUIRE", "prefer")],
            &[("SSH_ASKPASS_REQUIRE", "prefer"), ("SSH_ASKPASS", "")],
            &[
                ("SSH_ASKPASS_REQUIRE", "never"),
                ("SSH_ASKPASS", "my-askpass"),
            ],
            &[("SSH_ASKPASS_REQUIRE", " force")],
            &[
                ("SSH_ASKPASS_REQUIRE", "prefer"),
                ("SSH_ASKPASS", "my-askpass"),
            ],
            &[
                ("SSH_ASKPASS_REQUIRE", "prefer"),
                ("SSH_ASKPASS", "my-askpass"),
                ("DISPLAY", ""),
                ("WAYLAND_DISPLAY", ""),
            ],
        ];
        for env in cases {
            assert_eq!(
                transport_env(root.path(), &repo, env),
                ["GIT_SSH_COMMAND=ssh -o BatchMode=yes"],
                "{env:?}"
            );
        }
    }

    #[test]
    fn a_repos_core_ssh_command_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        git_in(&repo, &["config", "core.sshCommand", "my-ssh"]);
        assert_eq!(transport_env(root.path(), &repo, &[]), [] as [&str; 0]);
    }

    #[test]
    fn a_global_core_ssh_command_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let global = root.path().join("gitconfig");
        std::fs::write(&global, "[core]\n\tsshCommand = my-ssh\n").unwrap();
        let global = global.to_str().unwrap();
        assert_eq!(
            transport_env(root.path(), &repo, &[("GIT_CONFIG_GLOBAL", global)]),
            [] as [&str; 0]
        );
    }

    #[test]
    fn a_config_that_cant_be_read_is_not_overridden() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let global = root.path().join("gitconfig");
        std::fs::write(&global, "[core]\n\tsshCommand = my-ssh\n[broken\n").unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let log = root.path().join("git.log");
        let real_git = Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        let real_git = String::from_utf8(real_git.stdout).unwrap();
        crate::test_fakes::executable(
            &bin.join("git"),
            format!(
                "#!/bin/sh\necho \"$* ${{GIT_SSH_COMMAND-unset}}\" >>{}\nexec {} \"$@\"\n",
                log.display(),
                real_git.trim()
            ),
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

        let stderr = fetch_in_child(
            &repo,
            &[
                ("PATH", &path),
                ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ],
        );

        assert!(stderr.contains("fatal: bad config line 3"), "{stderr}");
        let log = std::fs::read_to_string(log).unwrap();
        let fetch = log.lines().find(|l| l.contains(" fetch ")).unwrap();
        assert!(fetch.ends_with(" unset"), "{log}");
    }

    #[test]
    fn the_remote_timeout_comes_from_loopherd_git_timeout() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        origin_runs(&repo, "sleep 30");
        let start = std::time::Instant::now();

        let stderr = fetch_in_child(&repo, &[("LOOPHERD_GIT_TIMEOUT", "1")]);

        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(
            stderr.contains(
                "fetch failed: git fetch --quiet origin: origin didn't answer within 1s;"
            ),
            "{stderr}"
        );
    }

    #[test]
    fn a_bad_loopherd_git_timeout_is_named_in_the_error() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        for bad in ["2m", "0", "", "86401", "18446744073709551615"] {
            let stderr = fetch_in_child(&repo, &[("LOOPHERD_GIT_TIMEOUT", bad)]);
            assert!(
                stderr.contains(&format!(
                    "fetch failed: LOOPHERD_GIT_TIMEOUT must be a whole number \
                     of seconds from 1 to 86400, not '{bad}'"
                )),
                "{stderr}"
            );
        }
    }

    #[test]
    fn a_loopherd_git_timeout_of_one_day_is_allowed() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());

        let stderr = fetch_in_child(&repo, &[("LOOPHERD_GIT_TIMEOUT", "86400")]);

        assert_eq!(stderr.lines().last(), Some("fetched"), "{stderr}");
    }

    #[test]
    fn a_default_branch_lookup_that_times_out_is_an_error_not_a_warning() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        origin_runs(&repo, "sleep 30");

        let stderr = default_branch_in_child(&repo, &[("LOOPHERD_GIT_TIMEOUT", "1")]);

        assert!(
            stderr.contains(
                "default branch failed: git ls-remote --symref origin HEAD: \
                 origin didn't answer within 1s;"
            ),
            "{stderr}"
        );
        assert!(!stderr.contains("warning"), "{stderr}");
    }

    /// A `PATH` holding only `tools`, linked from where they're found now.
    fn path_with_only(dir: &Path, tools: &[&str]) -> String {
        std::fs::create_dir(dir).unwrap();
        for tool in tools {
            let found = Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .output()
                .unwrap();
            let found = String::from_utf8(found.stdout).unwrap();
            std::os::unix::fs::symlink(found.trim(), dir.join(tool)).unwrap();
        }
        dir.to_str().unwrap().to_string()
    }

    /// Whether `pid` has exited, waiting up to a few seconds for it to.
    fn exits_soon(pid: &str) -> bool {
        let stat = format!("/proc/{pid}/stat");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match std::fs::read_to_string(&stat) {
                Err(_) => return true,
                Ok(s) if s.rsplit(')').next().unwrap().trim_start().starts_with('Z') => {
                    return true;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        false
    }

    #[test]
    fn a_timeout_kills_the_transports_helpers_without_a_kill_binary() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let pidfile = root.path().join("helper.pid");
        let transport = root.path().join("transport");
        crate::test_fakes::executable(
            &transport,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! >{}\nwait\n",
                pidfile.display()
            ),
        );
        origin_runs(&repo, transport.to_str().unwrap());
        let path = path_with_only(&root.path().join("bin"), &["git", "sh", "sleep"]);

        let stderr = fetch_in_child(&repo, &[("PATH", &path), ("LOOPHERD_GIT_TIMEOUT", "1")]);

        assert!(stderr.contains("didn't answer within 1s"), "{stderr}");
        let helper = std::fs::read_to_string(pidfile).unwrap();
        assert!(
            exits_soon(helper.trim()),
            "helper {helper} outlived the timeout"
        );
    }

    const WARN_TWICE_REPO: &str = "LOOPHERD_TEST_WARN_TWICE_REPO";

    /// Runs only as the child of the test below, which sets its repo.
    #[test]
    fn looks_up_the_default_branch_twice() {
        if let Some(repo) = std::env::var_os(WARN_TWICE_REPO) {
            default_branch(Path::new(&repo)).unwrap();
            default_branch(Path::new(&repo)).unwrap();
        }
    }

    #[test]
    fn a_stale_origin_head_warns_on_stderr_once_per_run() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        move_origin_default(root.path(), "staging");
        git_in(&repo, &["fetch", "--quiet", "origin"]);

        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "--nocapture"])
            .arg("worktree::tests::looks_up_the_default_branch_twice")
            .env(WARN_TWICE_REPO, &repo)
            .output()
            .unwrap();

        let stderr = String::from_utf8_lossy(&out.stderr);
        let warnings = stderr
            .lines()
            .filter(|l| l.starts_with("warning: origin's default branch is origin/staging"))
            .count();
        assert_eq!(warnings, 1, "{stderr}");
    }

    #[test]
    fn repo_root_from_a_subdirectory_is_the_main_checkout() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let sub = repo.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(repo_root(&sub).unwrap(), repo.canonicalize().unwrap());
    }

    #[test]
    fn repo_root_from_a_linked_worktree_is_the_main_checkout() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/new").unwrap();
        assert_eq!(repo_root(&wt).unwrap(), repo.canonicalize().unwrap());
    }

    #[test]
    fn repo_root_outside_a_repo_names_the_repo_flag() {
        let root = tempfile::tempdir().unwrap();
        let err = repo_root(root.path()).unwrap_err();
        assert!(format!("{err:#}").contains("--repo"), "{err:#}");
    }

    #[test]
    fn reuses_an_existing_worktree_for_the_branch() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let elsewhere = root.path().join("elsewhere");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feat/x",
                elsewhere.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(ensure_worktree(&repo, "feat/x").unwrap(), elsewhere);
    }

    /// Installs a hook that moves `branch` one commit on when a fetch updates
    /// a remote-tracking ref, standing in for a push racing `--close`.
    fn move_branch_on_next_fetch(repo: &Path, branch: &str) {
        let hook = repo.join(".git/hooks/reference-transaction");
        let script = format!(
            "#!/bin/sh\n\
             [ \"$1\" = committed ] && grep -q refs/remotes/ || exit 0\n\
             moved=$(git -c user.name=t -c user.email=t@t commit-tree '{branch}^{{tree}}' -p '{branch}' -m moved)\n\
             git update-ref 'refs/heads/{branch}' \"$moved\"\n"
        );
        crate::test_fakes::executable(&hook, script);
    }

    #[test]
    fn remove_keeps_a_branch_that_moved_after_its_head_was_checked() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        let wt = ensure_worktree(&repo, "feat/new").unwrap();
        commit(&wt, "work");
        push_head(&wt, "feat/new");
        merge_on_origin(root.path(), "feat/new");
        let checked = head(&wt);
        move_branch_on_next_fetch(&repo, "feat/new");

        let err = remove_if_merged(&repo, &wt).unwrap_err();

        assert!(format!("{err:#}").contains("update-ref"), "{err:#}");
        assert!(has_branch(&repo, "feat/new"));
        assert_ne!(head_of(&repo, "feat/new"), checked);
    }
}
