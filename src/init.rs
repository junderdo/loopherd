//! `loopherd init`: a working herd for the GitHub repo around a directory.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::context::Limit;
use crate::herd::{self, MergeGuard, Paths};
use crate::worktree::{checkout_root, same_dir};

mod agents;
mod ask;
mod explain;
mod rerun;
mod tracker;

pub use tracker::Choice as Tracker;

const HERD: &str = include_str!("init/herd.toml");
const LEAD_TABLE: &str = include_str!("init/lead-table.toml");
const GITHUB_SOURCE: &str = include_str!("init/github-source.toml");
const GITHUB_ISSUE: &str = include_str!("init/github-issue.toml");
const LOCAL_SOURCE: &str = include_str!("init/local-source.toml");
/// The local tracker's pipeline: `list` is `ready` for the source, `backlog` for the backlog.
const LOCAL_ISSUES: &str = include_str!("init/local-issues.sh");
const LOCAL_ISSUE: &str = include_str!("init/local-issue.toml");
const OTHER_ISSUE: &str = include_str!("init/other-issue.toml");
const PLACEHOLDER_SOURCE: &str = include_str!("init/placeholder-source.toml");
const MERGED_GITHUB: &str = include_str!("init/merged-github.toml");
const LEAD: &str = include_str!("init/lead.md");
const GROOM: &str = include_str!("init/groom.toml");
const GITHUB_WAYFINDING: &str = include_str!("init/github-wayfinding.toml");
const LOCAL_WAYFINDING: &str = include_str!("init/local-wayfinding.toml");
const OTHER_WAYFINDING: &str = include_str!("init/other-wayfinding.toml");

/// Each wayfinding label, the skill its task is resolved with, and the label's description.
const WAYFINDING_SKILLS: [(&str, &str, &str); 3] = [
    (
        "wayfinding:research",
        "research",
        "Resolved with the research skill",
    ),
    (
        "wayfinding:domain",
        "domain-modeling",
        "Resolved with the domain-modeling skill",
    ),
    (
        "wayfinding:prototype",
        "prototype",
        "Resolved with the prototype skill",
    ),
];
const GITHUB_ROUTING_NOTE: &str = "
# A wayfinding:research, wayfinding:domain or wayfinding:prototype label
# routes the issue to the wayfinding template, as type docs with the research,
# domain-modeling or prototype skill; any other issue goes to issue.";
const GITHUB_ROUTE: &str = r#"  | ([$labels[] | {"wayfinding:research": "research", "wayfinding:domain": "domain-modeling", "wayfinding:prototype": "prototype"}[.] | values][0]) as $wayfinding
"#;
const GITHUB_TASK_HEAD: &str = r"\(.number) \($type) \($skill)";
const GITHUB_ROUTED_TASK: &str = r#""issue \(.number) \($type) \($skill)""#;
const GITHUB_WAYFINDING_TASK: &str = r#""wayfinding \(.number) docs \($wayfinding)""#;
const GITHUB_GRILL_BATCH_TASK: &str = r#""grill-batch \(.number) docs grilling""#;
const GITHUB_READY: &str = r#"any($labels[]; . == "@READY_FOR_AGENT_LC@")"#;
/// A grilling issue is claimable only with grill:batch, ready label or not.
const GITHUB_GRILL_BATCH_CLAIMABLE: &str = r#"(if any($labels[]; . == "grilling") then any($labels[]; . == "grill:batch") else any($labels[]; . == "@READY_FOR_AGENT_LC@") end)"#;
const GITHUB_GRILLING: &str = r#"  | select(@GITHUB_CLAIMABLE@)
  | any($labels[]; . == "grilling") as $grilling
"#;
const GITHUB_GRILL_BATCH_NOTE: &str = "
# An issue labelled grilling and grill:batch goes to the grill-batch template,
# ready or not, as type docs with the grilling skill; a grilling issue without
# grill:batch waits for a live grill, so it isn't listed.";
const LOCAL_GRILL_BATCH_NOTE: &str = "
# A Status: line with grilling and grill:batch sends the issue to the
# grill-batch template, ready or not, as type docs with the grilling skill; a
# grilling issue without grill:batch waits for a live grill, so it isn't listed.";
const OTHER_GRILL_BATCH_NOTE: &str = "
# grill-batch, as type docs with the grilling skill, for a task marked grilling
# and grill:batch, ready or not (a grilling task without grill:batch waits for
# a live grill, so leave it out),";
const ANY_OTHER_ISSUE: &str = "\n# Any other issue goes to issue.";
const BACKLOG_GRILLING_NOTE: &str =
    "\n# A grilling issue without grill:batch stays too: it waits for a live grill.";
const LOCAL_READY: &str = r#"has(f, "@READY_FOR_AGENT_LC@")"#;
const LOCAL_GRILL_BATCH_CLAIMABLE: &str =
    r#"(has(f, "grilling") ? has(f, "grill:batch") : has(f, "@READY_FOR_AGENT_LC@"))"#;
const LOCAL_ROUTING_NOTE: &str = "
# A wayfinding:research, wayfinding:domain or wayfinding:prototype word routes
# the issue to the wayfinding template, as type docs with the research,
# domain-modeling or prototype skill; any other issue goes to issue.";
const LOCAL_PRINT: &str = "      print id, type, skill, slug, f";
/// The wayfinding-only routing, as init wrote it before grill-batch, so
/// `herd upgrade` still finds those herds' sources init's own.
const LOCAL_WAYFINDING_PRINT: &str = r#"      template = "issue"
      if (has(f, "wayfinding:research")) { template = "wayfinding"; type = "docs"; skill = "research" }
      else if (has(f, "wayfinding:domain")) { template = "wayfinding"; type = "docs"; skill = "domain-modeling" }
      else if (has(f, "wayfinding:prototype")) { template = "wayfinding"; type = "docs"; skill = "prototype" }
      # A wayfinding member name is "-research" longer, so its id is cut to fit 9 less.
      if (template == "wayfinding") {
        room -= 9
        id = id_of(f)
        room += 9
        if (length(id) + 1 + length(num[f]) > room - 9) {
          print "skipped " f ": the herd name leaves no room for its research name" > "/dev/stderr"
          continue
        }
        id = id "-" num[f]
      }
      print template, id, type, skill, slug, f"#;
const LOCAL_ROUTE_START: &str = r#"      template = "issue"; word = ""
      if (0) {}
"#;
const LOCAL_GRILL_BATCH_ROUTE: &str = r#"      else if (has(f, "grilling")) { template = "grill-batch"; type = "docs"; skill = "grilling"; word = "grill-batch" }
"#;
const LOCAL_WAYFINDING_ROUTE: &str = r#"      else if (has(f, "wayfinding:research")) { template = "wayfinding"; type = "docs"; skill = "research"; word = "research" }
      else if (has(f, "wayfinding:domain")) { template = "wayfinding"; type = "docs"; skill = "domain-modeling"; word = "research" }
      else if (has(f, "wayfinding:prototype")) { template = "wayfinding"; type = "docs"; skill = "prototype"; word = "research" }
"#;
const LOCAL_ROUTE_END: &str = r#"      # A routed member name is "-<word>" longer, so its id is cut to fit that much less.
      if (word != "") {
        longer = length(word) + 1
        room -= longer
        id = id_of(f)
        room += longer
        if (length(id) + 1 + length(num[f]) > room - longer) {
          print "skipped " f ": the herd name leaves no room for its " word " name" > "/dev/stderr"
          continue
        }
        id = id "-" num[f]
      }
      print template, id, type, skill, slug, f"#;
const OTHER_ROUTING_HEAD: &str = ": <template> is";
const OTHER_WAYFINDING_NOTE: &str = "
# wayfinding for a task marked wayfinding:research, wayfinding:domain or
# wayfinding:prototype, whose <skill> is research, domain-modeling or prototype,";
const OTHER_ROUTING_TAIL: &str = "\n# and issue for any other";
const GITHUB_DONE: &str = include_str!("init/github-done.toml");
const LOCAL_DONE: &str = include_str!("init/local-done.toml");
const OTHER_DONE: &str = include_str!("init/other-done.toml");
const GRILL: &str = include_str!("init/grill.toml");
const GRILL_PROMPT: &str = include_str!("init/grill.md");
const GRILL_BATCH: &str = include_str!("init/grill-batch.toml");
const GRILL_BATCH_PROMPT: &str = include_str!("init/grill-batch.md");
const GITHUB_BATCH_DONE: &str = include_str!("init/github-batch-done.toml");
const LOCAL_BATCH_DONE: &str = include_str!("init/local-batch-done.toml");
const OTHER_BATCH_DONE: &str = include_str!("init/other-batch-done.toml");
const GITHUB_BATCH_POST: &str =
    "`@GH_TOKEN@ gh issue comment {number} --repo @OWNER_REPO@ --body-file <file>`.";
const GITHUB_BATCH_WAIT: &str =
    "`@GH_TOKEN@ gh issue edit {number} --repo @OWNER_REPO@ --add-label @NEEDS_INFO@`.";
const LOCAL_BATCH_POST: &str = "the issue lives on @BRANCH@, so commit it there,
   leaving {branch} untouched: `git fetch -q origin @BRANCH@ && git switch --detach origin/@BRANCH@`,
   append the batch under {path}'s `## Comments` (adding the heading if it's
   missing), commit only {path}, and `@BATCH_PUSH@ origin HEAD:@BRANCH@`; if the
   push is refused, fetch and redo it.";
const LOCAL_BATCH_WAIT: &str = "on @BRANCH@ the same way, add `@NEEDS_INFO@` to its `Status:` line,
   commit only {path} and push.";
const LOCAL_BATCH_POST_EXCEPT: &str = ", but for posting the batch itself on @BRANCH@";
const OTHER_BATCH_POST: &str = "comment it on task {number}. (fill in)";
const OTHER_BATCH_WAIT: &str = "mark task {number} @NEEDS_INFO@. (fill in)";
const LEAD_GRILLING: &str = "Grilling is always the human's.";
const LEAD_GRILL_BATCH: &str =
    "Grilling is the human's, but for a batch grill: a grilling task marked
grill:batch goes to a grill-batch member, which posts its batch, marks the
task @NEEDS_INFO@ and stops, so feed reaps it. Once the human answers there,
the shepherd drops @NEEDS_INFO@ and runs `loopherd release <member>`; your next
feed starts a fresh member, which writes the decision.";

const AGREED: &str = "When the human agrees the outcome, ";
const GITHUB_GIT_PUSH: &str =
    "@GH_TOKEN@ git -c credential.helper= -c credential.helper='!gh auth git-credential' push";
const GRILL_PUSH: &str = "push the branch as @ACCOUNT@:
  `@GH_TOKEN@ git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin HEAD`.
  Open a PR with `@GH_TOKEN@ gh pr create --repo @OWNER_REPO@ --base @BRANCH@ --head {branch} --assignee @ACCOUNT@ --title \"<title>\" --body \"<summary>\"`";
const GITHUB_GRILL_READ: &str = "the issue and every comment:
`@GH_TOKEN@ gh issue view {number} --repo @OWNER_REPO@ --comments`.";
const GITHUB_GRILL_CLAIM: &str =
    "`@GH_TOKEN@ gh issue comment {number} --repo @OWNER_REPO@ --body \"claimed by {name}\"`,
then `@GH_TOKEN@ gh issue edit {number} --repo @OWNER_REPO@ --add-assignee @ACCOUNT@`.";
const GITHUB_GRILL_HAND_IN: &str = "When the human agrees the outcome, @GRILL_PUSH@,
  the body ending with the line `Closes #{number}`.";
