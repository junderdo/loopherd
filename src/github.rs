//! Open pull requests on GitHub, asked through the GitHub CLI (`gh`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::pr_link::PrHeads;
use crate::shell::run_within;
use crate::worktree::remote_timeout;

/// An open pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    pub number: u64,
    pub url: String,
}

/// What GitHub said about a branch's pull requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLookup {
    None,
    Open(Pr),
    /// GitHub can't be asked from here, for this reason: no `gh`, no
    /// account, or an origin that isn't on GitHub.
    Unknown(String),
}

pub trait PullRequests {
    /// The open pull request whose head is `branch` in `repo`'s GitHub repo.
    /// Errs when GitHub could be asked but the asking failed.
    fn open_on(&self, repo: &Path, branch: &str) -> Result<PrLookup>;
}

/// Asks GitHub through the `gh` program.
pub struct GhCli {
    program: PathBuf,
}

impl Default for GhCli {
    fn default() -> Self {
        GhCli {
            program: "gh".into(),
        }
    }
}

impl PullRequests for GhCli {
    fn open_on(&self, repo: &Path, branch: &str) -> Result<PrLookup> {
        let url = origin_url(repo)?;
        let Some((host, name)) = github_repo(&url) else {
            return Ok(PrLookup::Unknown("origin isn't on GitHub".into()));
        };
        if host != "github.com" {
            let Some(status) = self.run(repo, &["auth", "status", "--hostname", &host])? else {
                return Ok(no_gh());
            };
            if !status.status.success() {
                return Ok(PrLookup::Unknown(format!(
                    "origin is on {host}, which the GitHub CLI isn't logged in to"
                )));
            }
        }
        // Without -R, gh may pick another remote (an upstream, a fork's parent) as the base repo.
        let full = format!("{host}/{name}");
        let args = [
            "pr",
            "list",
            "-R",
            &full,
            "--state",
            "open",
            "--limit",
            "1",
            "--json",
            "number,url",
            "--head",
            branch,
        ];
        match self.run(repo, &args)? {
            Some(output) => lookup_from(&output),
            None => Ok(no_gh()),
        }
    }
}

impl PrHeads for GhCli {
    fn head_of(&self, pr: &str) -> Result<Option<String>> {
        let args = [
            "pr",
            "view",
            pr,
            "--json",
            "headRefName",
            "--jq",
            ".headRefName",
        ];
        let Some(output) = self.run(Path::new("."), &args)? else {
            bail!("no GitHub CLI (gh) on PATH");
        };
        let said = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Ok((!head.is_empty()).then_some(head));
        }
        if said.contains("Could not resolve to a PullRequest") {
            return Ok(None);
        }
        bail!("gh pr view: {}", said.trim())
    }
}

impl GhCli {
    /// Runs gh with `args`; `None` when there's no gh to run.
    fn run(&self, repo: &Path, args: &[&str]) -> Result<Option<Output>> {
        let mut gh = Command::new(&self.program);
        gh.args(args)
            .current_dir(repo)
            .env("GH_PROMPT_DISABLED", "1");
        let shown = format!("gh {}", args[..2].join(" "));
        match run_within(&mut gh, remote_timeout()?) {
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e.context(format!("run {shown}"))),
            Ok(None) => bail!("{shown} didn't answer in time"),
            Ok(Some(output)) => Ok(Some(output)),
        }
    }
}

