# A tour of loopherd's commands

`loopherd --help` and `loopherd <command> --help` give every flag. The
[README](../README.md) has the pitch and a first herd; [setup.md](setup.md)
walks a repo's setup.

## Set up

| Command | What it does |
|---|---|
| `setup` | Ask or set `config.toml`'s machine-wide settings (`--lead-defaults`, `--context`, `--revive-on-startup`, `--again`). |
| `init` | Write a herd for the current repo; `--diff` shows what it would write now. |
| `herds` | List herd files and their templates; exits 1 if any is invalid. |
| `herd get\|set\|unset <herd> <key>` | Read or edit one value in a herd file, keeping its comments (`queue.max`, `lead.merge`). |
| `herd remove <herd>` | Delete a finished herd's file and every run of it in the ledger; a name holding another herd's run keeps that run and its files. |
| `lead-defaults` | Show `config.toml`'s lead switches, on or off, for herds whose `[lead]` doesn't set them; `--set` writes them. |
| `config get\|set\|unset <key>` | Read or edit `context_window`, `default_context_limit`, `shepherd.context_limit`, `shepherd.self_restart`, `herdr_plugin.revive_on_startup`, `lead_defaults.review_rounds`, `view.theme` or `view.layout`. Token counts take `250k` or `1.5M`; the two limits also a percentage of the window (`65%`). |
| `merge-guard <herd>` | Set up the split credentials of a herd whose lead has `merge_guard = "credentials"`: its deploy key and the rulesets, then how to make its token. |
| `install-hook` | Add loopherd's Claude hooks and skill, with these docs as its `references/` (`--remove` takes them out). |
| `docs [<topic>]` | Print one of these docs (`tour`, `setup`, `install`, `glossary`, `herd-file`, `runbook`); with no topic, list them. |
| `install-herdr-keys` | Bind `prefix+m` and `prefix+shift+m` to the herdr plugin's views (`--remove` takes them out). |
| `doctor [<herd>]` | Check this machine's loopherd setup and print the problems (`--verbose`, `--json`, `--trackers`). |
| `update` | Update loopherd the way it was installed (`--check` to only look, `--version vX.Y.Z` for a release install). |

## Run

| Command | What it does |
|---|---|
| `lead <herd>` | Start the herd's lead, `lead-<herd>`, in a detached worktree. |
| `feed <herd>` | Retire members whose `done` check passes, then fill free slots from the queue's source (`--dry-run`). |
| `add <herd> <template> <params…>` | Spawn one member from a template (`--dry-run`; `--no-focus` for an attended one). |
| `spawn --name <name>` | Spawn a member without a herd file (`--repo`, `--worktree`, `--prompt`). |
| `tell <name> "<text>"` | Message a live member, then confirm it went working (`--text-file`). |
| `pause <herd>` / `resume <herd>` | Stop `feed` spawning for a herd (`--reason`, `--all`), and lift it. |
| `gate <sha>` | From a lead, run the herd file's `[lead] gate` commands on a pushed commit, in its broker outside the sandbox (`--base`). |
| `report "<text>"` | From a member's pane, leave a report for whoever watches it (`--kind done\|question\|note`). |

## Watch

| Command | What it does |
|---|---|
| `watch [<herd>…]` | Block until a member needs you, print what happened, exit (`--timeout`). |
| `reports` | Print unread reports, questions first, and mark them read (`--herd`, `--all`). |
| `status` | Members by herd with herdr's live status (`--all`, `--strays`, `--json`). |
| `read <member>` | A member's last lines of output, unwrapped (`--lines`, 40 by default, up to 1000). |
| `view` | The same as a live TUI, with each member's running subagents; Enter focuses the member's pane, `s` focuses the shepherd or starts one beside the view (left of it, or below with `view.layout = "stacked"`), `v` switches the two between side by side and stacked, saving it as `view.layout`, `t` switches the theme, saving it as `view.theme` in `config.toml`, and `x` shows or hides strays. |
| `pr [url]` | The herdr plugin's PR link handler: the member that owns the PR in a popup, else the browser. |
| `stats` | How each herd's work flowed: time to done, review rounds, time waiting on merge, and each run's outcome (`merged`, `closed`, `released`, `retired`), with medians over merged runs per herd and per template (`--herd`, `--since`, `--until`, `--json`). |

`watch` can also notify you outside your pane on a question or a blocked
member: set `[notify] enabled = true` in `config.toml`. The skill has the
options. A watch also says `<member> stalled` when a member has sat idle
after a paused stop for its herd's `stalled_after` (30 minutes by default).

## Recover

| Command | What it does |
|---|---|
| `revive` | Resume members after a herdr restart or a closed tab, by Claude session (`--dry-run`, `--name`, `--startup` for the herdr plugin's startup hook). |
| `restart <name>` | Give a member that wrote its handoff a fresh session in the same pane. |
| `prompt <name>` | Send a member's stored prompt again, e.g. after a trust dialog blocked its spawn. |
| `retire <name>` | Mark a member finished; `--close` also closes its tab and removes its merged worktree. |
| `release <name>` | Let `feed` spawn a retired member's name again, for a task it left unfinished (`--keep-branch`, `--drop-branch`). |
| `broker-restart <lead>` | Put a lead's broker on this loopherd binary, as after a merge that changes the broker. |

A member past its context limit (`context_limit` in its template or
`[lead]`, else `config.toml`'s `default_context_limit`; tokens, or a
percentage of `context_window` like `"65%"`) is asked once to write a
handoff (`loopherd handoff`) and report `--kind handoff`; `restart` then
resumes it from that file. The context window comes from the session's
model unless `context_window` overrides it.

## Hooks

`stop-hook`, `session-hook`, `tool-hook`, `prompt-hook` and `subagent-hook`
are the Claude hooks `install-hook` sets up, and `herdr-event-hook` is the
herdr plugin's. Claude and herdr run them; you don't.