const LOCAL_GRILL_READ: &str = "{path} in full, comments
included. The issue is a markdown file in the repo; its `Status:` line near
the top holds its triage state, and comments go under a `## Comments`
heading at the bottom.";
const LOCAL_GRILL_CLAIM: &str = "set its `Status:` line to
`claimed`, append `claimed by {name}` under `## Comments` (adding the heading
if it's missing), and commit.";
const LOCAL_GRILL_RESOLVE: &str =
    "When the human agrees the outcome, set {path}'s `Status:` line to
  `resolved`, append a one-line summary under `## Comments` and commit, so
  the issue closes when your branch merges. Then";
const LOCAL_GRILL_PR: &str = "@GRILL_PUSH@.";
const LOCAL_GRILL_PUSH: &str = "push the branch with
  `git push -u origin HEAD`. Open no pull request; the human merges the
  branch into @BRANCH@.";
const OTHER_GRILL_READ: &str = "task {number} and every comment on it. (fill in)";
const OTHER_GRILL_CLAIM: &str = "comment `claimed by {name}` on task {number}. (fill in)";
const OTHER_GRILL_PR: &str = "When the human agrees the outcome, @GRILL_PUSH@.
  Link it on task {number}, so the task closes when the PR merges. (fill in)";
const OTHER_GRILL_PUSH: &str = "When the human agrees the outcome, push the branch with
  `git push -u origin HEAD`, open no pull request, and hand it in on task
  {number}, so the task closes when the human merges it into @BRANCH@. (fill in)";

const GITHUB_GROOM_READ: &str =
    "with `@GH_TOKEN@ gh issue view <n> --repo @OWNER_REPO@ --comments`";
const GITHUB_GROOM_FORMS: &str = "\
- mark ready: `@GH_TOKEN@ gh issue edit <n> --repo @OWNER_REPO@ --add-label @READY_FOR_AGENT@`
- needs spec: `@GH_TOKEN@ gh issue comment <n> --repo @OWNER_REPO@ --body \"<what's missing>\"`
- close: `@GH_TOKEN@ gh issue close <n> --repo @OWNER_REPO@ --reason \"not planned\"  # <reason>`";
const GITHUB_GROOM_NEVER: &str = "Never add @READY_FOR_AGENT@, close or move an issue, or add or remove @NEEDS_INFO@ or blocked.";
const LOCAL_GROOM_READ: &str = "issue file at the path it prints";
const LOCAL_GROOM_FORMS: &str = "\
Start each command with `cd @CHECKOUT@ && ([ \"$(git branch --show-current)\" = @BRANCH@ ] && git pull -q --ff-only origin @BRANCH@ && [ -z \"$(git rev-list origin/@BRANCH@..HEAD)\" ] && git diff --quiet HEAD -- <path> || (echo \"not run: check out @BRANCH@ with no unpushed commits or edits to <path>\" >&2; false)) && `
and end it, before any `# <reason>`, with ` && git push -q origin HEAD:@BRANCH@`, so it commits only
its own edit, on an up-to-date @BRANCH@ with nothing unpushed:
- mark ready: `sed -i 's/^Status:.*/& @READY_FOR_AGENT@/' <path> && git commit -qm \"Mark <path> ready\" -- <path>`
- needs spec: `(grep -q '^## Comments' <path> || printf '\\n## Comments\\n' >> <path>) && printf '\\n%s\\n' \"<what's missing>\" >> <path> && git commit -qm \"Comment on <path>\" -- <path>`
- close: `sed -i 's/^Status:.*/Status: closed/' <path> && git commit -qm \"Close <path>\" -- <path>  # <reason>`";
const LOCAL_GROOM_NEVER: &str = "Never change a Status: line, which marks a task ready, closes it and holds @NEEDS_INFO@ and\nblocked, and commit nothing.";
const OTHER_GROOM_READ: &str = "with the tracker's own command (fill in)";
const OTHER_GROOM_FORMS: &str = "\
- mark ready: (fill in)
- needs spec, a comment saying what's missing: (fill in)
- close: (fill in)";
const OTHER_GROOM_NEVER: &str =
    "Never mark a task ready, close or move one, or add or remove @NEEDS_INFO@ or blocked.";

const LOCAL_GH_NOTE: &str = "\nRun every gh command as @ACCOUNT@, prefixed with
`@GH_TOKEN@` as in the commands below.
";
const LOCAL_PR_HAND_IN: &str = "6. Push the branch as @ACCOUNT@:
   `@GH_TOKEN@ git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin HEAD`.
   Open a PR with `@GH_TOKEN@ gh pr create --repo @OWNER_REPO@ --base @BRANCH@ --head {branch} --assignee @ACCOUNT@ --title \"<title>\" --body \"<summary>\"`.";
const LOCAL_PUSH_HAND_IN: &str =
    "6. Push the branch with `git push -u origin HEAD`. Open no pull request;
   the human merges the branch into @BRANCH@.";
const LEAD_GH: &str = "Run every gh command as @ACCOUNT@, prefixed with
`GH_TOKEN=$(gh auth token --user @ACCOUNT@)`. ";
const LEAD_GH_CREDENTIALS: &str = "Your gh already runs on the herd's own token, so
run gh commands as they are, with no `GH_TOKEN=` prefix. ";
const LEAD_ON_MERGED: &str = "On `<member> merged`, run `loopherd feed {herd}`,
   which retires it once its issue is closed.";
const LEAD_ON_BRANCH_MERGED: &str = "No `<member> merged` line comes, as the herd
   has no merged check: once a worker's branch merges, run `loopherd feed {herd}`,
   which retires it once its issue is closed.";
const LEAD_PR_MERGE_ASK: &str = "With merge off, a merge question carries the PR
link, any PR that must merge first, and the line
`! GH_TOKEN=$(gh auth token --user @ACCOUNT@) gh pr merge <n> --repo @OWNER_REPO@ --merge --match-head-commit <full sha>`
for the head your reviewer verified. Single-quote that question ('...', not
\"...\"): double quotes run the $(...) and leak the token.";
const LEAD_PR_MERGE_ASK_CREDENTIALS: &str = "With merge off, a merge question carries
the PR link, any PR that must merge first, and the full sha of the head your
reviewer verified. Your token can't merge, so the human merges from their own
terminal: `gh pr merge <n> --repo @OWNER_REPO@ --merge --admin --match-head-commit <full sha>`.";
const OTHER_PR_HAND_IN: &str = "5. Push the branch as @ACCOUNT@:
   `@GH_TOKEN@ git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin HEAD`.
   Then open a pull request into @BRANCH@ with `@GH_TOKEN@ gh pr create --repo @OWNER_REPO@ --base @BRANCH@ --head {branch} --assignee @ACCOUNT@ --title \"<title>\" --body \"<summary>\"`.
   Link it on task {number}, so the task closes when the PR merges. (fill in)";
const OTHER_PUSH_HAND_IN: &str =
    "5. Push the branch with `git push -u origin HEAD`, open no pull request, and hand it
   in on task {number}, so the task closes when the human merges it into @BRANCH@. (fill in)";
/// Marks what the human fills for a tracker init has no snippets for.
const FILL_IN: &str = "Fill in from docs/agents/issue-tracker.md";
const LEAD_BRANCH_MERGE_ASK: &str = "With merge off, a merge question carries the
branch, any branch that must merge first, and the full sha of the head your
reviewer verified.";

const NO_MERGED: &str =
    "without GitHub the herd has no merged check, so after_merge never runs for its members";
const NO_MERGED_NOTE: &str =
    "Without GitHub it has no merged check, so after_merge never runs for its members.\n";

pub struct Request<'a> {
    pub dir: &'a Path,
    pub herd: Option<&'a str>,
    /// Empty to take the ones suggested by the repo's build files.
    pub checks: &'a [String],
    pub gh_account: Option<&'a str>,
    /// `<owner>/<name>`, for an origin init can't read as GitHub.
    pub github: Option<&'a str>,
    /// Writes a local-tracker herd without GitHub even when origin is GitHub.
    pub no_github: bool,
    /// The tracker to record when the repo has no tracker doc.
    pub tracker: Option<Tracker>,
    pub max: Option<u64>,
    pub lead: Option<bool>,
    pub merge_guard: Option<MergeGuard>,
    /// Tokens or a percentage of the window, as the human gave it.
    pub context_limit: Option<String>,
    /// Templates to add without asking.
    pub with: &'a [Template],
    /// Herds whose lead is running, for a re-run's migration offers.
    pub running_leads: &'a BTreeSet<String>,
}

/// A template init offers beyond the herd's own, asked on a terminal.
#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Template {
    Groom,
    Wayfinding,
    GrillBatch,
}

#[derive(Clone)]
struct GitHub {
    /// `<owner>/<name>`.
    slug: String,
    account: String,
    token: String,
}

/// What init writes, settled from flags, inferred values and, on a
/// terminal, the human's answers.
#[derive(Clone)]
struct Herd {
    repo: PathBuf,
    name: String,
    github: Option<GitHub>,
    tracker: tracker::Tracker,
    branch: String,
    checks: Vec<String>,
    max: u64,
    lead: bool,
    /// The lead's switch lines, from `[lead_defaults]`.
    switches: String,
    /// How a lead with merge off is kept from merging; `None` with merge on.
    merge_guard: Option<MergeGuard>,
    context_limit: Option<Limit>,
    /// The limit that applies when `context_limit` is unset.
    config_limit: String,
    docs: Vec<agents::Doc>,
    /// What the human writes in the repo that init wouldn't.
    agents_steps: Vec<String>,
    groom: bool,
    /// Whether it has the attended grill template; only a herd from before it lacks one.
    grill: bool,
    /// Routes the queue's wayfinding tasks to their own template.
    wayfinding: bool,
    /// Routes the queue's batch grills to their own template.
    grill_batch: bool,
}

/// Writes the herd file, its lead and grill prompts and creates missing labels,
/// returning what it did and the next steps. Refuses, changing nothing,
/// when any of them exists. On a terminal it asks for each value, showing
/// what it inferred, and confirms before writing.
pub fn init(paths: &Paths, req: Request) -> Result<Vec<String>> {
    let ask = ask::Asker {
        terminal: ask::interactive(),
    };
    let no_github = req.no_github;
    let repo = ask_repo(req.dir, &ask)?;
    if let Some(name) = rerun_of(paths, &repo, req.herd)? {
        return rerun::rerun(paths, repo, name, req, &ask);
    }
    let herd = settle(paths, repo, req, &ask, Mode::Write)?;
    if ask.terminal {
        eprint!("{}", summary(&herd));
        if !ask.yes("Write this herd?", "", None, true)? {
            bail!("nothing written");
        }
    } else if herd.github.is_none() {
        eprintln!("warning: {NO_MERGED}");
        if !no_github && has_origin(&herd.repo) {
            eprintln!(
                "note: if origin is GitHub behind an ssh host alias, pass --repo <owner>/<name> \
                 to keep the merged check"
            );
        }
    }
    write(paths, herd)
}

/// A unified diff from each file init wrote to what it would write now, `None`
/// when they match. Asks nothing and changes nothing; an absent file diffs
/// from empty.
pub fn diff(paths: &Paths, req: Request) -> Result<Option<String>> {
    let ask = ask::Asker { terminal: false };
    let repo = ask_repo(req.dir, &ask)?;
    let herd = settle(paths, repo, req, &ask, Mode::Diff)?;
    let mut files = rendered(paths, &herd)?.files;
    for doc in &herd.docs {
        let path = herd.repo.join(doc.path);
        let text = match doc.append {
            true => tracker::read(&path)?.unwrap_or_default() + &doc.text,
            false => doc.text.clone(),
        };
        files.push((path, text));
    }
    let mut out = String::new();
    for (path, now) in files {
        let was = tracker::read(&path)?;
        let shown = path.display().to_string();
        let old_name = match was {
            Some(_) => shown.as_str(),
            None => "/dev/null",
        };
        let was = was.unwrap_or_default();
        if was == now {
            continue;
        }
        let text = similar::TextDiff::from_lines(&was, &now);
        out.push_str(
            &text
                .unified_diff()
                .header(old_name, &format!("{shown} (init now)"))
                .to_string(),
        );
    }
    Ok((!out.is_empty()).then_some(out))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Write,
    /// Renders over the herd's existing files rather than refusing them.
    Diff,
}

fn ask_repo(dir: &Path, ask: &ask::Asker) -> Result<PathBuf> {
    let here = checkout_root(dir).with_context(|| {
        format!(
            "can't read a git repo at {}; run init inside one",
            dir.display()
        )
    })?;
    ask.value(
        "Repo",
        explain::REPO,
        None,
        Some(&here.to_string_lossy()),
        "",
        |answer| {
            checkout_root(Path::new(answer))
                .with_context(|| format!("can't read a git repo at {answer}"))
        },
    )
}

/// The herd that already runs `repo`, which init re-runs on; refuses a
/// `--herd` naming another.
fn rerun_of(paths: &Paths, repo: &Path, given: Option<&str>) -> Result<Option<String>> {
    let Some(running) = herd_running(paths, repo)? else {
        return Ok(None);
    };
    if let Some(given) = given
        && given != running
    {
        refuse_other_repo(paths, given, repo)?;
        bail!(
            "herd {running} already runs {}; init writes one herd per repo",
            repo.display()
        );
    }
    Ok(Some(running))
}

