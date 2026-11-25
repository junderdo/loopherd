//! What each init question controls and when to answer it otherwise, shown
//! above its prompt on a terminal. Kept here so every caller asking a
//! question shows the same text.

pub const REPO: &str = "\
The checkout the herd's members branch from and push to.
Change it to set up a herd for a repo other than the one you're in.";

pub const HERD_NAME: &str = "\
Names the herd's file, its lead (lead-<name>) and its members (<name>-<n>).
Change it when another herd has this name or you want a shorter one.";

pub const TRACKER: &str = "\
Where the herd finds issues to work: GitHub, markdown files in the repo, or
another tracker you describe. Pick the one this repo already uses.";

pub const GITHUB: &str = "\
The GitHub repo the herd makes labels on and checks for merged pull requests.
Change it when origin is a fork or a host alias rather than that repo.";

pub const CHECKS: &str = "\
Commands each worker runs before calling its work done; all must pass.
Change them to match what this repo's CI runs.";

pub const MAX: &str = "\
How many workers the herd runs at once, each in its own pane and worktree.
Lower it to save tokens and load; raise it for a long, parallel backlog.";

pub const LEAD: &str = "\
A lead is a member that feeds the queue, watches workers and verifies PRs.
Say no to run the herd yourself, or for a herd of one or two workers.";

pub const CONTEXT_LIMIT: &str = "\
How full a worker's context gets before it hands off to a fresh session.
Lower it if workers lose track late in long tasks; Enter keeps the default.";

pub const GH_ACCOUNT: &str = "\
The gh login the herd's workers push and open pull requests as.
Change it when the herd should work as another account, such as a bot.";

pub const DEFAULT_BRANCH: &str = "\
The branch workers cut their branches from and open pull requests against.
Change it only when origin's default isn't where work merges.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_explanation_is_one_or_two_lines_of_at_most_80_columns() {
        for text in [
            REPO,
            HERD_NAME,
            TRACKER,
            GITHUB,
            CHECKS,
            MAX,
            LEAD,
            CONTEXT_LIMIT,
            GH_ACCOUNT,
            DEFAULT_BRANCH,
        ] {
            let lines: Vec<&str> = text.lines().collect();
            assert!((1..=2).contains(&lines.len()), "{text}");
            for line in lines {
                assert!(line.chars().count() <= 80, "{line}");
            }
        }
    }
}
