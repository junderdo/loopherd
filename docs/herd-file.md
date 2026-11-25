# An annotated herd file

A herd is one TOML file in loopherd's herds dir
(`~/.config/loopherd/herds/<herd>.toml`), named after the herd. `loopherd
init` writes one for a repo; this page explains each part, for tailoring
what init wrote or writing one by hand. [setup.md](setup.md) walks through
init; [tour.md](tour.md) lists the commands that read the file.

The example herd, `my-app`, works GitHub issues labelled `ready-for-agent`,
one worker per issue on its own branch, with a lead. Replace `my-app`,
`my-org/my-app`, `my-gh-user` and the checks with your project's.

## The herd file

```toml
# The main checkout. Members get worktrees beside it.
repo = "~/src/my-app"
# Passed to claude for every member, before a template's own args.
args = ["--permission-mode", "auto"]
# Shell commands `loopherd feed` runs in `repo` once per member after its
# work merges, outside any sandbox. Optional.
after_merge = ["git pull --ff-only"]
# Minutes a member may sit idle after a paused stop before a watch prints
# `<member> stalled`. Optional; default 30.
stalled_after = 30

# What `loopherd feed` starts.
[queue]
# The template each task runs. A herd with several sets
# `templates = ["issue", "docs"]` instead, and each task's first word
# names its template.
template = "issue"
# A command printing one task per line: its words fill the template's
# params, in order. Here "<number> <type> <skill> <slug>".
source = '''
gh issue list --repo my-org/my-app --state open --label ready-for-agent --json number,title,labels --jq '
  sort_by(.number)[]
  | select(any(.labels[].name; . == "blocked" or . == "needs-info") | not)
  | ([.title | ascii_downcase | scan("[a-z0-9]+")][:5] | join("-")) as $slug
  | "\(.number) feat tdd \(if $slug == "" then "issue" else $slug end)"'
'''
# Optional: a command printing one unready task per line, "<id> <title>".
# A watch prints `<herd> running low` when few tasks are left, and the lead
# triages these for the human.
backlog = '''
gh issue list --repo my-org/my-app --state open --json number,title,labels --jq '
  sort_by(.number)[]
  | select(any(.labels[].name; . == "ready-for-agent") | not)
  | "\(.number) \(.title)"'
'''
# How many members feed keeps running at once.
max = 3
# Claimable tasks at or below this count as running low; unset, twice max.
# low_at = 6

[template.issue]
# The words each task line carries, in order.
params = ["number", "type", "skill", "slug"]
# The member's name. {herd} and any param are placeholders.
name = "{herd}-{number}"
# The branch its worktree is cut on, from the repo's default branch. A
# review-only template sets `worktree = "pr-{number}"` instead, for a
# detached worktree.
branch = "{type}/{number}-{slug}"
# Exits 0 once the member's task is finished, so feed retires it and frees
# its slot. Placeholder values arrive shell-quoted.
done = "gh issue view {number} --repo my-org/my-app --json state --jq .state | grep -qx CLOSED"
# Exits 0 once its work is merged; the lead's watch wakes on it.
merged = "gh pr list --repo my-org/my-app --head {branch} --state merged --json number --jq 'length > 0' | grep -qx true"
# The member's first message. Besides params it takes {name}, {branch},
# {worktree} and {pane}. `prompt_file = "<file>"`, beside the herd file,
# works too.
prompt = '''
You're {name}, working GitHub issue #{number} of my-org/my-app on branch
{branch} in {worktree}.

Run every gh command as my-gh-user.

1. Read the issue and every comment: `gh issue view {number} --comments`.
2. Claim it: `gh issue comment {number} --body "claimed by {name}"`.
3. Build it with the {skill} skill.
4. Before calling it done, run each check and fix what fails:
   - `npm run lint`
   - `npm test`
5. Push the branch and open a PR against main whose body ends with
   `Closes #{number}`.
6. Finish with `loopherd report --kind done "<PR link and one-line summary>"`.

If main moves under you, merge origin/main into your branch; never rebase
or force-push.