fn settle(
    paths: &Paths,
    repo: PathBuf,
    req: Request,
    ask: &ask::Asker,
    mode: Mode,
) -> Result<Herd> {
    let inferred = match mode {
        // A named herd needs no inference, so another herd's unreadable file can't stop it.
        Mode::Diff if req.herd.is_none() => herd_running(paths, &repo)?,
        _ => None,
    }
    .or_else(|| repo.file_name().map(|n| n.to_string_lossy().into_owned()));
    let name = ask.value(
        "Herd name",
        explain::HERD_NAME,
        req.herd,
        inferred.as_deref(),
        "the repo has no directory name; pass --herd",
        |name| {
            let name = herd_name(name)?;
            match mode {
                Mode::Write => refuse_taken_name(paths, &name, &repo)?,
                Mode::Diff => refuse_other_repo(paths, &name, &repo)?,
            }
            Ok(name)
        },
    )?;
    let batch_file = grill_batch_file(paths, &name);
    if mode == Mode::Write && req.with.contains(&Template::GrillBatch) {
        refuse_grill_batch_prompt(&batch_file)?;
    }
    let herd_file = paths.herds.join(format!("{name}.toml"));
    let recorded = match mode {
        Mode::Diff => Recorded::read(&herd_file),
        Mode::Write => None,
    };
    let choice = match req.tracker {
        None if mode == Mode::Diff
            && tracker::read(&repo.join(tracker::TRACKER_DOC))?.is_none() =>
        {
            Some(Tracker::GitHub)
        }
        choice => choice,
    };
    let mut tracker = tracker::resolve(&repo, choice)?;
    if tracker.kind == tracker::Kind::Local && !has_origin(&repo) {
        bail!(
            "{} has no origin remote; a herd's members branch from origin and push to it, \
             so add one",
            repo.display()
        );
    }
    let origin = github_repo(&repo).map(|(owner, name)| format!("{owner}/{name}"));
    let parse_slug = |slug: &str| match slug.split_once('/') {
        Some((owner, name)) if is_github_name(owner) && is_github_name(name) => {
            Ok((owner.to_string(), name.to_string()))
        }
        _ => bail!("--repo {slug:?} is not <owner>/<name>"),
    };
    let recorded_github = recorded.as_ref().and_then(|r| r.github.as_deref());
    if req.no_github && tracker.kind != tracker::Kind::Local {
        // A tracker read from the repo's doc writes no doc of its own.
        bail!(match tracker.writes.is_empty() {
            false => "--no-github goes with --tracker local".to_string(),
            true => format!(
                "--no-github needs a local tracker, and {} names another",
                tracker::TRACKER_DOC
            ),
        });
    }
    let recorded_no_github =
        tracker.kind == tracker::Kind::Local && recorded.as_ref().is_some_and(|r| !r.has_merged);
    let slug_or_none = |slug: &str| match slug {
        "" => Ok(None),
        slug => parse_slug(slug).map(Some),
    };
    // Only a GitHub tracker needs GitHub; the others use it only for PRs.
    let github = match (&origin, tracker.kind) {
        _ if req.no_github || (recorded_no_github && req.github.is_none()) => None,
        (Err(_), tracker::Kind::Local | tracker::Kind::Other) => ask.value(
            "GitHub repo (owner/name); Enter for none",
            explain::GITHUB,
            req.github,
            Some(recorded_github.unwrap_or("")),
            "",
            slug_or_none,
        )?,
        (Ok(origin), tracker::Kind::Local) if ask.terminal && req.gh_account.is_none() => ask
            .value(
                &format!("GitHub repo (owner/name, origin is {origin}); Enter for none"),
                explain::GITHUB,
                req.github,
                Some(""),
                "",
                slug_or_none,
            )?,
        // Off a terminal, a GitHub origin keeps its merged check unless --no-github.
        _ => Some(
            ask.value(
                "GitHub repo (owner/name)",
                explain::GITHUB,
                req.github,
                recorded_github.or(origin.as_ref().ok().map(String::as_str)),
                &origin
                    .as_ref()
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default(),
                parse_slug,
            )?,
        ),
    };
    if github.is_none() && req.gh_account.is_some() {
        bail!("--gh-account needs a GitHub repo; pass --repo <owner>/<name> or drop it");
    }
    for check in req.checks {
        prompt_text(check)?;
    }
    let defaults = lead_defaults(paths)?;
    if defaults.merge && req.merge_guard.is_some() {
        bail!("--merge-guard is for a lead with merge off, and [lead_defaults] turns merge on");
    }

    let recorded_checks = recorded.as_ref().map(|r| r.checks.as_slice());
    let given = match req.checks {
        [] => recorded_checks.unwrap_or_default(),
        given => given,
    };
    let checks = match given {
        [] => {
            let suggested = suggested_checks(&repo);
            if suggested.is_empty() && !ask.terminal {
                bail!("no checks found in Cargo.toml or package.json; pass --check <cmd>");
            }
            checks(given, &suggested, ask)?
        }
        given => given.to_vec(),
    };
    let max = ask_max(ask, req.max.or(recorded.as_ref().and_then(|r| r.max)), "3")?;
    let lead_file = paths.herds.join(format!("{name}-lead.md"));
    let lead = loop {
        let lead = ask.yes(
            "Give the herd a lead, a member that feeds, watches and verifies its workers?",
            explain::LEAD,
            req.lead,
            recorded.as_ref().is_none_or(|r| r.lead),
        )?;
        if !lead || mode == Mode::Diff || fs::symlink_metadata(&lead_file).is_err() {
            break lead;
        }
        let refusal = format!(
            "{} already exists; init never replaces a lead prompt",
            lead_file.display()
        );
        match ask.terminal {
            true => eprintln!("{refusal}; answer n or remove it"),
            false => bail!("{refusal}; pass --no-lead to write the herd without a lead"),
        }
    };
    let merge_guard = match lead && !defaults.merge {
        true => Some(merge_guard(
            ask,
            req.merge_guard,
            recorded.as_ref().and_then(|r| r.merge_guard),
        )?),
        false => None,
    };
    if merge_guard == Some(MergeGuard::Credentials) && github.is_none() {
        bail!(
            "merge_guard = \"credentials\" needs the herd's repo on GitHub; pass --repo <owner>/<name>"
        );
    }
    let (context_limit, config_limit) = ask_context_limit(
        paths,
        ask,
        req.context_limit.clone().or(recorded
            .as_ref()
            .and_then(|r| r.context_limit)
            .map(|l| l.to_string())),
        "",
    )?;
    let github = match github {
        Some((owner, name)) => {
            let account = account(&owner, req.gh_account, ask)?;
            let token = gh(None, &["auth", "token", "--user", &account])?
                .trim()
                .to_string();
            Some(GitHub {
                slug: format!("{owner}/{name}"),
                account,
                token,
            })
        }
        None => None,
    };
    let branch = match &github {
        Some(github) => gh(
            Some(&github.token),
            &[
                "repo",
                "view",
                &github.slug,
                "--json",
                "defaultBranchRef",
                "--jq",
                ".defaultBranchRef.name // \"\"",
            ],
        )?,
        None if tracker.kind == tracker::Kind::Local => {
            let branch = crate::worktree::default_branch(&repo)?;
            branch
                .strip_prefix("origin/")
                .unwrap_or(&branch)
                .to_string()
        }
        None => match origin_head(&repo)? {
            Some(branch) => branch,
            None => {
                let recorded = recorded.as_ref().and_then(|r| r.branch.clone());
                match recorded.or(symbolic_ref(&repo, "HEAD")?) {
                    Some(branch) => {
                        if mode == Mode::Write {
                            eprintln!(
                                "origin/HEAD isn't set, so the default branch is the checked-out \
                                 {branch}; `git remote set-head origin <branch>` sets it."
                            );
                        }
                        branch
                    }
                    None => String::new(),
                }
            }
        },
    };
    let unknown_branch = format!(
        "can't tell {}'s default branch: origin/HEAD isn't set and HEAD is detached; \
         check out the default branch or run `git remote set-head origin <branch>`",
        repo.display()
    );
    let default_branch = match branch.trim() {
        "" => match &github {
            Some(github) => bail!(
                "{} has no default branch yet; push a first commit",
                github.slug
            ),
            None if tracker.kind == tracker::Kind::Local => {
                bail!("can't tell {}'s default branch from origin", repo.display())
            }
            None => None,
        },
        branch => Some(branch),
    };
    let branch = ask.value(
        "Default branch",
        explain::DEFAULT_BRANCH,
        None,
        default_branch,
        match default_branch {
            Some(_) => "",
            None => &unknown_branch,
        },
        parse_branch,
    )?;
    let groom = ask.yes(
        "Add a groom template, to triage unready tasks when the herd runs low?",
        explain::GROOM,
        req.with.contains(&Template::Groom).then_some(true),
        ask.terminal || recorded.as_ref().is_some_and(|r| r.groom),
    )?;
    // A local source cuts each id to fit; elsewhere the name must hold a 6-digit number.
    let research_name_fits =
        tracker.kind == tracker::Kind::Local || name.len() + "-research-".len() + 6 <= 32;
    let no_room = format!(
        "the herd name {name} leaves no room for wayfinding members' names, \
         {name}-research-<number>, in 32 characters; a herd with wayfinding takes a name \
         of at most 16"
    );
    let with_wayfinding = req.with.contains(&Template::Wayfinding);
    if with_wayfinding && !research_name_fits {
        bail!("{no_room}");
    }
    if ask.terminal && !research_name_fits {
        eprintln!("No wayfinding template: {no_room}.");
    }
    let wayfinding = research_name_fits
        && ask.yes(
            "Add a wayfinding template, for research, domain and prototype tasks?",
            explain::WAYFINDING,
            with_wayfinding.then_some(true),
            ask.terminal || recorded.as_ref().is_some_and(|r| r.wayfinding),
        )?;
    let batch_name_fits =
        tracker.kind == tracker::Kind::Local || name.len() + "-grill-batch-".len() + 6 <= 32;
    let no_batch_room = format!(
        "the herd name {name} leaves no room for grill-batch members' names, \
         {name}-grill-batch-<number>, in 32 characters; a herd with grill-batch takes a name \
         of at most 13"
    );
    let with_grill_batch = req.with.contains(&Template::GrillBatch);
    if with_grill_batch && !batch_name_fits {
        bail!("{no_batch_room}");
    }
    if ask.terminal && !batch_name_fits {
        eprintln!("No grill-batch template: {no_batch_room}.");
    }
    let grill_batch = batch_name_fits
        && ask.yes(
            "Add a grill-batch template, so grilling tasks marked for batch mode run unattended?",
            explain::GRILL_BATCH,
            with_grill_batch.then_some(true),
            ask.terminal || recorded.as_ref().is_some_and(|r| r.grill_batch),
        )?;
    // `--with` refused it before any question.
    if grill_batch && mode == Mode::Write && !with_grill_batch {
        refuse_grill_batch_prompt(&batch_file)?;
    }
    let sections = sections(
        tracker.kind,
        wayfinding,
        grill_batch,
        &tracker.labels.needs_info,
    );
    let tracker_docs = tracker
        .writes
        .drain(..)
        .map(|(path, mut text)| {
            if path == tracker::TRACKER_DOC {
                text.push_str(&sections);
            }
            agents::Doc::new(path, text)
        })
        .collect();
    let agents_herd = agents::Herd {
        account: github.as_ref().map(|g| g.account.as_str()),
        branch: &branch,
        checks: &checks,
        needs_info: &tracker.labels.needs_info,
        tracker: tracker.kind,
        sections: &sections,
    };
    let plan = agents::missing(&repo, &agents_herd, tracker_docs)?;
    let mut docs = plan.docs;
    match mode {
        Mode::Write => {
            for doc in docs.iter().filter(|doc| doc.append) {
                doc.refuse_unappendable(&repo)?;
            }
            let new_docs = docs.iter().filter(|doc| !doc.append);
            refuse_unwritable(
                [herd_file]
                    .into_iter()
                    .chain(lead.then(|| lead_file.clone()))
                    .chain(grill_batch.then(|| batch_file.clone()))
                    .chain(new_docs.map(|doc| repo.join(doc.path))),
            )?;
        }
        Mode::Diff => {
            docs.extend(tracker::built_in(&repo, &sections)?);
            docs.extend(agents::present(&repo, &agents_herd)?);
        }
    }
    Ok(Herd {
        repo,
        name,
        github,
        tracker,
        branch,
        checks,
        max,
        lead,
        switches: switch_lines(&defaults),
        merge_guard,
        context_limit,
        config_limit,
        docs,
        agents_steps: plan.steps,
        groom,
        grill: true,
        wayfinding,
        grill_batch,
    })
}

