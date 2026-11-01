//! Git worktrees for spawned agents.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

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
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .with_context(|| format!("{} is not in a git repo; pass --repo", dir.display()))?;
    let common = PathBuf::from(common.trim());
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
    git(repo, &["fetch", "--quiet", "origin"])?;
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

/// Why `branch` at `head` isn't merged into origin's default branch, if it isn't.
fn unmerged(repo: &Path, branch: &str, head: &str) -> Result<Option<String>> {
    git(repo, &["fetch", "--quiet", "origin"])?;
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

/// `repo`'s linked worktree entry at `path`, compared without canonicalizing
/// `path` itself since it may not exist.
fn listed_worktree(repo: &Path, path: &Path) -> Result<Option<ListedWorktree>> {
    let wanted = with_real_parent(path);
    let list = git(repo, &["worktree", "list", "--porcelain"])?;
    for block in list.split("\n\n").skip(1) {
        let mut lines = block.lines();
        let Some(listed) = lines.next().and_then(|l| l.strip_prefix("worktree ")) else {
            continue;
        };
        if with_real_parent(Path::new(listed)) != wanted {
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
    git(repo, &["fetch", "--quiet", "origin"])?;
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

/// Origin's default branch, printing a warning when the local origin/HEAD is stale.
fn default_branch(repo: &Path) -> Result<String> {
    let (branch, warning) = default_branch_and_warning(repo)?;
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    Ok(branch)
}

/// Origin's default branch as the remote reports it, falling back to the local
/// origin/HEAD, which git never updates after the clone.
fn default_branch_and_warning(repo: &Path) -> Result<(String, Option<String>)> {
    let Some(remote) = remote_default_branch(repo) else {
        return Ok((local_default_branch(repo)?, None));
    };
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

/// `None` when origin can't be reached or names no HEAD branch.
fn remote_default_branch(repo: &Path) -> Option<String> {
    let out = git(repo, &["ls-remote", "--symref", "origin", "HEAD"]).ok()?;
    let line = out
        .lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/"))?;
    let (branch, _) = line.split_once('\t')?;
    Some(format!("origin/{branch}"))
}

fn local_default_branch(repo: &Path) -> Result<String> {
    let head = git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .or_else(|_| {
        git(repo, &["remote", "set-head", "origin", "--auto"])?;
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
    use super::testing::*;
    use super::*;

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

    #[test]
    fn an_unreachable_origin_falls_back_to_the_local_origin_head() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        git_in(&repo, &["remote", "set-url", "origin", "/nowhere"]);
        assert_eq!(default_branch(&repo).unwrap(), "origin/main");
    }

    #[test]
    fn a_stale_origin_head_warns_with_both_branches_and_the_fix() {
        let root = tempfile::tempdir().unwrap();
        let repo = repo_with_origin(root.path());
        move_origin_default(root.path(), "staging");
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
        use std::os::unix::fs::PermissionsExt;
        let hook = repo.join(".git/hooks/reference-transaction");
        let script = format!(
            "#!/bin/sh\n\
             [ \"$1\" = committed ] && grep -q refs/remotes/ || exit 0\n\
             moved=$(git -c user.name=t -c user.email=t@t commit-tree '{branch}^{{tree}}' -p '{branch}' -m moved)\n\
             git update-ref 'refs/heads/{branch}' \"$moved\"\n"
        );
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
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
