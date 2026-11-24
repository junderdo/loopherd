# Setting up a repo

The [README](../README.md#a-first-herd) has the short version. This page
walks the same steps with the detail: what `init` writes, the trust dialog,
the shepherd, the lead and its sandbox, and writing a herd file by hand.

Start with loopherd [installed](install.md), herdr running, and the repo
cloned as its main checkout. Then, in order:

1. **Trust the repo in Claude Code.** Members can't start past Claude's
   trust dialog, so accept it once from the main checkout: `cd <repo> &&
   claude`, accept, exit. Every worktree shares it
   ([details](research/claude-workspace-trust.md)).
2. **Write the herd.** In the repo, run `loopherd init`. It asks on a
   terminal and takes flags otherwise (`loopherd init --help`). Follow the
   steps it prints up to `loopherd herds`, filling each `(fill in)`
   placeholder it lists, then review and commit the repo files it added.
   Its last steps, the checks and `loopherd lead`, come below.
3. **Check it.** `loopherd herds` calls the herd file valid, and
   `loopherd feed <herd> --dry-run` lists the tasks you expect.
4. **Open the herd view.** In a new herdr tab, run `loopherd view`. It
   lists every member, sorted by what needs you, with the subagents each
   is running; Enter focuses the selected member's pane, `s` focuses the
   shepherd (or, with none, starts one in a pane split left of the view, or
   below it with `view.layout` set to `stacked`), `v` switches the view and
   the shepherd between side by side and stacked, saving it as
   `view.layout`, and `t` switches between its purple-orange, phosphor and
   catppuccin themes, remembering the choice as `view.theme` in
   `config.toml`; `x` shows or hides strays. The detail panel sits beside the list while it fits, and
   under the selected member otherwise.
5. **Start the shepherd.** Press `s` in the view, run in a pane rather
   than the plugin's popup, to start it beside the view, in the view's
   directory (a plugin pane's workspace directory, not the plugin's). Or in
   another herdr tab, `cd` to the main checkout, run `claude`, and give it
   this first prompt; the skill has it name its herdr agent `shepherd`, so
   `s` then finds it:

   ```text
   You're the shepherd for the <herd> herd in this repo. Read `loopherd --skill`
   and follow its Shepherd section: feed the herd, watch it, verify members'
   work, and clean up after merges.
   ```

6. **Start the lead,** if the herd file has a `[lead]` table:
   `loopherd lead <herd>`. Run it yourself, or ask the shepherd to. The
   lead, `lead-<herd>`, then feeds and watches the herd's workers within
   the switches you set; the shepherd's watch reads the lead and relays
   its questions to you.

The guide a shepherd or lead follows, from setting up a herd to cleaning up
after merges, is the loopherd skill: `loopherd --skill` prints it
(`--section lead` prints one section).

## The lead's sandbox

The lead runs in the Claude Code sandbox, which limits its writes to its
own worktree and loopherd's state dir, so it doesn't change herd files,
`config.toml` or `~/.claude` by mistake. It is a guard rail, not a cage
([D89](decisions/D89.md)).

- **Unix sockets.** It gets none, blocked by Seatbelt on macOS and by
  Claude Code's seccomp filter on Linux and WSL2, where present, so herdr,
  your ssh agent and WSL2's Windows interop are closed to it, unless its
  `[lead]` sets `unix_sockets = "all"`, which reopens all three
  ([D105](decisions/D105.md)). A lead with `merge_guard = "credentials"`
  is refused it ([D114](decisions/D114.md)): `loopherd lead`, `revive` and
  `restart` refuse the pair, `loopherd herd set` refuses an edit that
  makes it, and `loopherd doctor` fails the herd.
- **No seccomp filter.** Where, on Linux or WSL2, Claude Code has no
  seccomp filter to block them, `loopherd doctor` and `loopherd setup` name
  `npm install -g @anthropic-ai/sandbox-runtime`, and the lead isn't
  started or restarted unless its `[lead]` sets `sandbox = "prefer"`; one
  with `merge_guard = "credentials"` isn't started or restarted even then.
  Under `unix_sockets = "all"` the socket check is skipped: the lead isn't
  refused or flagged for open sockets, though `require` still refuses it
  when the sandbox can't start at all.
- **What still gets past it.** Workers run unsandboxed, so a lead can have
  one act for it (LH-284), and most of the state dir stays writable to it
  (LH-298).
- **The broker.** Its `tell`, `read`, `status`, `feed`, `add`, `prompt`,
  `restart`, `retire`, `release` and `gate` run in its broker, a process
  `loopherd lead` starts outside the sandbox, which acts only on the lead's
  own herd ([D98](decisions/D98.md)). With the broker down they fail,
  naming `loopherd revive --name <lead>`, which starts it again.
  `loopherd gate <sha>` runs the herd file's `[lead] gate` commands there
  on a pushed commit merged with origin's default branch (`--base` for a
  stacked PR's), so a lead without sockets can still run checks that need
  them ([D149](decisions/D149.md)).
- **Loosened settings.** A lead whose sandbox can't start, or whose sandbox
  your own `~/.claude/settings.json` loosens (an `excludedCommands` or
  `allowWrite` entry, say), isn't started unless its `[lead]` sets
  `sandbox = "prefer"`, which starts it with a warning instead
  ([D100](decisions/D100.md)). A `merge_guard = "credentials"` lead isn't
  started either way; set `merge_guard = "hook"` to start it with the
  warnings.
- **Claude args.** A lead's Claude args (`[lead].args`, else the herd's
  `args`, then the command line) may be only `--model`, `--effort`,
  `--verbose` and `--permission-mode` with `default`, `manual`, `auto` or
  `plan`; any other is refused or warned of the same way, but loopherd's
  own launch flags, `--settings`, `--mcp-config` and `--strict-mcp-config`,
  are refused under both `require` and `prefer`. Under `require`, a lead
  started with an arg now off that list can't be restarted or revived
  until you retire it and run `loopherd lead` again.
