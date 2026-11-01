//! Git worktrees for spawned agents.

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
    if let Some(branch) = &branch {
        git(repo, &["fetch", "--quiet", "origin"])?;
        let base = default_branch(repo)?;
        if !is_ancestor(repo, &head, &base)? {
            return Ok(Removal::Kept(format!("{branch} is not merged into {base}")));
        }
    }
    git(repo, &["worktree", "remove", path_str])?;
    if let Some(branch) = &branch {
        // `branch -d` judges against the checkout's HEAD, which may lag origin;
        // merging was checked above, and the old value guards the delete.
        git(
            repo,
            &["update-ref", "-d", &format!("refs/heads/{branch}"), &head],
        )?;
    }
    Ok(Removal::Removed)
}

/// Markers in a worktree's git dir that git leaves while an operation is
/// stopped part way.
const IN_PROGRESS: [(&str, &str); 6] = [
    ("rebase-merge", "rebase"),
    ("rebase-apply", "rebase"),
    ("MERGE_HEAD", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
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

fn default_branch(repo: &Path) -> Result<String> {
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
}
