# loopherd

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

```bash
scripts/install --settings <file>   # hooks in another settings file; pass the same
                                    # --settings <file> to install-hook --remove and watch
git pull && scripts/install         # update; replaces the binary, edits settings only if out of date
scripts/install --no-splash         # skip the opening animation (or LOOPHERD_NO_SPLASH=1)
loopherd install-hook --remove      # remove the hooks ...
rm ~/.local/bin/loopherd            # ... and the binary
```

Claude Code asks whether to trust a repo the first time it starts there, and
`loopherd spawn`/`add` stop at that dialog (exit 3). Trust each repo once
from its main checkout (`cd <repo> && claude`, accept, exit); every worktree
shares it ([details](docs/research/claude-workspace-trust.md)). If a spawn
still blocks, answer the dialog in the pane it names and run
`loopherd prompt <name>` to send the stored prompt.

## Quickstart

A herd is one TOML file; the filename is the herd's name. Herd files live in
`$XDG_CONFIG_HOME/loopherd/herds/<herd>.toml` (`~/.config/loopherd/herds/`
by default; `LOOPHERD_CONFIG_DIR` replaces the `loopherd` directory).

```toml
# ~/.config/loopherd/herds/zagops-prs.toml
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
loopherd add zagops-prs pr 1050 --dry-run
loopherd add zagops-prs pr 1050         # worktree + herdr tab + named agent + prompt
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
loopherd feed zagops-prs --dry-run      # run source and done checks; change nothing
loopherd feed zagops-prs                # retire finished members, fill free slots
```

Exits 3 when a member is blocked at startup, 4 when the only errors were
malformed source lines.

### Report and reports: the mailbox

Members leave reports from their own pane; the shepherd reads them.

```bash
loopherd report "<text>"                        # from a member's pane; --kind done|question|note
loopherd report --kind question "<question>"
loopherd reports                                # unread, questions first; marks them read
loopherd reports --herd zagops-prs
loopherd reports --all [<name>...]              # everything, marks nothing read
```

### Watch: block until a member needs you

```bash
loopherd watch                              # leads and unled herds
loopherd watch zagops-prs --timeout 3600    # exits 2 with no output on timeout
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
(merges, installs, rebases and force-pushes) by denying the Bash command;
`new_cards`, `design_escalation` and `blocked_dialogs` rely on the prompt.

```toml
[lead]
prompt_file = "zagops-prs-lead.md"     # beside the herd file; must exist
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
loopherd lead zagops-prs --dry-run
loopherd lead zagops-prs
loopherd watch zagops-prs               # from your pane: wakes on the lead, not its workers
```

A complete lead setup is in [docs/examples/](docs/examples/). The playbook
the shepherd and leads follow is
[docs/agents/shepherd.md](docs/agents/shepherd.md).

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
[docs/agents/issue-tracking.md](docs/agents/issue-tracking.md).

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test
LOOPHERD_LIVE=1 scripts/herdr-dev cargo test --test live   # against a dev herdr session
scripts/trello help                                        # needs TRELLO_API_KEY / TRELLO_TOKEN
```

## License

GPL-3.0. See [LICENSE](LICENSE).