fn refuse_grill_batch_prompt(file: &Path) -> Result<()> {
    if fs::symlink_metadata(file).is_ok() {
        bail!(
            "{} already exists; init never replaces a grill-batch prompt",
            file.display()
        );
    }
    Ok(())
}

/// The workers' `context_limit`, Enter taking `default` and an empty answer
/// leaving it unset, with the text naming what applies when it is unset.
fn ask_context_limit(
    paths: &Paths,
    ask: &ask::Asker,
    given: Option<String>,
    default: &str,
) -> Result<(Option<Limit>, String)> {
    let config = paths.herds.with_file_name("config.toml");
    let config_file = crate::config::read(&config)?;
    let cap = crate::context::limit_cap(&config)?;
    let unset = config_file
        .default_context_limit
        .map_or(crate::context::DEFAULT_LIMIT, |limit| limit.of(cap.window));
    let window = match config_file.context_window {
        Some(_) => "config.toml's",
        None => "the largest known",
    };
    let config_limit = match config_file.default_context_limit {
        None if config_file.context_window.is_none() => {
            format!("the default {}", cap.limit.min(unset))
        }
        _ if unset > cap.limit => format!(
            "the cap of {} (80% of {window} {}-token context window)",
            cap.limit, cap.window
        ),
        Some(limit) => format!("config.toml's {limit}"),
        None => format!("the default {unset}"),
    };
    let context_limit = ask.value(
        &format!(
            "Workers' handoff limit in tokens or a percentage of the window (context_limit); \
             Enter leaves it unset, \
             so {config_limit} applies"
        ),
        explain::CONTEXT_LIMIT,
        given.as_deref(),
        Some(default),
        "",
        |limit| {
            if limit.is_empty() {
                return Ok(None);
            }
            let unchanged = limit == default;
            let limit = crate::context::limit("context_limit", limit)?;
            // A re-run keeps a herd's value that a later config.toml put over the cap.
            if unchanged {
                return Ok(Some(limit));
            }
            if limit.of(cap.window) > cap.limit {
                bail!(
                    "context_limit {limit} is over {}, 80% of the {}-token context window",
                    cap.limit,
                    cap.window
                );
            }
            Ok(Some(limit))
        },
    )?;
    Ok((context_limit, config_limit))
}

fn ask_max(ask: &ask::Asker, given: Option<u64>, default: &str) -> Result<u64> {
    ask.value(
        "max, how many workers run at once",
        explain::MAX,
        given.map(|m| m.to_string()).as_deref(),
        Some(default),
        "",
        |max| match max.parse::<i64>() {
            Ok(max) if max > 0 => Ok(max as u64),
            Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => {
                bail!("max {max} is too big; at most {}", i64::MAX)
            }
            _ => bail!("max must be a positive integer, got {max:?}"),
        },
    )
}

/// `[lead_defaults]` from config.toml, over loopherd's own defaults.
fn lead_defaults(paths: &Paths) -> Result<herd::Switches> {
    let config = paths.herds.with_file_name("config.toml");
    Ok(crate::config::lead_defaults(&config)?
        .unwrap_or_default()
        .over(&Default::default()))
}

/// One `<switch> = <on>` line per switch, as the lead table holds them.
fn switch_lines(defaults: &herd::Switches) -> String {
    defaults
        .named()
        .map(|(name, on)| format!("{name} = {on}"))
        .join("\n")
}

/// Asks, on a terminal, how a lead with merge off is kept from merging.
fn merge_guard(
    ask: &ask::Asker,
    given: Option<MergeGuard>,
    recorded: Option<MergeGuard>,
) -> Result<MergeGuard> {
    if ask.terminal && given.is_none() {
        eprintln!("{MERGE_GUARD_HELP}");
    }
    let given = given.map(MergeGuard::name);
    let default = recorded.unwrap_or_default().name();
    ask.value(
        "merge_guard",
        "",
        given,
        Some(default),
        "",
        |answer| match answer.trim() {
            "hook" => Ok(MergeGuard::Hook),
            "credentials" => Ok(MergeGuard::Credentials),
            other => bail!("answer hook or credentials, not {other:?}"),
        },
    )
}

const MERGE_GUARD_HELP: &str = "The lead has merge off. How is it kept from merging?
  hook         Its tool hook refuses merges and force-pushes, but it holds your
               GitHub credentials, so a hijacked lead could get past it. You're
               told of every merge while it runs. Nothing to set up.
  credentials  It pushes with a deploy key and runs gh with a token that can't
               merge, rulesets stop merges and force-pushes on GitHub, and it
               can't read your credentials. Needs loopherd merge-guard <herd> and
               a token you make in GitHub's web UI, once per repo.";

fn parse_branch(branch: &str) -> Result<String> {
    if branch.contains(char::is_whitespace) {
        bail!("{branch:?} can't name a branch");
    }
    prompt_text(branch)?;
    // The herd's shell commands hold it unquoted.
    if branch.starts_with('-')
        || !branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        bail!(
            "init takes a branch of letters, digits and - _ . / only, \
             not starting with -; got {branch:?}"
        );
    }
    Ok(branch.to_string())
}

/// The lead and members are lead-<herd> and <herd>-<n>, both [a-z][a-z0-9_-]{0,31}.
fn herd_name(name: &str) -> Result<String> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 27
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if !valid {
        bail!(
            "{name:?} can't name a herd: it needs a lowercase letter then at most 26 of \
             a-z, 0-9, _ and -, to name its lead and members; pass --herd <name>"
        );
    }
    Ok(name.to_string())
}

/// `given`, else `offered`: on a terminal kept, or each kept, dropped or
/// replaced, then others added.
fn checks(given: &[String], offered: &[String], ask: &ask::Asker) -> Result<Vec<String>> {
    if !given.is_empty() {
        return Ok(given.to_vec());
    }
    let suggested = offered.to_vec();
    if !ask.terminal {
        return Ok(suggested);
    }
    ask::explain(explain::CHECKS);
    if !suggested.is_empty() {
        eprintln!("Checks workers run before calling work done:");
        for check in &suggested {
            eprintln!("  {check}");
        }
        if ask.yes("Use these checks?", "", None, true)? {
            return Ok(suggested);
        }
        eprintln!("Enter keeps a check, - drops it, anything else replaces it.");
    }
    let mut checks = vec![];
    for check in &suggested {
        let kept = ask.value("Check", "", None, Some(check), "", |answer| match answer {
            "-" => Ok(None),
            answer => prompt_text(answer).map(|_| Some(answer.to_string())),
        })?;
        checks.extend(kept);
    }
    loop {
        let check = ask::line(match checks.is_empty() {
            true => "A check workers run before calling work done: ",
            false => "Another check (Enter when done): ",
        })?;
        match check.trim() {
            "" if checks.is_empty() => continue,
            "" => return Ok(checks),
            check => match prompt_text(check) {
                Ok(_) => checks.push(check.to_string()),
                Err(e) => eprintln!("{e:#}"),
            },
        }
    }
}

/// Cargo's checks for a Cargo.toml, and package.json's `lint` and `test` scripts.
fn suggested_checks(repo: &Path) -> Vec<String> {
    let mut checks = vec![];
    if repo.join("Cargo.toml").is_file() {
        checks.extend(
            [
                "cargo fmt --check",
                "cargo clippy --all-targets -- -D warnings",
                "cargo test",
            ]
            .map(String::from),
        );
    }
    let package = repo.join("package.json");
    let json = match fs::read_to_string(&package) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}'))
            .map_err(anyhow::Error::from),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return checks,
        Err(e) => Err(e.into()),
    };
    let json = match json {
        Ok(json) => json,
        Err(e) => {
            eprintln!(
                "warning: can't read {}, so it suggests no checks: {e}",
                package.display()
            );
            return checks;
        }
    };
    if let Some(scripts) = json.get("scripts") {
        let script = |name| scripts.get(name).and_then(serde_json::Value::as_str);
        if script("lint").is_some() {
            checks.push("npm run lint".into());
        }
        // npm init's placeholder test script always fails.
        if script("test").is_some_and(|test| !test.contains("no test specified")) {
            checks.push("npm test".into());
        }
    }
    checks
}

fn summary(herd: &Herd) -> String {
    let mut also = vec![];
    if let Some(github) = &herd.github
        && herd.tracker.kind == tracker::Kind::GitHub
    {
        let names: Vec<&str> =
            wanted_labels(&herd.tracker.labels, herd.wayfinding, herd.grill_batch)
                .into_iter()
                .map(|(name, ..)| name)
                .collect();
        let (last, rest) = names.split_last().expect("at least four labels");
        also.push(format!(
            "creates the labels {} and {last} in {} where missing",
            rest.join(", "),
            github.slug
        ));
    }
    if !herd.docs.is_empty() {
        let docs: Vec<String> = herd.docs.iter().map(agents::Doc::planned).collect();
        also.push(format!(
            "writes {} in the repo, uncommitted",
            and_list(&docs)
        ));
    }
    let also = match also.is_empty() {
        true => String::new(),
        false => format!("Writing it also {}.\n", also.join(", and ")),
    };
    let limit = match herd.context_limit {
        Some(limit) => limit.to_string(),
        None => format!("unset, {} applies", herd.config_limit),
    };
    let branch = match &herd.github {
        Some(github) => format!(
            "GitHub  {} as {}, branch {}",
            github.slug, github.account, herd.branch
        ),
        None => format!("branch  {}", herd.branch),
    };
    let no_merged = match herd.github {
        Some(_) => "",
        None => NO_MERGED_NOTE,
    };
    format!(
        "\nHerd {name}:\n  repo    {repo}\n  {branch}\n  \
         max     {max} workers at once\n  lead    {lead}\n  limit   {limit}\n  checks  {checks}\n  \
         groom   {groom}\n  wayfinding {wayfinding}\n  grill-batch {grill_batch}\n  \
         also    grill (attended)\n{also}{no_merged}",
        name = herd.name,
        repo = herd.repo.display(),
        max = herd.max,
        lead = match (herd.lead, herd.merge_guard) {
            (false, _) => "no".to_string(),
            (true, None) => "yes".to_string(),
            (true, Some(guard)) => format!("yes, merge_guard {}", guard.name()),
        },
        checks = herd.checks.join("; "),
        groom = if herd.groom { "yes" } else { "no" },
        wayfinding = if herd.wayfinding { "yes" } else { "no" },
        grill_batch = if herd.grill_batch { "yes" } else { "no" },
    )
}

