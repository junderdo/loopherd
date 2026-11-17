//! Open pull requests on GitHub, asked through the GitHub CLI (`gh`).

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

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

pub struct GhCli;

impl PullRequests for GhCli {
    fn open_on(&self, repo: &Path, branch: &str) -> Result<PrLookup> {
        let mut gh = Command::new("gh");
        gh.args(["pr", "list", "--state", "open", "--limit", "1"])
            .args(["--json", "number,url", "--head", branch])
            .current_dir(repo)
            .env("GH_PROMPT_DISABLED", "1");
        let output = match run_within(&mut gh, remote_timeout()?) {
            Err(e) if is_not_found(&e) => {
                return Ok(PrLookup::Unknown("no GitHub CLI (gh) on PATH".into()));
            }
            result => result.context("run gh pr list")?,
        };
        let Some(output) = output else {
            bail!("gh pr list didn't answer in time");
        };
        lookup_from(&output)
    }
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn lookup_from(output: &Output) -> Result<PrLookup> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let said = stderr.trim();
        if said.contains("gh auth login") || said.contains("not logged in") {
            return Ok(PrLookup::Unknown(
                "the GitHub CLI has no account logged in".into(),
            ));
        }
        if said.contains("known GitHub host") || said.contains("no git remotes") {
            return Ok(PrLookup::Unknown("origin isn't on GitHub".into()));
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
            "none of the git remotes configured for this repository point to a known GitHub host.\n",
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
}