If you hit a question only the human can answer, stop rather than guess:
add the `needs-info` label, comment the question and the options you see,
then `loopherd report --kind question "<the exact question>"`.
'''
# Optional:
# args = ["--model", "sonnet"]   # after the herd's args
# attended = true                # the human sits in the member's pane; only `add` starts one
# slot = false                   # its members hold no `max` slot

# The herd's lead, `lead-my-app`: a member that runs the shepherd's loop
# for the herd with no human in its pane. Started by `loopherd lead my-app`.
[lead]
# Its prompt, beside the herd file (below). `prompt = '''…'''` works too.
prompt_file = "my-app-lead.md"
# Switches: what the lead may do on its own. Off, it asks the human with a
# question report. One left unset comes from [lead_defaults] in
# config.toml, else is off.
merge = false              # merge a verified PR
install = false            # install loopherd and its hooks while feeding
rebase = false             # have a member rebase and force-push
new_cards = false          # create follow-up tasks for minor or out-of-scope findings
design_escalation = false  # settle a design question a member raises
blocked_dialogs = false    # answer a member's blocking dialog
follow_ups_ready = false   # file a qualifying follow-up ready to claim; needs new_cards
# Review rounds on one PR before it asks the human; default 3.
review_rounds = 3
# The checks `loopherd gate <sha>` runs on a PR's head merged with its base,
# outside the lead's sandbox.
gate = ["npm run lint", "npm test"]
# How the lead is kept from merging while `merge` is off: "hook" (default)
# or "credentials".
# merge_guard = "hook"
# "require" (default) refuses to start the lead without the Claude Code
# sandbox; "prefer" starts it guarded by its hook alone. Only the human
# loosens it.
# sandbox = "require"
```

A template with no `name` takes its first param as the member name. Any
param used in `name`, `branch` or `worktree` must be a plain word
(`[A-Za-z0-9][A-Za-z0-9._-]*`, no `..`).

Edit a herd file with `loopherd herd set <herd> <key> <value>` (and
`herd unset`) rather than by hand where you can: it validates the result.
`loopherd herds` reports each file valid or not, and `loopherd feed <herd>
--dry-run` shows what the queue would start.

## The lead's prompt

`my-app-lead.md`, beside the herd file. `{name}`, `{herd}`, `{worktree}`,
`{switches}` and `{review_rounds}` are filled when the lead starts. Keep
the project's specifics here (its tracker, merge line, after-merge steps);
the loopherd skill carries the loop itself.

```markdown
You're {name}, the lead of herd {herd}, in {worktree}, a detached checkout
of my-app you never commit in. No human is in this pane.

Read `loopherd --skill` in full, its Lead section most closely, then
AGENTS.md and docs/agents/shepherd.md. Run every gh command as my-gh-user.
You run the shepherd's loop for {herd}'s workers: feed, watch, verify each
worker's PR with a reviewer subagent, send only blocking findings back to
the member, and the after-merge steps.

Your switches (off means the human does it; ask with a question report):
{switches}

Your review-round limit: {review_rounds}.

Start now:
1. `loopherd status` and `loopherd reports --all`, to see where the herd is.
2. `loopherd feed {herd} --dry-run`, then `loopherd feed {herd}` if it reads right.
3. Arm `loopherd watch {herd} --timeout 3600` as a background task, and
   re-arm after every exit but `{herd} drained`: on that, report done and
   stop. On `<member> merged`, run `loopherd feed {herd}`. On
   `<name> claimable`, feed. Exit 2 is your heartbeat: check each worker's
   PR for a merge and run `loopherd feed {herd} --dry-run`.

Before merging a verified PR, or asking the human to, run `loopherd gate
<full head sha>` as a background task, and go on only on its `pass` line.

When you need the human, run `loopherd report --kind question "<the exact
question>"` and end your turn. With merge off, a merge question carries the
PR link and the line
`! gh pr merge <n> --merge --match-head-commit <full sha>` for the head your
reviewer verified.
```
