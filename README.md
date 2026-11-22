# loopherd

<img width="666" height="374" alt="Adobe Express - Adobe Express - loopherd-splash" src="https://github.com/user-attachments/assets/bc27723b-1ee9-4b1f-afea-e8bc408d9022" />

Herd your coding agents. loopherd is a Rust CLI and TUI for Linux and macOS
that groups coding agents into **herds** and keeps them working together
across sessions and restarts. It layers on [herdr](https://herdr.dev/), which
runs the terminals and knows whether each agent is working, blocked, or done.

- **Templates**: declare once how to spawn a member (repo, worktree, prompt),
  then `loopherd add <herd> <template> <params…>`.
- **Queues**: `feed` fills a herd's free slots from a task source, such as
  a repo's `ready-for-agent` issues, and retires members whose work is done.
- **Leads**: a lead member runs a herd's loop unattended, within switches
  you set.
- **Mailbox**: members report to a mailbox, and `watch` wakes you, or
  notifies you, when one needs you.
- **Revival and handoff**: a ledger of each member's task and Claude
  session, so a reboot or closed tab loses nothing, and a member near its
  context limit hands off to a fresh session.
- **One view** of every member across repos, sorted by what needs you.

## Install

Pick one. Both need [herdr](https://herdr.dev/) and
[Claude Code](https://code.claude.com/docs/en/overview) (`claude`); on
Linux and WSL2, leads also need [bubblewrap and
socat](https://code.claude.com/docs/en/sandboxing#set-up-linux-and-wsl2)
for the sandbox they run in. Both install
`~/.local/bin/loopherd` (put `~/.local/bin` on `PATH`), add loopherd's
Claude hooks and skill, run `loopherd setup`, and finish with `loopherd
doctor`, whose failures (say, herdr not started yet) are printed but don't
fail the install.

<table>
<tr><th>From junderdo.dev</th><th>From the repo</th></tr>
<tr valign="top"><td>

The latest release, with no clone and no Rust. Also needs `curl` (or
`wget`), `tar`, and `sha256sum` or `shasum`.

```bash
curl -fsSL https://junderdo.dev/loopherd/install.sh | sh
```

Picks the build for your OS and arch (Linux or macOS, x86_64 or aarch64)
and checks it against the release's `SHA256SUMS`. To pin a release:

```bash
curl -fsSL https://junderdo.dev/loopherd/install.sh | LOOPHERD_VERSION=v0.1.0 sh
```

The same script is served as
`https://github.com/junderdo/loopherd/releases/latest/download/install.sh`.

**Update:** run the one-liner again.

</td><td>

Builds from source: for contributors, or for the latest `main`. Also needs
`git`, a [Rust toolchain](https://rustup.rs/) and
[`jq`](https://jqlang.github.io/jq/download/).

```bash
git clone https://github.com/junderdo/loopherd.git
cd loopherd
scripts/install
```

On a terminal it also offers loopherd's agent types (`builder`,
`reviewer`, `researcher`, `scout`), the `mattpocock-skills` plugin and
loopherd's herdr plugin (below), each default no. `--with-agents`,
`--with-skills` and `--with-herdr-plugin` install one without asking;
`scripts/install --help` lists its flags.

**Update:** `git pull && scripts/install`.

</td></tr>
</table>

`loopherd setup` asks for `config.toml`'s machine-wide settings while
they're unset: what a lead may do by default, then the context window and
handoff limits. Run it again with `--again`.

To uninstall:

```bash
loopherd install-hook --remove      # add --settings <file> if you installed with one
rm ~/.local/bin/loopherd
```

Agent types from `scripts/install` are yours to remove: the files listed in
`$XDG_STATE_HOME/loopherd/agents.sha256`, then that file.

The hooks act only in a member's pane: they record each stop and each
subagent it starts or finishes in its mailbox, keep its session current
across `/clear` and resume, and deny a lead the commands its switches leave
to you. `install-hook` backs up
`settings.json` before changing it.

### herdr plugin

Optional, once loopherd is installed (above). Needs herdr 0.7.5 or later.
A source install asks "Install loopherd's herdr plugin?" when herdr is on
`PATH` and the plugin is missing or older than the checkout's, and
`scripts/install --with-herdr-plugin` installs or updates it without
asking. Or by hand:

```bash
herdr plugin install junderdo/loopherd/herdr-plugin
```

It adds a `loopherd: herd status` action and a `loopherd: open herd view`
action, which opens `loopherd view` in a popup over most of the screen;
`q` or Escape quits the view and closes the popup.

Once the plugin is installed, a source install asks "Add herdr key
bindings for loopherd's plugin?", and `scripts/install --with-herdr-keys`
adds them without asking: `prefix+m` opens the view and `prefix+shift+m`
opens it beside the shepherd. Both are unbound in herdr 0.9.5's defaults
(`herdr --default-config`). It writes them as `[[keys.command]]` entries
at the end of the `config.toml` herdr reads, after backing it up beside
itself, and reloads a running server. A key your config or herdr's
defaults already hold, or an action you already bound, is skipped with a
line saying why. herdr's keybinds help (`prefix+?`) lists them under
"custom" by their descriptions. `loopherd install-herdr-keys --remove`
takes out only those entries, matched by command and description, so a
binding you wrote by hand from the snippet below, or an earlier
README's, stays. `status` gets no key: its output goes to
herdr's plugin log, not the screen.

Or bind it by hand and run `herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "loopherd.view"
description = "herd view"
```

A popup that closes as soon as it
opens means the view couldn't start; run `loopherd view` in a pane to see
why.

`loopherd: open view beside shepherd` (`loopherd.view-beside-shepherd`)
splits the view into a pane right of the shepherd's, the one its last
handoff recorded, leaving focus where it was. With no shepherd recorded,
or its pane gone or no longer the shepherd's, it splits right of the
focused pane (the popup when that fails too), and the view says why. Bind
it the same way, with its own key (install uses `prefix+shift+m`).

The plugin's event hooks record a member going blocked, or its pane
exiting or closing, or its tab or workspace closing, so the next
`loopherd watch` reports it even when none was running. They follow a
member's pane moved to another tab or workspace.

A Ctrl-click on a GitHub PR link in any pane shows the member that owns
the PR in a popup (herd, live status, template, review round, last
reports; q or Esc closes it). A PR no open member owns opens in the
browser. Clicks reach the plugin only while herdr's `ui.mouse_capture`
is on; with it off, the terminal handles them.

The plugin runs the `loopherd` on `PATH`, else
`~/.local/bin/loopherd`, and never installs loopherd or its hooks;
without one, its commands fail with a pointer here. herdr shows no
action output on screen: read it, errors included, with
`herdr plugin log list --plugin loopherd`.

To have herdr run `loopherd revive` by itself after it restores a session,
add this to `config.toml` in loopherd's config dir:

```toml
[herdr_plugin]
revive_on_startup = true
```

It revives only the members recorded on the herdr server that started,
and is off without that key. herdr shows nothing of its run; it leaves a
`note` in each lead's mailbox instead, which the shepherd's watch shows.
Remove the plugin with `herdr plugin uninstall loopherd`.

## Set up a repo

Start with loopherd installed (above), herdr running, and the repo cloned
as its main checkout. Then, in order:

1. **Trust the repo in Claude Code.** Members can't start past Claude's
   trust dialog, so accept it once from the main checkout: `cd <repo> &&
   claude`, accept, exit. Every worktree shares it
   ([details](docs/research/claude-workspace-trust.md)).
2. **Write the herd.** In the repo, run `loopherd init`. It asks on a
   terminal and takes flags otherwise (`loopherd init --help`). Follow the
   steps it prints up to `loopherd herds`, filling each `(fill in)`
   placeholder it lists, then review and commit the repo files it added.
   Its last steps, the checks and `loopherd lead`, come below.
3. **Check it.** `loopherd herds` calls the herd file valid, and
   `loopherd feed <herd> --dry-run` lists the tasks you expect.
4. **Open the herd view.** In a new herdr tab, run `loopherd view`. It
   lists every member, sorted by what needs you; Enter focuses the
   selected member's pane, `S` focuses the shepherd (or, with none, starts
   one in a pane split below the view), and `t` switches between its
   purple-orange, phosphor and catppuccin themes, remembering the choice as
   `view.theme` in `config.toml`.
5. **Start the shepherd.** Press `S` in the view, run in a pane rather
   than the plugin's popup, to start it below the view, in the view's
   directory (a plugin pane's workspace directory, not the plugin's). Or in
   another herdr tab, `cd` to the main checkout, run `claude`, and give it
   this first prompt; the skill has it name its herdr agent `shepherd`, so
   `S` then finds it:

   ```text
   You're the shepherd for the <herd> herd in this repo. Read `loopherd --skill`
   and follow its Shepherd section: feed the herd, watch it, verify members'
   work, and clean up after merges.
   ```

6. **Start the lead,** if the herd file has a `[lead]` table:
   `loopherd lead <herd>`. Run it yourself, or ask the shepherd to. The
   lead, `lead-<herd>`, then feeds and watches the herd's workers within
   the switches you set; the shepherd's watch reads the lead and relays
   its questions to you. The lead runs in the Claude Code sandbox, which
   limits its writes to its own worktree and loopherd's state dir, so it
   doesn't change herd files, `config.toml` or `~/.claude` by mistake. It
   is a guard rail, not a cage ([D89](docs/decisions/D89.md)). It gets
   no Unix sockets, blocked by Seatbelt on macOS and by Claude Code's
   seccomp filter on Linux and WSL2, where present, so herdr, your ssh agent and WSL2's Windows interop are closed to it, unless its `[lead]` sets `unix_sockets = "all"`, which
   reopens all three ([D105](docs/decisions/D105.md)); a lead with
   `merge_guard = "credentials"` is refused it
   ([D114](docs/decisions/D114.md)). Where, on Linux or WSL2, Claude
   Code has no seccomp filter to block them, `loopherd doctor` and
   `loopherd setup` name `npm install -g @anthropic-ai/sandbox-runtime`,
   and the lead isn't started or restarted unless its `[lead]` sets
   `sandbox = "prefer"`; one with `merge_guard = "credentials"` isn't
   started or restarted even then. Under `unix_sockets = "all"` the socket
   check is skipped: the lead isn't refused or flagged for open sockets.
   What still gets past it: workers run unsandboxed, so a lead can have
   one act for it (LH-284), and most of the state dir stays writable to it
   (LH-298).
   Its `tell`, `read`, `status`, `feed`, `add`, `prompt`, `restart`,
   `retire` and `release` run in its broker, a process `loopherd lead`
   starts outside the sandbox, which acts only on the lead's own herd
   ([D98](docs/decisions/D98.md)). With the broker down they fail, naming
   `loopherd revive --name <lead>`, which starts it again.
   A lead whose sandbox can't start, or whose sandbox your own
   `~/.claude/settings.json` loosens (an `excludedCommands` or `allowWrite`
   entry, say), isn't started unless its `[lead]` sets `sandbox = "prefer"`,
   which starts it with a warning instead ([D100](docs/decisions/D100.md)).
   A `merge_guard = "credentials"` lead isn't started either way; set
   `merge_guard = "hook"` to start it with the warnings.
   A lead's Claude args (`[lead].args`, else the herd's `args`, then the
   command line) may be only `--model`, `--effort`, `--verbose` and
   `--permission-mode` with `default`, `manual`, `auto` or `plan`; any
   other is refused or warned of the same way. Under `require`, a lead
   started with an arg now off that list can't be restarted or revived
   until you retire it and run `loopherd lead` again.
   A lead starts with no MCP servers; list the ones it may use, by the
   name `claude mcp add --scope user` gave them, in `[lead] mcp_servers`.
   Those run outside the sandbox.

The guide a shepherd or lead follows, from setting up a herd to cleaning up
after merges, is the loopherd skill: `loopherd --skill` prints it
(`--section lead` prints one section), and `install-hook` installs it for
Claude: the file in `~/.agents/skills/loopherd/`, and `~/.claude/skills/loopherd`
a relative symlink to it, as the `skills` CLI lays out a skill.

### What `init` writes

`loopherd init` infers the repo, herd name, GitHub repo and account,
default branch, and checks from `Cargo.toml` and `package.json`, and on a
terminal asks for each with the inferred value as the default. For a local
tracker on a GitHub origin, the GitHub repo question defaults to none (no gh
calls, no PR `merged` check); `--no-github` makes that choice off a terminal.
It writes
`<herd>.toml` and `<herd>-lead.md` in `$XDG_CONFIG_HOME/loopherd/herds/`
(default `~/.config/loopherd/herds/`; `LOOPHERD_CONFIG_DIR` replaces the
`loopherd` dir), creates the labels the queue reads for a GitHub tracker,
and, uncommitted, adds `docs/agents/loopherd.md`, a tracker doc if the repo
has none, and a `## loopherd` section in `AGENTS.md`. The tracker can be
GitHub issues, local markdown issue files, or another one you describe.

Its printed steps also cover adding an origin remote and writing by hand a
repo file init left alone (a symlinked `AGENTS.md`, a doc that would land
outside the repo), each when it applies, and always tailoring the herd to
the repo per "Setting up a project" in `loopherd --skill`.

### If a spawn blocks on the trust dialog

A spawn that still meets the dialog exits 3 and names the pane. Answer the
dialog there, then run `loopherd prompt <name>` to send the member its
prompt.

### Writing a herd file by hand

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
[docs/examples/](docs/examples/).

A template with `attended = true` starts a member the human sits in, such
as a grill tied to a card (D112). Only `add` starts one: it focuses the new
pane (`--no-focus` skips that), records the caller's pane and fills a
`{shepherd_pane}` placeholder with it, and `retire --close` focuses that pane
again if the human is still in the member's (`herdr agent focus` takes only
an agent's pane, so run `add` from one). It holds no `max` slot, is never
asked to hand off, and is left out of `stats`. A watch wakes on its `gone`
alone, and a lead's watch also on its `merged`.

A template with `slot = false` (default `true`) starts members that hold no
`max` slot: `feed` and `add` don't count them, `add` never warns past `max`
for one, and the watch doesn't count one as an open worker for `no workers
open`, `out of work`, `drained` or `all drained`. Its own `done`, stop and
`gone` events still print. Only `add` starts one, as `max` can't bound
it; `attended = true` implies it (D116).

## Tour

`loopherd --help` and `loopherd <command> --help` give every flag.

### Set up

| Command | What it does |
|---|---|
| `setup` | Ask or set `config.toml`'s machine-wide settings (`--lead-defaults`, `--context`, `--again`). |
| `init` | Write a herd for the current repo; `--diff` shows what it would write now. |
| `herds` | List herd files and their templates; exits 1 if any is invalid. |
| `herd get\|set\|unset <herd> <key>` | Read or edit one value in a herd file, keeping its comments (`queue.max`, `lead.merge`). |
| `herd remove <herd>` | Delete a finished herd's file and every run of it in the ledger; a name holding another herd's run keeps that run and its files. |
| `lead-defaults` | Show `config.toml`'s lead switches, on or off, for herds whose `[lead]` doesn't set them; `--set` writes them. |
| `config get\|set\|unset <key>` | Read or edit `context_window`, `default_context_limit`, `shepherd.context_limit`, `shepherd.self_restart`, `lead_defaults.review_rounds` or `view.theme`. |
| `install-hook` | Add loopherd's Claude hooks and skill (`--remove` takes them out). |

### Run

| Command | What it does |
|---|---|
| `lead <herd>` | Start the herd's lead, `lead-<herd>`, in a detached worktree. |
| `feed <herd>` | Retire members whose `done` check passes, then fill free slots from the queue's source (`--dry-run`). |
| `add <herd> <template> <params…>` | Spawn one member from a template (`--dry-run`; `--no-focus` for an attended one). |
| `spawn --name <name>` | Spawn a member without a herd file (`--repo`, `--worktree`, `--prompt`). |
| `pause <herd>` / `resume <herd>` | Stop `feed` spawning for a herd (`--reason`, `--all`), and lift it. |
| `report "<text>"` | From a member's pane, leave a report for whoever watches it (`--kind done\|question\|note`). |

### Watch

| Command | What it does |
|---|---|
| `watch [<herd>…]` | Block until a member needs you, print what happened, exit (`--timeout`). |
| `reports` | Print unread reports, questions first, and mark them read (`--herd`, `--all`). |
| `status` | Members by herd with herdr's live status (`--all`, `--strays`, `--json`). |
| `read <member>` | A member's last lines of output, unwrapped (`--lines`, 40 by default, up to 1000). |
| `view` | The same as a live TUI; Enter focuses the member's pane, `S` focuses the shepherd or starts one below the view, and `t` switches the theme, saving it as `view.theme` in `config.toml`. |
| `pr [url]` | The herdr plugin's PR link handler: the member that owns the PR in a popup, else the browser. |
| `stats` | How each herd's work flowed: time to done, review rounds, time waiting on merge, and each run's outcome (`merged`, `closed`, `released`, `retired`), with medians over merged runs per herd and per template (`--herd`, `--since`, `--until`, `--json`). |

`watch` can also notify you outside your pane on a question or a blocked
member: set `[notify] enabled = true` in `config.toml`. The skill has the
options.

### Recover

| Command | What it does |
|---|---|
| `revive` | Resume members after a herdr restart or a closed tab, by Claude session (`--dry-run`, `--name`, `--startup` for the herdr plugin's startup hook). |
| `restart <name>` | Give a member that wrote its handoff a fresh session in the same pane. |
| `prompt <name>` | Send a member's stored prompt again, e.g. after a trust dialog blocked its spawn. |
| `retire <name>` | Mark a member finished; `--close` also closes its tab and removes its merged worktree. |

A member past its context limit (`context_limit` in its template or
`[lead]`, else `config.toml`'s `default_context_limit`; tokens, or a
percentage of `context_window` like `"65%"`) is asked once to
write a handoff and report `--kind handoff`; `restart` then resumes it from
that file.

### Hooks

`stop-hook`, `session-hook`, `tool-hook`, `prompt-hook` and `subagent-hook`
are the Claude hooks `install-hook` sets up. Claude runs them; you don't.

## Where it's going

Direction, decisions and open questions are in
[docs/wayfinding.md](docs/wayfinding.md); what herdr offers is in
[docs/research/herdr.md](docs/research/herdr.md). Releases are cut as in
[docs/releasing.md](docs/releasing.md).

## How it's built

loopherd is also a loop-engineering experiment: much of it is written by
agents pulling cards from the
private [Trello board](https://trello.com/b/MtiTIrp1/loopherd), with tests and lints
as the back-pressure. Agent instructions start at [AGENTS.md](AGENTS.md);
the card workflow is
[docs/agents/issue-tracker.md](docs/agents/issue-tracker.md).

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
LOOPHERD_LIVE=1 scripts/herdr-dev cargo test --test live   # against a dev herdr session
scripts/trello help                                        # needs TRELLO_API_KEY / TRELLO_TOKEN
```

## License

GPL-3.0. See [LICENSE](LICENSE).