/// The lead prompt, when the herd has a lead, the grill prompt, then the herd file, each with its text.
fn rendered(paths: &Paths, herd: &Herd) -> Result<Rendered> {
    let Herd {
        repo,
        name,
        github,
        tracker,
        branch,
        ..
    } = herd;
    let herd_file = paths.herds.join(format!("{name}.toml"));
    let lead_file = paths.herds.join(format!("{name}-lead.md"));
    let checks = herd
        .checks
        .iter()
        .map(|check| Ok(format!("   - `{}`", prompt_text(check)?)))
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let branch = prompt_text(branch)?;
    let repo_path = toml_string(&repo.to_string_lossy());
    let lead_prompt = toml_string(&format!("{name}-lead.md"));
    let max = herd.max.to_string();
    let context_limit = match herd.context_limit {
        Some(Limit::Tokens(tokens)) => format!("context_limit = {tokens}\n"),
        Some(percent) => format!("context_limit = \"{percent}\"\n"),
        None => String::new(),
    };
    let labels = &tracker.labels;
    let needs_info = prompt_text(&labels.needs_info)?;
    let (ready_for_agent_lc, needs_info_lc, ready_for_human_lc) = (
        labels.ready_for_agent.to_ascii_lowercase(),
        labels.needs_info.to_ascii_lowercase(),
        labels.ready_for_human.to_ascii_lowercase(),
    );
    // A fetch that loses a race to update origin/<branch> fails; its retry finds it current.
    let fetch = format!(
        "git fetch -q origin {branch} 2>/dev/null || git fetch -q origin {branch} || exit\n"
    );
    let git_ref = format!("origin/{branch}");
    // Leaves room for "<herd>-" in a member name of at most 32 characters.
    let id_room = (32 - name.len() - 1).to_string();
    let (slug, account) = match github {
        Some(github) => (github.slug.as_str(), github.account.as_str()),
        None => ("", ""),
    };
    let (task, lead_tasks) = match tracker.kind {
        tracker::Kind::Local => (
            "ready issue under .scratch/ in this repo".to_string(),
            format!(
                "Tasks are issue files under `.scratch/` on origin's {branch}. A \
                 worker's branch sets its issue's `Status:` line to `resolved`, so \
                 the issue closes when the branch merges."
            ),
        ),
        tracker::Kind::GitHub => (
            format!("ready GitHub issue in {slug}"),
            "Tasks are GitHub issues in @OWNER_REPO@. A worker opens a PR whose body \
             carries `Closes #<n>`, so its issue closes when the PR merges."
                .to_string(),
        ),
        tracker::Kind::Other => (
            format!("ready task in the tracker {}", tracker::TRACKER_DOC),
            format!(
                "Tasks come from the tracker {doc} describes. {FILL_IN}: how a \
                 worker's task closes when its @WORK@ merges. (fill in)",
                doc = tracker::TRACKER_DOC
            ),
        ),
    };
    let dir_name = repo.file_name().unwrap_or_default().to_string_lossy();
    let checkout_of = match github {
        Some(github) if tracker.kind != tracker::Kind::Other => github.slug.as_str(),
        _ => &dir_name,
    };
    let github_text = |github_text: &'static str, otherwise: &'static str| match github {
        Some(_) => github_text,
        None => otherwise,
    };
    // A credentials lead can't read the human's gh login, so its own GH_TOKEN wins; a
    // human shell without one still runs as the account.
    let credentials = herd.merge_guard == Some(MergeGuard::Credentials);
    let token = match credentials {
        true => format!("GH_TOKEN=${{GH_TOKEN:-$(gh auth token --user {account})}}"),
        false => format!("GH_TOKEN=$(gh auth token --user {account})"),
    };
    let gh = format!("{token} gh");
    // Checks and worker prompts are templates, which take {{ and }} for braces.
    let member_token = token.replace('{', "{{").replace('}', "}}");
    let switches = match herd.merge_guard {
        Some(MergeGuard::Credentials) => format!(
            "{}\nmerge_guard = \"credentials\"\n\n[worker]\n\
             # A credentials lead starts only while its workers are sandboxed.\n\
             sandbox = \"require\"",
            herd.switches
        ),
        Some(guard) => format!("{}\nmerge_guard = \"{}\"", herd.switches, guard.name()),
        None => herd.switches.clone(),
    };
    let mut values = vec![
        ("OWNER_REPO", slug),
        ("ACCOUNT", account),
        ("GH", &gh),
        ("GH_TOKEN", &member_token),
        ("CHECKOUT_OF", checkout_of),
        ("HERD", name),
        ("REPO", &repo_path),
        ("BRANCH", &branch),
        ("CHECKS", &checks),
        ("MAX", &max),
        ("CONTEXT_LIMIT", &context_limit),
        ("LEAD_PROMPT", &lead_prompt),
        ("SWITCHES", &switches),
        ("READY_FOR_AGENT", &labels.ready_for_agent),
        ("NEEDS_INFO", &needs_info),
        ("READY_FOR_HUMAN", &labels.ready_for_human),
        ("NEEDS_INFO_LC", &needs_info_lc),
        ("READY_FOR_HUMAN_LC", &ready_for_human_lc),
        ("READY_FOR_AGENT_LC", &ready_for_agent_lc),
        ("FETCH", &fetch),
        ("REF", &git_ref),
        ("ID_ROOM", &id_room),
        ("TASK", &task),
        ("FILL_IN", FILL_IN),
        ("HANDED_IN", github_text("PR link", "branch")),
        ("WORK", github_text("PR", "branch")),
    ];
    let snippets = [
        ("GH_NOTE", github_text(LOCAL_GH_NOTE, "")),
        ("HAND_IN", github_text(LOCAL_PR_HAND_IN, LOCAL_PUSH_HAND_IN)),
        (
            "OTHER_HAND_IN",
            github_text(OTHER_PR_HAND_IN, OTHER_PUSH_HAND_IN),
        ),
        (
            "LEAD_GH",
            github_text(
                if credentials {
                    LEAD_GH_CREDENTIALS
                } else {
                    LEAD_GH
                },
                "",
            ),
        ),
        (
            "ON_MERGED",
            github_text(LEAD_ON_MERGED, LEAD_ON_BRANCH_MERGED),
        ),
        (
            "MERGE_ASK",
            match credentials {
                true => LEAD_PR_MERGE_ASK_CREDENTIALS,
                false => github_text(LEAD_PR_MERGE_ASK, LEAD_BRANCH_MERGE_ASK),
            },
        ),
    ]
    .map(|(key, text)| (key, fill(text, &values)));
    values.extend(snippets.iter().map(|(key, text)| (*key, text.as_str())));
    let task_fields = match tracker.kind {
        tracker::Kind::Local => "<id> <type> <skill> <slug> <path>",
        tracker::Kind::GitHub | tracker::Kind::Other => "<number> <type> <skill> <slug>",
    };
    let routed = herd.wayfinding || herd.grill_batch;
    let source_prints = match routed {
        true => format!("<template> {task_fields}"),
        false => task_fields.to_string(),
    };
    let (batch_note, wayfinding_note) = match tracker.kind {
        tracker::Kind::GitHub => (GITHUB_GRILL_BATCH_NOTE, GITHUB_ROUTING_NOTE),
        tracker::Kind::Local => (LOCAL_GRILL_BATCH_NOTE, LOCAL_ROUTING_NOTE),
        tracker::Kind::Other => (OTHER_GRILL_BATCH_NOTE, OTHER_WAYFINDING_NOTE),
    };
    let routing_note = match (routed, tracker.kind) {
        (false, _) => String::new(),
        (true, tracker::Kind::Other) => [
            OTHER_ROUTING_HEAD,
            if herd.grill_batch { batch_note } else { "" },
            if herd.wayfinding { wayfinding_note } else { "" },
            OTHER_ROUTING_TAIL,
        ]
        .concat(),
        (true, _) => [
            if herd.grill_batch { batch_note } else { "" },
            match herd.wayfinding {
                true => wayfinding_note,
                false => ANY_OTHER_ISSUE,
            },
        ]
        .concat(),
    };
    let mut templates = vec!["\"issue\""];
    templates.extend(herd.wayfinding.then_some("\"wayfinding\""));
    templates.extend(herd.grill_batch.then_some("\"grill-batch\""));
    let queue_template = match routed {
        true => format!("templates = [{}]", templates.join(", ")),
        false => "template = \"issue\"".to_string(),
    };
    // Searching keeps gh's 1000-issue window to claimable ones, as --label does.
    let ready_label = match herd.grill_batch {
        true => format!(
            "--search 'label:\"{}\",\"grill:batch\"' ",
            labels.ready_for_agent
        ),
        false => format!("--label {} ", labels.ready_for_agent),
    };
    let (github_claimable, local_claimable) = match herd.grill_batch {
        true => (GITHUB_GRILL_BATCH_CLAIMABLE, LOCAL_GRILL_BATCH_CLAIMABLE),
        false => (GITHUB_READY, LOCAL_READY),
    };
    let (github_claimable, local_claimable) = (
        fill(github_claimable, &values),
        fill(local_claimable, &values),
    );
    let github_grilling = match herd.grill_batch {
        true => fill(GITHUB_GRILLING, &[("GITHUB_CLAIMABLE", &github_claimable)]),
        false => String::new(),
    };
    let task_head = match routed {
        true => {
            let mut branches = vec![];
            if herd.grill_batch {
                branches.push(format!("$grilling then {GITHUB_GRILL_BATCH_TASK}"));
            }
            if herd.wayfinding {
                branches.push(format!("$wayfinding then {GITHUB_WAYFINDING_TASK}"));
            }
            format!(
                r"\(if {} else {GITHUB_ROUTED_TASK} end)",
                branches.join(" elif ")
            )
        }
        false => GITHUB_TASK_HEAD.to_string(),
    };
    let local_print = match (herd.wayfinding, herd.grill_batch) {
        (false, false) => LOCAL_PRINT.to_string(),
        (true, false) => LOCAL_WAYFINDING_PRINT.to_string(),
        _ => [
            LOCAL_ROUTE_START,
            if herd.grill_batch {
                LOCAL_GRILL_BATCH_ROUTE
            } else {
                ""
            },
            if herd.wayfinding {
                LOCAL_WAYFINDING_ROUTE
            } else {
                ""
            },
            LOCAL_ROUTE_END,
        ]
        .concat(),
    };
    let lead_grilling = match herd.grill_batch {
        true => fill(LEAD_GRILL_BATCH, &values),
        false => LEAD_GRILLING.to_string(),
    };
    values.extend([
        ("SOURCE_PRINTS", source_prints.as_str()),
        ("ROUTING_NOTE", &routing_note),
        ("ROUTE", if herd.wayfinding { GITHUB_ROUTE } else { "" }),
        ("TASK_HEAD", &task_head),
        ("QUEUE_TEMPLATE", &queue_template),
        ("READY_LABEL", &ready_label),
        ("GITHUB_CLAIMABLE", &github_claimable),
        ("GITHUB_GRILLING", &github_grilling),
        ("LOCAL_CLAIMABLE", &local_claimable),
        (
            "BACKLOG_NOTE",
            if herd.grill_batch {
                BACKLOG_GRILLING_NOTE
            } else {
                ""
            },
        ),
        ("GRILLING", &lead_grilling),
    ]);
    let local = |list, print| {
        fill(
            LOCAL_ISSUES,
            &[values.as_slice(), &[("LIST", list), ("LOCAL_PRINT", print)]].concat(),
        )
    };
    let (local_ready, local_backlog) =
        (local("ready", &local_print), local("backlog", LOCAL_PRINT));
    values.push(("LOCAL_READY", &local_ready));
    values.push(("LOCAL_BACKLOG", &local_backlog));
    // Filled before they join the herd file, whose fill leaves inserted text literal.
    let (source, template) = match tracker.kind {
        tracker::Kind::GitHub => (GITHUB_SOURCE, GITHUB_ISSUE),
        tracker::Kind::Local => (LOCAL_SOURCE, LOCAL_ISSUE),
        tracker::Kind::Other => (PLACEHOLDER_SOURCE, OTHER_ISSUE),
    };
    let source = fill(source, &values);
    let source = source.trim_end();
    let merged = match github {
        Some(_) => fill(MERGED_GITHUB, &values),
        None => String::new(),
    };
    let lead_tasks = fill(&lead_tasks, &values);
    values.push(("MERGED", merged.trim_end()));
    let done = match tracker.kind {
        tracker::Kind::GitHub => GITHUB_DONE,
        tracker::Kind::Local => LOCAL_DONE,
        tracker::Kind::Other => OTHER_DONE,
    };
    let issue_done = fill(done, &values);
    values.push(("DONE", issue_done.trim_end()));
    let template = fill(template, &values);
    let wayfinding = match herd.wayfinding {
        true => format!("\n\n{}", wayfinding(&template, tracker.kind, &values)),
        false => String::new(),
    };
    let (grill, grill_prompt) = match herd.grill {
        true => grill(herd, &values, done),
        false => (String::new(), String::new()),
    };
    let batch = herd
        .grill_batch
        .then(|| grill_batch(herd, &values, &template));
    let grill_batch_table = match &batch {
        Some((table, _)) => format!("\n\n{table}"),
        None => String::new(),
    };
    let lead_table = match herd.lead {
        true => format!("\n{}", fill(LEAD_TABLE, &values)),
        false => String::new(),
    };
    values.push(("SOURCE", source));
    values.push(("TEMPLATE", template.trim_end()));
    values.push(("WAYFINDING", wayfinding.trim_end()));
    values.push(("GRILL_BATCH", grill_batch_table.trim_end()));
    values.push(("LEAD_TABLE", &lead_table));
    values.push(("LEAD_TASKS", &lead_tasks));
    let groom = match herd.groom {
        true => groom(herd, &values)?,
        false => String::new(),
    };
    values.push(("GROOM", &groom));
    values.push(("GRILL", &grill));
    let lead = herd.lead.then(|| (lead_file, fill(LEAD, &values)));
    let grill_prompt = herd.grill.then(|| (grill_file(paths, name), grill_prompt));
    let batch_prompt = batch.map(|(_, prompt)| (grill_batch_file(paths, name), prompt));
    let files = lead
        .into_iter()
        .chain(grill_prompt)
        .chain(batch_prompt)
        .chain([(herd_file, fill(HERD, &values))])
        .collect();
    Ok(Rendered {
        files,
        grill: grill.trim_start().to_string(),
        wayfinding: wayfinding.trim_start().to_string(),
        grill_batch: grill_batch_table.trim_start().to_string(),
    })
}

