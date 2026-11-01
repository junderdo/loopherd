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

    pub fn current_branch(dir: &Path) -> String {
        git(dir, &["branch", "--show-current"])
            .unwrap()
            .trim()
            .to_string()
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