fn no_gh() -> PrLookup {
    PrLookup::Unknown("no GitHub CLI (gh) on PATH".into())
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn origin_url(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()
        .context("run git remote get-url origin")?;
    if !out.status.success() {
        bail!(
            "git remote get-url origin: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The host and `owner/repo` a GitHub-style remote `url` names: an https or
/// ssh URL, or scp-like `user@host:owner/repo`.
fn github_repo(url: &str) -> Option<(String, String)> {
    let (host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(scheme, "https" | "http" | "ssh" | "git") {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit('@').next()?;
            (host.split(':').next()?, path)
        }
        None => {
            let (user_host, path) = url.split_once(':')?;
            if user_host.contains('/') {
                return None;
            }
            (user_host.rsplit('@').next()?, path)
        }
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if host.is_empty() || owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((host.to_string(), format!("{owner}/{name}")))
}

fn lookup_from(output: &Output) -> Result<PrLookup> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let said = stderr.trim();
        // gh's message for a non-GitHub remote also says to run `gh auth login`.
        if said.contains("known GitHub host") {
            return Ok(PrLookup::Unknown("origin isn't on GitHub".into()));
        }
        if said.contains("gh auth login") || said.contains("not logged in") {
            return Ok(PrLookup::Unknown(
                "the GitHub CLI has no account logged in".into(),
            ));
        }
        bail!("gh pr list: {said}");
    }
    let prs: Vec<RawPr> =
        serde_json::from_slice(&output.stdout).context("read gh pr list's JSON")?;
    Ok(match prs.into_iter().next() {
        Some(pr) => PrLookup::Open(Pr {
            number: pr.number,
            url: pr.url,
        }),
        None => PrLookup::None,
    })
}

#[derive(serde::Deserialize)]
struct RawPr {
    number: u64,
    url: String,
}

#[cfg(test)]
pub mod fake {
    use super::*;

    /// Answers every lookup with the same [`PrLookup`], or an error.
    pub struct FakePrs(pub std::result::Result<PrLookup, String>);

    impl PullRequests for FakePrs {
        fn open_on(&self, _repo: &Path, _branch: &str) -> Result<PrLookup> {
            self.0.clone().map_err(|e| anyhow::anyhow!(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn an_open_pr_is_named_by_number_and_url() {
        let out = output(
            0,
            r#"[{"number":42,"url":"https://github.com/o/r/pull/42"}]"#,
            "",
        );

        assert_eq!(
            lookup_from(&out).unwrap(),
            PrLookup::Open(Pr {
                number: 42,
                url: "https://github.com/o/r/pull/42".into()
            })
        );
    }

    #[test]
    fn no_open_pr_is_none() {
        assert_eq!(lookup_from(&output(0, "[]", "")).unwrap(), PrLookup::None);
    }

    #[test]
    fn no_account_is_unknown_not_an_error() {
        let out = output(
            4,
            "",
            "To get started with GitHub CLI, please run:  gh auth login\n",
        );

        assert_eq!(
            lookup_from(&out).unwrap(),
            PrLookup::Unknown("the GitHub CLI has no account logged in".into())
        );
    }

    #[test]
    fn an_origin_off_github_is_unknown_not_an_error() {
        let out = output(
            1,
            "",
            "none of the git remotes configured for this repository point to a known GitHub \
             host. To tell gh about a new GitHub host, please use `gh auth login`\n",
        );

        assert_eq!(
            lookup_from(&out).unwrap(),
            PrLookup::Unknown("origin isn't on GitHub".into())
        );
    }

    #[test]
    fn any_other_failure_is_an_error() {
        let out = output(1, "", "error connecting to api.github.com\n");

        assert_eq!(
            format!("{:#}", lookup_from(&out).unwrap_err()),
            "gh pr list: error connecting to api.github.com"
        );
    }

    #[test]
    fn reads_owner_and_repo_from_each_remote_url_shape() {
        let want = Some(("github.com".to_string(), "o/r".to_string()));
        for url in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "https://user@github.com/o/r/",
            "ssh://git@github.com/o/r.git",
            "ssh://git@github.com:22/o/r.git",
            "git@github.com:o/r.git",
        ] {
            assert_eq!(github_repo(url), want, "{url}");
        }
        for url in ["/srv/origin.git", "file:///srv/o/r", "https://github.com/o"] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    /// A repo whose origin is `url`, and a fake gh that records its
    /// arguments and lists `stdout`.
    fn fake_gh(url: &str, stdout: &str) -> (tempfile::TempDir, GhCli) {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["remote", "add", "origin", url]);
        let program = dir.path().join("gh");
        let args = dir.path().join("args");
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho '{stdout}'\n",
            args.display()
        );
        crate::test_fakes::executable(&program, script);
        (dir, GhCli { program })
    }

    #[test]
    fn asks_about_origins_repo_by_name() {
        let (dir, gh) = fake_gh("git@github.com:o/r.git", "[]");

        assert_eq!(gh.open_on(dir.path(), "feat/x").unwrap(), PrLookup::None);

        assert_eq!(
            std::fs::read_to_string(dir.path().join("args")).unwrap(),
            "pr list -R github.com/o/r --state open --limit 1 --json number,url --head feat/x\n"
        );
    }

    #[test]
    fn an_origin_off_github_never_runs_gh() {
        let (dir, gh) = fake_gh("/srv/origin.git", "[]");

        assert_eq!(
            gh.open_on(dir.path(), "feat/x").unwrap(),
            PrLookup::Unknown("origin isn't on GitHub".into())
        );
        assert!(!dir.path().join("args").exists());
    }

    #[test]
    fn a_missing_gh_is_unknown() {
        let (dir, mut gh) = fake_gh("https://github.com/o/r", "[]");
        gh.program = dir.path().join("no-such-gh");

        assert_eq!(gh.open_on(dir.path(), "feat/x").unwrap(), no_gh());
    }

    /// A fake gh that records its arguments and answers `pr view` with
    /// `stdout`, or fails with `stderr`.
    fn fake_gh_view(answer: Result<&str, &str>) -> (tempfile::TempDir, GhCli) {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("gh");
        let args = dir.path().join("args");
        let say = match answer {
            Ok(stdout) => format!("echo '{stdout}'"),
            Err(stderr) => format!("echo '{stderr}' >&2; exit 1"),
        };
        let script = format!("#!/bin/sh\necho \"$@\" >> '{}'\n{say}\n", args.display());
        crate::test_fakes::executable(&program, script);
        (dir, GhCli { program })
    }

    const PR: &str = "https://github.com/o/r/pull/12";

    #[test]
    fn asks_gh_for_a_prs_head_branch() {
        let (dir, gh) = fake_gh_view(Ok("feat/LH-1-x"));

        assert_eq!(gh.head_of(PR).unwrap(), Some("feat/LH-1-x".into()));

        assert_eq!(
            std::fs::read_to_string(dir.path().join("args")).unwrap(),
            format!("pr view {PR} --json headRefName --jq .headRefName\n")
        );
    }

    #[test]
    fn a_pr_github_doesnt_have_has_no_head() {
        let (_dir, gh) = fake_gh_view(Err(
            "GraphQL: Could not resolve to a PullRequest with the number of 12. (repository.pullRequest)",
        ));

        assert_eq!(gh.head_of(PR).unwrap(), None);
    }

    #[test]
    fn a_gh_that_fails_otherwise_is_an_error() {
        let (_dir, gh) = fake_gh_view(Err(
            "To get started with GitHub CLI, please run:  gh auth login",
        ));

        let err = gh.head_of(PR).unwrap_err();

        assert!(format!("{err:#}").contains("gh auth login"), "{err:#}");
    }

    #[test]
    fn no_gh_to_ask_is_an_error() {
        let (dir, mut gh) = fake_gh_view(Ok("feat/x"));
        gh.program = dir.path().join("no-such-gh");

        let err = gh.head_of(PR).unwrap_err();

        assert!(
            format!("{err:#}").contains("no GitHub CLI (gh) on PATH"),
            "{err:#}"
        );
    }
}