/// The gh account `[template.issue]`'s checks run as, from their `--user <login>`.
fn recorded_account(herd_file: &Path) -> Option<String> {
    let herd: toml::Table = fs::read_to_string(herd_file).ok()?.parse().ok()?;
    let issue = herd.get("template")?.get("issue")?;
    ["done", "merged"]
        .iter()
        .filter_map(|key| issue.get(key)?.as_str())
        .find_map(|check| {
            let (_, after) = check.split_once("gh auth token --user ")?;
            let login = after
                .split(|c: char| c == ')' || c.is_whitespace())
                .next()?;
            is_github_name(login).then(|| login.to_string())
        })
}

struct Rendered {
    files: Vec<(PathBuf, String)>,
    /// The `[template.grill]` within the herd file.
    grill: String,
    /// The `[template.wayfinding]` within the herd file, empty without one.
    wayfinding: String,
    /// The `[template.grill-batch]` within the herd file, empty without one.
    grill_batch: String,
}

/// `template` as init would write it now for the existing `herd`, rendered as
/// `init --diff` renders over the herd's files.
pub fn template(paths: &Paths, herd: &str, template: &str) -> Result<herd::NewTemplate> {
    let with = match template {
        "grill" => vec![],
        "wayfinding" => vec![Template::Wayfinding],
        "grill-batch" => vec![Template::GrillBatch],
        _ => bail!("init writes no template {template}"),
    };
    let repo = herd::repo(paths, herd)?;
    let account = recorded_account(&paths.herds.join(format!("{herd}.toml")));
    let req = Request {
        dir: &repo,
        herd: Some(herd),
        // Neither template's prompt names checks, so a herd without readable ones still renders.
        checks: &["true".to_string()],
        gh_account: account.as_deref(),
        github: None,
        no_github: false,
        tracker: None,
        max: None,
        lead: None,
        merge_guard: None,
        context_limit: None,
        with: &with,
        running_leads: &BTreeSet::new(),
    };
    let ask = ask::Asker { terminal: false };
    let repo = ask_repo(&repo, &ask)?;
    let mut herd = settle(paths, repo, req, &ask, Mode::Diff)?;
    match template {
        "wayfinding" => return routed(paths, &mut herd, Template::Wayfinding),
        "grill-batch" => return routed(paths, &mut herd, Template::GrillBatch),
        _ => {}
    }
    let mut rendered = rendered(paths, &herd)?;
    let prompt_file = grill_file(paths, &herd.name);
    let prompt = rendered
        .files
        .iter_mut()
        .find(|(path, _)| *path == prompt_file)
        .map(|(_, text)| std::mem::take(text))
        .context("init renders no grill prompt")?;
    Ok(herd::NewTemplate {
        table: rendered.grill,
        prompt: Some((format!("{}-grill.md", herd.name), prompt)),
        queue: None,
    })
}

/// A template the queue routes to, with the `[queue]` init writes without and
/// with it, and its prompt file when it has one.
fn routed(paths: &Paths, herd: &mut Herd, template: Template) -> Result<herd::NewTemplate> {
    let herd_file = paths.herds.join(format!("{}.toml", herd.name));
    let batch_file = grill_batch_file(paths, &herd.name);
    let mut render = |on| -> Result<(toml_edit::Table, Rendered)> {
        match template {
            Template::Wayfinding => herd.wayfinding = on,
            Template::GrillBatch => herd.grill_batch = on,
            Template::Groom => unreachable!("the queue routes nothing to groom"),
        }
        let rendered = rendered(paths, herd)?;
        let (_, text) = rendered
            .files
            .iter()
            .find(|(path, _)| *path == herd_file)
            .context("init renders no herd file")?;
        let doc: toml_edit::DocumentMut = text.parse().context("rendered herd file")?;
        let queue = doc
            .get("queue")
            .and_then(toml_edit::Item::as_table)
            .context("rendered herd file has no [queue]")?;
        Ok((queue.clone(), rendered))
    };
    let (before, _) = render(false)?;
    let (after, rendered) = render(true)?;
    let (table, prompt) = match template {
        Template::GrillBatch => {
            let prompt = rendered
                .files
                .into_iter()
                .find(|(path, _)| *path == batch_file)
                .map(|(_, text)| (format!("{}-grill-batch.md", herd.name), text));
            (rendered.grill_batch, prompt)
        }
        _ => (rendered.wayfinding, None),
    };
    Ok(herd::NewTemplate {
        table,
        prompt,
        queue: Some(herd::RoutedQueue { before, after }),
    })
}

fn grill_file(paths: &Paths, herd: &str) -> PathBuf {
    paths.herds.join(format!("{herd}-grill.md"))
}

fn grill_batch_file(paths: &Paths, herd: &str) -> PathBuf {
    paths.herds.join(format!("{herd}-grill-batch.md"))
}

/// A grill's words for the herd's tracker: the issue it names, how to read and
/// claim it, and how to hand in the outcome, each filled from `values`.
struct GrillSteps {
    issue: String,
    read: String,
    claim: String,
    hand_in: String,
}

fn grill_steps(herd: &Herd, values: &[(&str, &str)]) -> GrillSteps {
    let github = herd.github.is_some();
    let pick = |with: &'static str, without: &'static str| if github { with } else { without };
    let (issue, read, claim, hand_in) = match herd.tracker.kind {
        tracker::Kind::GitHub => (
            "GitHub issue #{number} of @OWNER_REPO@",
            GITHUB_GRILL_READ,
            GITHUB_GRILL_CLAIM,
            GITHUB_GRILL_HAND_IN.to_string(),
        ),
        tracker::Kind::Local => (
            "the local issue {path}",
            LOCAL_GRILL_READ,
            LOCAL_GRILL_CLAIM,
            format!(
                "{LOCAL_GRILL_RESOLVE} {}",
                pick(LOCAL_GRILL_PR, LOCAL_GRILL_PUSH)
            ),
        ),
        tracker::Kind::Other => (
            "task {number}",
            OTHER_GRILL_READ,
            OTHER_GRILL_CLAIM,
            pick(OTHER_GRILL_PR, OTHER_GRILL_PUSH).to_string(),
        ),
    };
    let push = fill(GRILL_PUSH, values);
    let values = [values, &[("GRILL_PUSH", push.as_str())]].concat();
    GrillSteps {
        issue: fill(issue, &values),
        read: fill(read, &values),
        claim: fill(claim, &values),
        hand_in: fill(&hand_in, &values),
    }
}

fn value<'a>(values: &[(&str, &'a str)], key: &str) -> &'a str {
    values
        .iter()
        .find(|(k, _)| *k == key)
        .map_or("", |(_, v)| *v)
}