- **MCP servers.** A lead starts with none; list the ones it may use, by
  the name `claude mcp add --scope user` gave them, in `[lead]
  mcp_servers`. Those run outside the sandbox.

## What `init` writes

`loopherd init` infers the repo, herd name, GitHub repo and account,
default branch, and checks from `Cargo.toml` and `package.json`, and on a
terminal asks for each with the inferred value as the default. For a local
tracker on a GitHub origin, the GitHub repo question defaults to none (no gh
calls, no PR `merged` check); `--no-github` makes that choice off a
terminal. It writes `<herd>.toml` and `<herd>-lead.md` in
`$XDG_CONFIG_HOME/loopherd/herds/` (default `~/.config/loopherd/herds/`;
`LOOPHERD_CONFIG_DIR` replaces the `loopherd` dir), creates the labels the
queue reads for a GitHub tracker, and, uncommitted, adds
`docs/agents/loopherd.md`, a tracker doc if the repo has none, and a
`## loopherd` section in `AGENTS.md`. The tracker can be GitHub issues,
local markdown issue files, or another one you describe. `init --diff`
shows what it would write now, against what it wrote.

Its printed steps also cover adding an origin remote and writing by hand a
repo file init left alone (a symlinked `AGENTS.md`, a doc that would land
outside the repo), each when it applies, and always tailoring the herd to
the repo per "Setting up a project" in `loopherd --skill`.

## If a spawn blocks on the trust dialog

A spawn that still meets the dialog exits 3 and names the pane. Answer the
dialog there, then run `loopherd prompt <name>` to send the member its
prompt.

## Writing a herd file by hand

A herd is one TOML file in the herds dir, named `<herd>.toml`. A minimal one
with a single template:

```toml
repo = "~/src/my-project"                   # the main checkout
args = ["--permission-mode", "auto"]        # passed to claude (optional)

[template.pr]
params = ["number"]
name = "pr-{number}"
worktree = "pr-review-{number}"             # detached worktree; or branch = "..."
prompt = "You're {name}. Run `gh pr checkout --detach {number}` and review it."
```

Then `loopherd add my-project pr 1050`. Patterns take `{param}`
placeholders and `{herd}`; `prompt`, `done` and `merged` also take
`{name}`, `{branch}`, `{worktree}` and `{pane}`. A param used in `name`,
`branch` or `worktree`, or any param of a template with no `name`, must be a
plain word (`[A-Za-z0-9][A-Za-z0-9._-]*`, no `..`). A `[queue]`, a `[lead]`
table and the rest are what `init` writes; start from its output or from
[examples/](examples/).

An optional top-level `stalled_after = <minutes>` (default 30, at least 1)
sets how long a member may sit idle after a paused stop before a watch
says `<member> stalled`; `status` shows it as `stalled <n>m`
([D152](decisions/D152.md)).

### Attended templates

A template with `attended = true` starts a member the human sits in, such
as a grill tied to a card (D112). Only `add` starts one: it focuses the new
pane (`--no-focus` skips that), records the caller's pane and fills a
`{shepherd_pane}` placeholder with it (`herdr agent focus` takes only an
agent's pane, so run `add` from one). If the human is still in the member's
pane, `retire --close` focuses the live shepherd's pane, as the view's `s`
finds it, else that recorded pane, else warns and leaves focus to herdr. It
holds no `max` slot, is never asked to hand off, and is left out of
`stats`. A watch wakes on its `gone` alone, and a lead's watch also on its
`merged`.

A grill runs interactively or in batch: the `grill:batch` label picks
batch, where it answers every question with its pick and the human accepts
or overrides by number once ([D132](decisions/D132.md));
[examples/loopherd-cards-grill.toml](examples/loopherd-cards-grill.toml)
and [examples/loopherd-cards-routed.toml](examples/loopherd-cards-routed.toml)
show both.

### Slotless templates

A template with `slot = false` (default `true`) starts members that hold no
`max` slot: `feed` and `add` don't count them, `add` never warns past `max`
for one, and the watch doesn't count one as an open worker for `no workers
open`, `out of work`, `drained` or `all drained`. Its own `done`, stop and
`gone` events still print. Only `add` starts one, as `max` can't bound
it; `attended = true` implies it (D116).
