# loopherd

<img width="666" height="374" alt="Adobe Express - Adobe Express - loopherd-splash" src="https://github.com/user-attachments/assets/bc27723b-1ee9-4b1f-afea-e8bc408d9022" />

Herd your coding agents. loopherd is a Rust CLI and TUI for Linux that groups
coding agents into **herds** and keeps them working together across sessions
and restarts. It layers on [herdr](https://herdr.dev/), which runs the
terminals and knows whether each agent is working, blocked, or done.

- **Templates**: declare once how to spawn a member (repo, worktree, prompt),
  then `loopherd add <herd> <template> <params…>`.
- **Herds across repos**: one view of every member, sorted by what needs you.
- **Revival**: a ledger of each member's task and Claude session, so a reboot
  or a closed tab doesn't lose it (`claude --resume`).
- **Queues**: `feed` fills a herd's free slots from a task source and retires
  members whose work is done.
- **Cooperation**: members report to a mailbox, `watch` wakes you when one
  needs you, and a lead member runs a herd's loop unattended within switches
  you set.

## Prerequisites

| Need | Why | Install |
|---|---|---|
| Rust toolchain (`cargo`) | builds loopherd | [rustup.rs](https://rustup.rs/) |
| `jq` | `scripts/install` reads cargo's build output | [jqlang.github.io/jq](https://jqlang.github.io/jq/download/) |
| herdr | the terminal multiplexer loopherd drives | [herdr.dev](https://herdr.dev/) |
| Claude Code (`claude`) | the agent each member runs | [code.claude.com](https://code.claude.com/docs/en/overview) |

## Install

```bash
scripts/install
```

Builds a release binary, installs it as `~/.local/bin/loopherd`, and adds
loopherd's Stop, StopFailure, SessionStart and PreToolUse hooks to
`$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`. When
that file exists and changes, a timestamped `settings.json.<time>.bak` is
written beside it first. The hooks act only in a herd member's pane: they
record each stop in the member's mailbox, keep its session current across
`/clear` and resume, and deny a lead's merge, install, rebase and force-push
commands while its herd file's switch is off. `~/.local/bin` must be on
`PATH`.

On a terminal it then offers loopherd's own agent types (`builder`,
`reviewer`, `researcher`, `scout`) for `agents/` beside the settings file,
default no. Each installed file's hash goes in
`$XDG_STATE_HOME/loopherd/agents.sha256`; a later run updates only files
still as loopherd left them, and asks before touching one you edited or own:
keep it, overwrite it after a `.bak`, or install loopherd's as
`loopherd-<name>`.

It then offers the `mattpocock-skills@claude-plugins-official` plugin, also
default no, first listing any skills in `skills/` beside the settings file
that share a name with the plugin's (both load; the plugin's are prefixed
`mattpocock-skills:`). It adds the `anthropics/claude-plugins-official`
marketplace if missing, leaves an installed plugin alone, and skips the
offer when there is no `claude` CLI. With `--settings <file>`, `claude`
runs with `CLAUDE_CONFIG_DIR` set to that file's directory, so the plugin
goes there too, and the installer names that dir when it asks and reports.

```bash
scripts/install --with-agents       # install or update the agent types and ask nothing else;
                                    # with no terminal, keeps any file of yours and says so
scripts/install --with-skills       # install the mattpocock-skills plugin and ask nothing else
scripts/install --no-extras         # neither extra, no questions
scripts/install --settings <file>   # hooks in another settings file; pass the same
                                    # --settings <file> to install-hook --remove and watch
git pull && scripts/install         # update; replaces the binary, edits settings only if out of date
scripts/install --no-splash         # skip the opening animation (or LOOPHERD_NO_SPLASH=1)
loopherd install-hook --remove      # remove the hooks ...
rm ~/.local/bin/loopherd            # ... and the binary
```

Installed agent types are yours to remove: the files listed in
`$XDG_STATE_HOME/loopherd/agents.sha256`, then that file.

Claude Code asks whether to trust a repo the first time it starts there, and
`loopherd spawn`/`add` stop at that dialog (exit 3). Trust each repo once
from its main checkout (`cd <repo> && claude`, accept, exit); every worktree
shares it ([details](docs/research/claude-workspace-trust.md)). If a spawn
still blocks, answer the dialog in the pane it names and run
`loopherd prompt <name>` to send the stored prompt.

## Set up a repo

For a repo whose `origin` is on GitHub, with its tasks as GitHub issues:

```bash
cd <repo>
loopherd init   # [--check <cmd>]... [--max <n>] [--no-lead] [--context-limit <tokens>] [--herd <name>] [--gh-account <login>] [--repo <owner>/<name>] [--tracker github|local|other [--tracker-desc <text>]]
```

On a terminal, init asks for the repo, herd name, GitHub repo, checks, how
many workers run at once (`max`, default 3), whether the herd gets a lead,
the workers' handoff limit (`context_limit`; Enter leaves it unset so
`config.toml`'s default applies), the gh account and the default branch,
each with what it inferred as the default, then shows a summary to confirm
before writing. A flag answers its question. Checks are suggested from
`Cargo.toml` (`cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`) or `package.json` (`npm run lint`, `npm test`). Without a
terminal it uses flags and inferred values, and names the flag for any it
can't infer.

init reads the tracker from `docs/agents/issue-tracker.md`, in
mattpocock-skills' format. With none, it takes `--tracker` (`other` with
`--tracker-desc`), else asks on a terminal, else uses `github`, and writes
that doc plus `docs/agents/triage-labels.md` with the default labels,
uncommitted. A doc titled `# Issue tracker: GitHub` (any case) gets the
GitHub snippets below; any other tracker gets an empty queue `source` that
`loopherd herds` reports invalid until you fill it from the doc, and no
labels. When `triage-labels.md` exists, its labels for
`ready-for-agent`, `needs-info` and `ready-for-human` replace those names
below; `blocked` is always loopherd's.

It writes `<herd>.toml` (named after the repo dir) and, unless
`--no-lead`, `<herd>-lead.md` in the herds dir, and for GitHub creates the labels `ready-for-agent`, `needs-info`,
`ready-for-human` and `blocked` where missing. The herd feeds one worker per
open issue labelled `ready-for-agent` and none of the other three, on branch
`<type>/<n>-<slug>`; a `skill:<name>` label picks the skill it builds with
(`tdd` by default). Every gh call runs as the logged-in account matching the
remote's owner, else `--gh-account`; `--repo` names the GitHub repo when
origin is an ssh host alias. init infers the rest, commits nothing,
and refuses to replace either file (`--no-lead` leaves an existing lead
prompt alone); the files are yours to edit. Then follow
the steps it prints: trust the repo in Claude once, fill the queue `source`
for a tracker other than GitHub, `loopherd herds`,
`loopherd feed <herd> --dry-run`, `loopherd lead <herd>`.

## Quickstart

A herd is one TOML file; the filename is the herd's name. Herd files live in
`$XDG_CONFIG_HOME/loopherd/herds/<herd>.toml` (`~/.config/loopherd/herds/`
by default; `LOOPHERD_CONFIG_DIR` replaces the `loopherd` directory).

```toml
# ~/.config/loopherd/herds/my-project.toml
repo = "~/work/projects/zagops/zagops"      # the main checkout
workspace = "zagops"                        # herdr workspace label (optional)
args = ["--permission-mode", "auto"]        # passed to claude (optional)

[template.pr]
params = ["number"]
name = "zpr-{number}"
worktree = "pr-review-{number}"             # detached worktree; or branch = "..."
prompt = "You're {name}. Run `gh pr checkout --detach {number}`, read the review comments and CI, and report what's needed."
```

Patterns take `{param}` placeholders plus `{herd}`; `branch` and `worktree`
also take `{name}`; `prompt`, `done` and `merged` also take `{name}`,
`{branch}`, `{worktree}` and `{pane}`. `{{` and `}}` are literal braces (jq
filters in a `done` check need them). `prompt_file = "<file>"`, read beside
the herd file, is used instead of `prompt`; setting both is an error.
Unknown keys are rejected.

```bash
loopherd herds                          # list herd files; exits 1 if any is invalid
loopherd add my-project pr 1050 --dry-run
loopherd add my-project pr 1050         # worktree + herdr tab + named agent + prompt
loopherd status                         # members by herd, blocked/done first
loopherd view                           # live TUI; Enter focuses the pane, a/s toggle retired/strays
loopherd revive --dry-run               # after a herdr restart or a closed tab
loopherd revive                         # resume every non-retired member by session ID
loopherd retire zpr-1050                # ledger only
loopherd retire zpr-1050 --close        # also close the tab; remove worktree and branch if clean and merged
```

Without a herd file: `loopherd spawn --name <name> --repo <repo>
--worktree <branch> --prompt "<text>" [-- <claude args>]`.

## Tour

### Feed: a queue per herd

Add a `[queue]` and a `done` check to the template, and `feed` keeps `max`
members working on whatever the source prints: one task per line, one
whitespace-separated value per param.

```toml
[queue]
template = "pr"
source = "gh pr list --json number --jq '.[].number'"
max = 3

[template.pr]
# ... as above, plus:
done = "gh pr view {number} --json state --jq .state | grep -qx MERGED"
```

```bash
loopherd feed my-project --dry-run      # run source and done checks; change nothing
loopherd feed my-project                # retire finished members, fill free slots
```

Exits 3 when a member is blocked at startup, 4 when the only errors were
malformed source lines.

### Report and reports: the mailbox

Members leave reports from their own pane; the shepherd reads them.

```bash
loopherd report "<text>"                        # from a member's pane; --kind done|question|note
loopherd report --kind question "<question>"
loopherd reports                                # unread, questions first; marks them read
loopherd reports --herd my-project
loopherd reports --all [<name>...]              # everything, marks nothing read
loopherd stats [--herd <herd>] [--since 2026-11-01] [--json]   # how each herd's work flowed
```

### Watch: block until a member needs you

```bash
loopherd watch                              # leads and unled herds
loopherd watch my-project --timeout 3600    # exits 2 with no output on timeout
```

Wakes on an unread report, a stop that leaves a member paused, finished or
failed, or a member going blocked or gone in herdr; prints one line per event
gathered in that wake, then exits. From your pane it covers leads and unled
herds; a led herd's workers are its lead's to watch. Needs the hooks from
`scripts/install`. One watch per reader at a time.

### Lead: a herd that runs itself

A `[lead]` table lets `loopherd lead <herd>` start `lead-<herd>` in a
detached worktree of the herd's repo. Its prompt file tells it to run the
feed/watch loop for that herd's workers and to ask you, with a question
report, before anything a switch leaves off. Every switch is off until you
turn it on. The PreToolUse hook enforces `merge`, `install` and `rebase`
(merges, installs, rebases and force-pushes) by denying the Bash command,
and denies a lead's `loopherd restart --force` outright;
`new_cards`, `design_escalation` and `blocked_dialogs` rely on the prompt.

```toml
[lead]
prompt_file = "my-project-lead.md"     # beside the herd file; must exist
merge = false
install = false
rebase = false
new_cards = false
design_escalation = false
blocked_dialogs = false
```

The prompt file takes `{name}`, `{herd}`, `{worktree}` and `{switches}` (one
`<switch> on|off` line each); start from
[docs/examples/loopherd-cards-lead.md](docs/examples/loopherd-cards-lead.md).

```bash
loopherd lead my-project --dry-run
loopherd lead my-project
loopherd watch my-project               # from your pane: wakes on the lead, not its workers
```

A complete lead setup is in [docs/examples/](docs/examples/). The playbook
the shepherd and leads follow is
[docs/agents/shepherd.md](docs/agents/shepherd.md).

### Handoff at a context limit

Past its context limit, a member's Stop hook asks it, once per session, to
write a handoff to `$XDG_STATE_HOME/loopherd/handoffs/<name>.md` and report
`--kind handoff`. `loopherd restart <name>` then clears its session and has
it resume from that file, in the same pane and worktree.

```toml
# in a [template.*] or [lead] table
context_limit = 150000

# $LOOPHERD_CONFIG_DIR/config.toml (optional)
default_context_limit = 150000   # for members without their own
context_window = 200000          # limits are capped at 80% of this
```

## Status

Spawn, revive, herds and templates, status/view, feed, the mailbox and watch,
and leads all ship (slices 1–6, D5–D17). Direction, decisions, and open
questions are in [docs/wayfinding.md](docs/wayfinding.md); what herdr offers
is in [docs/research/herdr.md](docs/research/herdr.md).

## How it's built

loopherd is also a loop-engineering experiment: much of it is written by
agents pulling cards from the
[Trello board](https://trello.com/b/MtiTIrp1/loopherd), with tests and lints
as the back-pressure. Agent instructions start at [AGENTS.md](AGENTS.md);
the card workflow is
[docs/agents/issue-tracker.md](docs/agents/issue-tracker.md).

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test
LOOPHERD_LIVE=1 scripts/herdr-dev cargo test --test live   # against a dev herdr session
scripts/trello help                                        # needs TRELLO_API_KEY / TRELLO_TOKEN
```

## License

GPL-3.0. See [LICENSE](LICENSE).