fn check_lines(checks: &[&str]) -> String {
    checks
        .iter()
        .map(|check| check.trim_end())
        .filter(|check| !check.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `[template.grill]`, led by a blank line, and its prompt, for the herd's tracker.
fn grill(herd: &Herd, values: &[(&str, &str)], done: &str) -> (String, String) {
    let (key, params, usage) = match herd.tracker.kind {
        tracker::Kind::GitHub | tracker::Kind::Other => {
            ("number", r#"["number", "slug"]"#, "<number> <slug>")
        }
        tracker::Kind::Local => ("id", r#"["id", "slug", "path"]"#, "<id> <slug> <path>"),
    };
    // A grill takes the worker's context limit from the herd's default, not the issue's.
    let done = fill(done, &[&[("CONTEXT_LIMIT", "")], values].concat());
    let checks = check_lines(&[&done, value(values, "MERGED")]);
    let prompt_file = toml_string(&format!("{}-grill.md", herd.name));
    let steps = grill_steps(herd, values);
    let values = [
        values,
        &[
            ("GRILL_KEY", key),
            ("GRILL_PARAMS", params),
            ("GRILL_USAGE", usage),
            ("GRILL_CHECKS", &checks),
            ("GRILL_PROMPT", &prompt_file),
            ("GRILL_ISSUE", &steps.issue),
            ("GRILL_READ", &steps.read),
            ("GRILL_CLAIM", &steps.claim),
            ("GRILL_HAND_IN", &steps.hand_in),
            ("TRACKER_DOC", tracker::TRACKER_DOC),
        ],
    ]
    .concat();
    (
        format!("\n{}", fill(GRILL, &values)),
        fill(GRILL_PROMPT, &values),
    )
}

/// The `[template.grill-batch]` and its prompt, for the herd's tracker. Its
/// params are the issue template's, as the queue routes tasks to it.
fn grill_batch(herd: &Herd, values: &[(&str, &str)], issue: &str) -> (String, String) {
    let params = issue
        .lines()
        .find_map(|line| line.strip_prefix("params = "))
        .expect("the issue template has params");
    let (key, task_word, batch_key, done, post, wait, post_except) = match herd.tracker.kind {
        tracker::Kind::GitHub => (
            "number",
            "issue",
            "#{number}",
            GITHUB_BATCH_DONE,
            GITHUB_BATCH_POST,
            GITHUB_BATCH_WAIT,
            "",
        ),
        tracker::Kind::Local => (
            "id",
            "issue",
            "{id}",
            LOCAL_BATCH_DONE,
            LOCAL_BATCH_POST,
            LOCAL_BATCH_WAIT,
            LOCAL_BATCH_POST_EXCEPT,
        ),
        tracker::Kind::Other => (
            "number",
            "task",
            "#{number}",
            OTHER_BATCH_DONE,
            OTHER_BATCH_POST,
            OTHER_BATCH_WAIT,
            "",
        ),
    };
    let done = fill(done, values);
    let checks = check_lines(&[&done, value(values, "MERGED")]);
    let prompt_file = toml_string(&format!("{}-grill-batch.md", herd.name));
    let steps = grill_steps(herd, values);
    // Claiming a local issue commits on the branch, which waits for the human's answer.
    let claim = match herd.tracker.kind {
        tracker::Kind::Local => String::new(),
        _ => format!("\nClaim it: {}\n", steps.claim),
    };
    let push = match herd.github {
        Some(_) => fill(GITHUB_GIT_PUSH, values),
        None => "git push".to_string(),
    };
    let post_values = [values, &[("BATCH_PUSH", push.as_str())]].concat();
    let [post, wait, post_except] = [post, wait, post_except].map(|text| fill(text, &post_values));
    // The batch's outcome runs once the human has accepted, so it waits on no one.
    let hand_in = match steps.hand_in.strip_prefix(AGREED) {
        Some(rest) => rest[..1].to_uppercase() + &rest[1..],
        None => steps.hand_in.clone(),
    };
    let values = [
        values,
        &[
            ("GRILL_KEY", key),
            ("BATCH_PARAMS", params),
            ("BATCH_CHECKS", &checks),
            ("BATCH_PROMPT", &prompt_file),
            ("GRILL_ISSUE", &steps.issue),
            ("GRILL_READ", &steps.read),
            ("GRILL_HAND_IN", &hand_in),
            ("BATCH_CLAIM", &claim),
            ("TASK_WORD", task_word),
            ("BATCH_KEY", batch_key),
            ("BATCH_POST", &post),
            ("BATCH_WAIT", &wait),
            ("POST_EXCEPT", &post_except),
            ("TRACKER_DOC", tracker::TRACKER_DOC),
        ],
    ]
    .concat();
    (
        fill(GRILL_BATCH, &values),
        fill(GRILL_BATCH_PROMPT, &values),
    )
}

/// The `[template.wayfinding]`: the rendered `issue` template's settings on a
/// `docs/` branch and a `-research-` name, with the tracker's wayfinding prompt.
fn wayfinding(issue: &str, kind: tracker::Kind, values: &[(&str, &str)]) -> String {
    let prompt = match kind {
        tracker::Kind::GitHub => GITHUB_WAYFINDING,
        tracker::Kind::Local => LOCAL_WAYFINDING,
        tracker::Kind::Other => OTHER_WAYFINDING,
    };
    let (settings, _) = issue
        .split_once("prompt = '''")
        .expect("the issue template has a prompt");
    let settings = settings
        .replacen("[template.issue]", "[template.wayfinding]", 1)
        .replacen("name = \"{herd}-", "name = \"{herd}-research-", 1)
        .replacen("branch = \"{type}/", "branch = \"docs/", 1);
    format!("{settings}{}", fill(prompt, values))
}

/// The tracker doc's and `docs/agents/loopherd.md`'s word on the herd's routed
/// templates: wayfinding tasks and batch grills, each only when the herd has it.
fn sections(kind: tracker::Kind, wayfinding: bool, grill_batch: bool, needs_info: &str) -> String {
    let (wayfinding_section, batch_section) = match kind {
        tracker::Kind::GitHub => (GITHUB_WAYFINDING_SECTION, GITHUB_BATCH_SECTION),
        tracker::Kind::Local => (LOCAL_WAYFINDING_SECTION, LOCAL_BATCH_SECTION),
        tracker::Kind::Other => (OTHER_WAYFINDING_SECTION, OTHER_BATCH_SECTION),
    };
    let mut text = String::new();
    if wayfinding {
        text.push_str(wayfinding_section);
    }
    if grill_batch {
        text.push_str(&fill(batch_section, &[("NEEDS_INFO", needs_info)]));
    }
    text
}

const GITHUB_WAYFINDING_SECTION: &str = "
## Wayfinding tasks

A ready issue labelled `wayfinding:research`, `wayfinding:domain` or
`wayfinding:prototype` is answered rather than built. loopherd's queue routes
it to the herd's wayfinding template, whose worker resolves it with the
`research`, `domain-modeling` or `prototype` skill and opens a PR with the
output (a research note, a decision record or a prototype) on a `docs/` branch.
These are not `/wayfinder`'s `wayfinder:` labels, which loopherd doesn't route.
";
const LOCAL_WAYFINDING_SECTION: &str = "
## Wayfinding tasks

A ready issue whose `Status:` line has `wayfinding:research`,
`wayfinding:domain` or `wayfinding:prototype` is answered rather than built.
loopherd's queue routes it to the herd's wayfinding template, whose worker
resolves it with the `research`, `domain-modeling` or `prototype` skill and
hands in the output (a research note, a decision record or a prototype) on a
`docs/` branch. These are not `/wayfinder`'s `Type:` lines, which loopherd
doesn't route.
";
const OTHER_WAYFINDING_SECTION: &str = "
## Wayfinding tasks

A ready task marked `wayfinding:research`, `wayfinding:domain` or
`wayfinding:prototype` is answered rather than built. loopherd's queue routes
it to the herd's wayfinding template, whose worker resolves it with the
`research`, `domain-modeling` or `prototype` skill and hands in the output (a
research note, a decision record or a prototype) on a `docs/` branch.
";

const GITHUB_BATCH_SECTION: &str = "
## Batch grills

An open issue labelled `grilling` and `grill:batch` is grilled without the
human in the pane, ready or not. loopherd's queue routes it to the
herd's grill-batch template, whose worker answers the grill's questions itself,
posts them as one batch comment on the issue, labels it `@NEEDS_INFO@` and
stops. The human replies there: `accept all`, overrides by number, or
`discuss <n>`. Then the shepherd drops `@NEEDS_INFO@` and runs
`loopherd release <member>`, and the next feed starts a fresh member, which
writes the decision and files its issues. A `grilling` issue without
`grill:batch` waits for a live grill.
";
const LOCAL_BATCH_SECTION: &str = "
## Batch grills

An open issue whose `Status:` line has `grilling` and `grill:batch` is grilled
without the human in the pane, ready or not. loopherd's queue routes it to the
herd's grill-batch template, whose worker answers the grill's questions itself,
commits them as one batch under the issue's `## Comments` on the default
branch, adds `@NEEDS_INFO@` to its `Status:` line and stops. The human replies
under the batch: `accept all`, overrides by number, or `discuss <n>`. Then the
shepherd drops `@NEEDS_INFO@` and runs `loopherd release <member>`, and the
next feed starts a fresh member, which writes the decision and files its
issues. A `grilling` issue without `grill:batch` waits for a live grill.
";
const OTHER_BATCH_SECTION: &str = "
## Batch grills

An open task marked `grilling` and `grill:batch` is grilled without the human
in the pane, ready or not. loopherd's queue routes it to the herd's grill-batch
template, whose worker answers the grill's questions itself, comments them as
one batch on the task, marks it `@NEEDS_INFO@` and stops. The human replies
there: `accept all`, overrides by number, or `discuss <n>`. Then the shepherd
drops `@NEEDS_INFO@` and runs `loopherd release <member>`, and the next feed
starts a fresh member, which writes the decision and files its tasks. A
`grilling` task without `grill:batch` waits for a live grill.
";
/// The `[template.groom]` for the herd's tracker.
fn groom(herd: &Herd, values: &[(&str, &str)]) -> Result<String> {
    let (read, forms, never) = match herd.tracker.kind {
        tracker::Kind::GitHub => (GITHUB_GROOM_READ, GITHUB_GROOM_FORMS, GITHUB_GROOM_NEVER),
        tracker::Kind::Local => (LOCAL_GROOM_READ, LOCAL_GROOM_FORMS, LOCAL_GROOM_NEVER),
        tracker::Kind::Other => (OTHER_GROOM_READ, OTHER_GROOM_FORMS, OTHER_GROOM_NEVER),
    };
    let repo = herd.repo.to_string_lossy();
    let checkout = prompt_text(&format!("'{}'", repo.replace('\'', r"'\''")))?;
    let values = [values, &[("CHECKOUT", checkout.as_str())]].concat();
    let snippets = [read, forms, never].map(|text| fill(text, &values));
    let values = [
        values.as_slice(),
        &[
            ("GROOM_READ", snippets[0].as_str()),
            ("GROOM_FORMS", &snippets[1]),
            ("GROOM_NEVER", &snippets[2]),
        ],
    ]
    .concat();
    Ok(fill(GROOM, &values))
}

fn write(paths: &Paths, herd: Herd) -> Result<Vec<String>> {
    let Herd {
        repo,
        name,
        github,
        tracker,
        ..
    } = &herd;
    let herd_file = paths.herds.join(format!("{name}.toml"));
    let lead_file = paths.herds.join(format!("{name}-lead.md"));
    let herd_files = rendered(paths, &herd)?.files;
    let mut lines = match github {
        Some(github) if tracker.kind == tracker::Kind::GitHub => create_labels(
            &github.token,
            &github.slug,
            &tracker.labels,
            herd.wayfinding,
            herd.grill_batch,
        )?,
        _ => vec![],
    };
    let mut written = Written::default();
    for (path, text) in herd_files {
        if let Err(e) = written.new_file(&path, &text) {
            written.undo();
            return Err(e);
        }
    }
    for doc in &herd.docs {
        let done = match doc.append {
            true => doc.append_to(repo),
            false => written.new_file(&repo.join(doc.path), &doc.text),
        };
        if let Err(e) = done {
            written.undo();
            return Err(e);
        }
    }
    lines.push(format!("wrote {}", herd_file.display()));
    if herd.lead {
        lines.push(format!("wrote {}", lead_file.display()));
    }
    lines.push(format!("wrote {}", grill_file(paths, name).display()));
    if herd.grill_batch {
        lines.push(format!("wrote {}", grill_batch_file(paths, name).display()));
    }
    lines.extend(herd.docs.iter().map(agents::Doc::done));
    let mut steps = vec![format!(
        "Open Claude in {} once and accept its trust dialog; members can't start past \
         it, and init never accepts it for you.",
        repo.display()
    )];
    if tracker.kind == tracker::Kind::Other {
        lines.push(
            "The tracker isn't one init has snippets for, so the herd holds placeholders, \
             and `loopherd herds` reports it invalid until its queue source is filled."
                .into(),
        );
        steps.push(format!(
            "{FILL_IN}, in {}: [queue] source, [queue] backlog, [template.issue] done, {}\
             and each prompt step marked (fill in).",
            herd_file.display(),
            match herd.wayfinding {
                true => "[template.wayfinding] done, ",
                false => "",
            }
        ));
        if herd.lead {
            steps.push(format!(
                "{FILL_IN}, in {}: the line marked (fill in).",
                lead_file.display()
            ));
        }
        steps.push(format!(
            "{FILL_IN}, in {}: [template.grill] done, and in {} each line marked (fill in).",
            herd_file.display(),
            grill_file(paths, name).display()
        ));
        if herd.grill_batch {
            steps.push(format!(
                "{FILL_IN}, in {}: [template.grill-batch] done, and in {} each line marked \
                 (fill in).",
                herd_file.display(),
                grill_batch_file(paths, name).display()
            ));
        }
        steps.push(
            "Fill in each section of docs/agents/loopherd.md marked (fill in), or adapt the \
             doc to the same tracker if it was already there."
                .into(),
        );
    }
    if !has_origin(repo) {
        steps.push(
            "Add an origin remote (`git remote add origin <url>`): members branch from it, \
             and `loopherd feed` fails without one."
                .into(),
        );
    }
    steps.extend(herd.agents_steps.iter().cloned());
    if herd.docs.iter().any(|doc| doc.path == agents::SHEPHERD_DOC) {
        steps.push(format!(
            "Fill in {}, the shepherd's runbook for this repo; `loopherd docs runbook` \
             says what each section holds.",
            agents::SHEPHERD_DOC
        ));
    }
    steps.push(
        "Tailor the herd to the repo per \"Setting up a project\" in `loopherd --skill`.".into(),
    );
    if !herd.docs.is_empty() {
        let paths: Vec<String> = herd.docs.iter().map(|doc| doc.path.to_string()).collect();
        steps.push(format!(
            "Review and commit what init wrote in the repo: {}.",
            and_list(&paths)
        ));
    }
    steps.extend([
        "loopherd herds".into(),
        format!("loopherd feed {name} --dry-run"),
    ]);
    if herd.merge_guard == Some(MergeGuard::Credentials) {
        steps.push(format!(
            "loopherd merge-guard {name}, then make and store the lead's token as it says"
        ));
    }
    if herd.lead {
        steps.push(format!("loopherd lead {name}"));
    }
    lines.push("Next:".into());
    for (n, step) in steps.iter().enumerate() {
        lines.push(format!("  {}. {step}", n + 1));
    }
    Ok(lines)
}

/// Refuses a new file whose nearest existing directory init can't write in.
fn refuse_unwritable(new_files: impl IntoIterator<Item = PathBuf>) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    for path in new_files {
        // A relative path's walk ends at "", the cwd.
        let cwd_for_empty = |dir: &Path| match dir.as_os_str().is_empty() {
            true => Path::new(".").to_path_buf(),
            false => dir.to_path_buf(),
        };
        let Some(dir) = path
            .ancestors()
            .skip(1)
            .map(cwd_for_empty)
            .find(|dir| fs::symlink_metadata(dir).is_ok())
        else {
            continue;
        };
        if !dir.exists() {
            bail!(
                "{} is a dangling symlink; init won't write through it",
                dir.display()
            );
        }
        let c_dir = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
        // SAFETY: c_dir is a valid NUL-terminated path for the call's duration.
        if unsafe { libc::access(c_dir.as_ptr(), libc::W_OK) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(e).with_context(|| format!("write {}", path.display()));
        }
    }
    Ok(())
}

/// The files and directories init created, to undo when a later write fails.
#[derive(Default)]
struct Written {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

impl Written {
    fn new_file(&mut self, path: &Path, text: &str) -> Result<()> {
        if let Some(dir) = path.parent() {
            let missing: Vec<&Path> = dir
                .ancestors()
                .take_while(|dir| fs::symlink_metadata(dir).is_err())
                .collect();
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            self.dirs
                .extend(missing.into_iter().rev().map(Path::to_path_buf));
        }
        write_new(path, text)?;
        self.files.push(path.to_path_buf());
        Ok(())
    }

    fn undo(&self) {
        for file in &self.files {
            let _ = fs::remove_file(file);
        }
        for dir in self.dirs.iter().rev() {
            let _ = fs::remove_dir(dir);
        }
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => items.join(""),
    }
}

fn refuse_taken_name(paths: &Paths, herd: &str, repo: &Path) -> Result<()> {
    let herd_file = &paths.herds.join(format!("{herd}.toml"));
    if fs::symlink_metadata(herd_file).is_ok() {
        refuse_other_repo(paths, herd, repo)?;
        bail!(
            "{} already exists; init never replaces a herd file",
            herd_file.display()
        );
    }
    let grill_file = grill_file(paths, herd);
    if fs::symlink_metadata(&grill_file).is_ok() {
        bail!(
            "{} already exists; init never replaces a grill prompt",
            grill_file.display()
        );
    }
    Ok(())
}

fn refuse_other_repo(paths: &Paths, herd: &str, repo: &Path) -> Result<()> {
    if let Ok(other) = herd::repo(paths, herd)
        && !same_dir(&other, repo)
    {
        bail!(
            "herd {herd} already points at {}; pass --herd <name> to name this repo's herd",
            other.display()
        );
    }
    Ok(())
}

/// The herd that runs `repo`; an error for a herd file init can't read.
fn herd_running(paths: &Paths, repo: &Path) -> Result<Option<String>> {
    for other in herd::names(&paths.herds)? {
        let file = paths.herds.join(format!("{other}.toml"));
        if !fs::metadata(&file).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let other_repo = herd::repo(paths, &other).with_context(|| {
            format!(
                "can't tell which repo {} runs; fix or remove it",
                file.display()
            )
        })?;
        if same_dir(&other_repo, repo) {
            return Ok(Some(other));
        }
    }
    Ok(None)
}

/// The answers an earlier init left in its herd file, for `--diff` to render with.
struct Recorded {
    lead: bool,
    merge_guard: Option<MergeGuard>,
    max: Option<u64>,
    context_limit: Option<Limit>,
    /// The worker prompt's checklist items that are a bare `check`; a reworded one is skipped.
    checks: Vec<String>,
    /// Whether any checklist item was reworded.
    checks_reworded: bool,
    /// The default branch the worker prompt names.
    branch: Option<String>,
    /// The `owner/name` that `merged` checks PRs on.
    github: Option<String>,
    /// The gh account the herd's commands take their token from.
    account: Option<String>,
    has_merged: bool,
    groom: bool,
    grill: bool,
    wayfinding: bool,
    grill_batch: bool,
}

impl Recorded {
    fn read(herd_file: &Path) -> Option<Recorded> {
        let herd: toml::Table = fs::read_to_string(herd_file).ok()?.parse().ok()?;
        let issue = herd
            .get("template")
            .and_then(|t| t.get("issue"))
            .and_then(toml::Value::as_table);
        let number = |value: Option<&toml::Value>| {
            value
                .and_then(toml::Value::as_integer)
                .and_then(|n| u64::try_from(n).ok())
        };
        let prompt = issue
            .and_then(|t| t.get("prompt"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        let items: Vec<&str> = prompt
            .lines()
            .skip_while(|line| !line.starts_with("4. Before calling it done"))
            .skip(1)
            .map_while(|line| line.strip_prefix("   - "))
            .collect();
        let checks: Vec<String> = items
            .iter()
            .filter_map(|item| item.strip_prefix('`')?.strip_suffix('`'))
            .map(|check| check.replace("{{", "{").replace("}}", "}"))
            .collect();
        let branch = prompt.lines().find_map(|line| {
            let branch = line.strip_prefix("If ")?.split_once(" moves under you")?.0;
            Some(branch.to_string())
        });
        let account = fs::read_to_string(herd_file).ok().and_then(|text| {
            let (_, rest) = text.split_once("gh auth token --user ")?;
            let account = rest.split(|c: char| c == ')' || c.is_whitespace()).next()?;
            Some(account.to_string())
        });
        Some(Recorded {
            checks_reworded: checks.len() != items.len(),
            account,
            branch,
            lead: herd.contains_key("lead"),
            merge_guard: match herd.get("lead").and_then(|l| l.get("merge_guard")) {
                Some(value) => value.clone().try_into().ok(),
                None => None,
            },
            max: number(herd.get("queue").and_then(|q| q.get("max"))),
            context_limit: issue
                .and_then(|t| t.get("context_limit"))
                .and_then(|limit| limit.clone().try_into().ok()),
            checks,
            github: issue
                .and_then(|t| t.get("merged"))
                .and_then(toml::Value::as_str)
                .and_then(|merged| {
                    let list = "gh pr list --repo ";
                    let (at, _) = merged
                        .match_indices(list)
                        .find(|(at, _)| *at == 0 || merged[..*at].ends_with(' '))?;
                    merged[at + list.len()..].split_whitespace().next()
                })
                .map(String::from),
            has_merged: issue.is_some_and(|t| t.contains_key("merged")),
            grill: herd
                .get("template")
                .is_some_and(|t| t.get("grill").is_some()),
            groom: herd
                .get("template")
                .is_some_and(|t| t.get("groom").is_some()),
            wayfinding: herd
                .get("template")
                .is_some_and(|t| t.get("wayfinding").is_some()),
            grill_batch: herd
                .get("template")
                .is_some_and(|t| t.get("grill-batch").is_some()),
        })
    }
}

/// The branch the local origin/HEAD names; asks no remote.
fn origin_head(repo: &Path) -> Result<Option<String>> {
    Ok(symbolic_ref(repo, "refs/remotes/origin/HEAD")?
        .map(|head| head.strip_prefix("origin/").unwrap_or(&head).to_string()))
}

fn symbolic_ref(repo: &Path, name: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", name])
        .current_dir(repo)
        .output()
        .context("run git")?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

fn has_origin(repo: &Path) -> bool {
    Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// `origin`'s GitHub owner and repo name.
fn github_repo(repo: &Path) -> Result<(String, String)> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo)
        .output()
        .context("run git")?;
    if !output.status.success() {
        bail!(
            "{} has no origin remote; pass --repo <owner>/<name>",
            repo.display()
        );
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parts = github_path(&url).map(|path| {
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        path.split('/').map(String::from).collect::<Vec<_>>()
    });
    match parts.as_deref() {
        Some([owner, name]) if is_github_name(owner) && is_github_name(name) => {
            Ok((owner.clone(), name.clone()))
        }
        _ => bail!("origin {url} is not a GitHub repo; pass --repo <owner>/<name>"),
    }
}

/// The path of a `github.com` remote URL, whatever the host's case, scp-like
/// (`git@github.com:o/r`) or with a scheme, a user and a port
/// (`ssh://git@ssh.github.com:443/o/r`).
fn github_path(url: &str) -> Option<&str> {
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = authority.rsplit('@').next()?.split(':').next()?;
    crate::github::is_github_com(host).then_some(path)
}

/// Safe to put unquoted in the herd's shell commands and prompts, and not
/// an option, a path step (`.` or `..`) or a `.git` suffix. A leading dot is
/// allowed: GitHub repos like `<org>/.github` have one.
fn is_github_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..")
        && !name.starts_with('-')
        && !name.ends_with(".git")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// `wanted` if gh is logged in as it, else the logged-in account named
/// `owner` (the remote's owner, or a re-run's account), asked on a terminal
/// from the logged-in ones.
fn account(owner: &str, wanted: Option<&str>, ask: &ask::Asker) -> Result<String> {
    let logins = gh_logins()?;
    let logins: Vec<&str> = logins
        .iter()
        .map(String::as_str)
        .filter(|l| is_github_name(l) && !l.starts_with('.'))
        .collect();
    if logins.is_empty() {
        bail!("gh has no logged-in account; run `gh auth login`");
    }
    let found = logins.join(", ");
    let logged_in = |name: &str| logins.iter().find(|l| l.eq_ignore_ascii_case(name));
    ask.value(
        &format!("gh account (logged in: {found})"),
        explain::GH_ACCOUNT,
        wanted,
        logged_in(owner).copied(),
        &format!(
            "no logged-in gh account matches the remote owner {owner} \
             (found: {found}); pass --gh-account <login>"
        ),
        |wanted| match logged_in(wanted) {
            Some(login) => Ok(login.to_string()),
            None => bail!("gh has no logged-in account {wanted} (found: {found})"),
        },
    )
}

/// The labels the herd's queue and workers use: name, description and color.
fn wanted_labels(
    labels: &tracker::Labels,
    wayfinding: bool,
    grill_batch: bool,
) -> Vec<(&str, &str, &str)> {
    let mut wanted = vec![
        (
            labels.ready_for_agent.as_str(),
            "Fully specified; an agent may claim it",
            "0E8A16",
        ),
        (
            &labels.needs_info,
            "Stopped on a question for the human",
            "D876E3",
        ),
        (
            &labels.ready_for_human,
            "Needs a human to work it",
            "FBCA04",
        ),
        ("blocked", "Waiting on other issues", "B60205"),
    ];
    if wayfinding {
        wanted.extend(
            WAYFINDING_SKILLS.map(|(label, _, description)| (label, description, "1D76DB")),
        );
    }
    if grill_batch {
        wanted.extend([
            ("grilling", "A decision to grill before building", "5319E7"),
            (
                "grill:batch",
                "Grilled unattended, as one batch to review",
                "5319E7",
            ),
        ]);
    }
    wanted
}

/// Creates each label the herd's queue and workers use that the repo lacks,
/// one line per label created.
fn create_labels(
    token: &str,
    slug: &str,
    labels: &tracker::Labels,
    wayfinding: bool,
    grill_batch: bool,
) -> Result<Vec<String>> {
    let wanted = wanted_labels(labels, wayfinding, grill_batch);
    let existing = gh(
        Some(token),
        &[
            "label", "list", "--repo", slug, "--limit", "1000", "--json", "name", "--jq",
            ".[].name",
        ],
    )?;
    let existing: Vec<&str> = existing.lines().collect();
    let mut created = vec![];
    for (name, description, color) in wanted {
        if existing
            .iter()
            .any(|label| label.eq_ignore_ascii_case(name))
        {
            continue;
        }
        gh(
            Some(token),
            &[
                "label",
                "create",
                name,
                "--repo",
                slug,
                "--description",
                description,
                "--color",
                color,
            ],
        )?;
        created.push(format!("created label {name} in {slug}"));
    }
    Ok(created)
}

/// The github.com logins gh holds, unfiltered. A gh too old for
/// `auth status --json` is read from its plain `auth status` lines instead.
fn gh_logins() -> Result<Vec<String>> {
    let args = [
        "auth",
        "status",
        "--json",
        "hosts",
        "--jq",
        r#".hosts["github.com"][]?.login"#,
    ];
    let output = gh_output(None, &args)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(String::from)
            .collect());
    }
    if !stderr.contains("unknown flag: --json") {
        bail!("gh {}: {}", args.join(" "), stderr.trim());
    }
    // Plain auth status prints on stdout or stderr depending on the gh version,
    // and gh before 2.54 exits 0 even when every account fails, so the exit
    // code says nothing; any output without a login is gh's failure.
    let args = ["auth", "status", "--hostname", "github.com"];
    let output = gh_output(None, &args)?;
    let text = [output.stdout, output.stderr].concat();
    let text = String::from_utf8_lossy(&text);
    let logins: Vec<String> = text
        .lines()
        .filter_map(plain_login)
        .map(String::from)
        .collect();
    if logins.is_empty() && !text.trim().is_empty() {
        bail!("gh {}: {}", args.join(" "), text.trim());
    }
    Ok(logins)
}

/// The login on a plain `gh auth status` line, in its newer
/// `Logged in to github.com account <login>` or older `… as <login>` form.
fn plain_login(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once("Logged in to github.com ")?;
    let rest = rest
        .strip_prefix("account ")
        .or_else(|| rest.strip_prefix("as "))?;
    rest.split_whitespace().next()
}

/// gh's stdout; `token` runs it as that account, else as gh's own login.
fn gh(token: Option<&str>, args: &[&str]) -> Result<String> {
    let output = gh_output(token, args)?;
    if !output.status.success() {
        bail!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn gh_output(token: Option<&str>, args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("gh");
    command.args(args);
    match token {
        Some(token) => command.env("GH_TOKEN", token),
        None => command.env_remove("GH_TOKEN"),
    };
    command
        .output()
        .context("run gh; is the GitHub CLI installed?")
}

/// `value` as literal text inside the worker prompt, a `'''` TOML string
/// whose `{key}` placeholders loopherd fills.
fn prompt_text(value: &str) -> Result<String> {
    if value.contains("'''") || value.chars().any(|c| c.is_control()) {
        bail!("{value:?} can't go in the worker prompt: it has ''' or a control character");
    }
    Ok(value.replace('{', "{{").replace('}', "}}"))
}

/// `text` with each `@KEY@` of `values` replaced in one pass, so a value
/// naming another key stays literal.
fn fill(text: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let value = after.split_once('@').and_then(|(key, _)| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (key.len(), *value))
        });
        match value {
            Some((len, value)) => {
                out.push_str(value);
                rest = &after[len + 1..];
            }
            None => {
                out.push('@');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn toml_string(value: &str) -> String {
    toml_edit::Value::from(value).to_string().trim().to_string()
}

/// Writes `text` to a new file at `path`, leaving no file behind on failure.
fn write_new(path: &Path, text: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    if let Err(e) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e).with_context(|| format!("write {}", path.display()));
    }
    Ok(())
}
