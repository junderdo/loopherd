# herdr — research notes

Researched 2026-10-31 against herdr 0.9.5 (installed at `~/.local/bin/herdr`).

## What it is

Agent-aware terminal multiplexer, Rust + Ratatui, AGPL-3.0. A background
server owns the PTYs; TUI clients attach/detach. Topology is
workspace → tab → pane, with opaque stable IDs (`w1`, `w1:t1`, `w1:p1`).
A closed ID is never reused within one server, but it can be after a restart
(below).

### ID reuse across a server restart

Checked 2026-11-17 against herdr 0.9.5 in a throwaway session
(`herdr --session <name> server`, then `herdr server stop` and start again):

- A restart restores every open workspace, tab and pane under its old ID,
  with its label.
- Workspace IDs are reused: the next one is one past the highest restored.
  With `w4` restored, closing `w5` and restarting gave the next workspace
  `w5` again; closing every workspace and restarting gave `w1`. Without a
  restart, closing `w1` and creating one gave `w3`.
- Pane and tab numbers in a restored workspace are not reused: closing
  `w4:t3` and restarting gave the next tab `w4:t4`, pane `w4:p4`.
- So a pane ID like `w1:p1` recorded for a workspace closed before a
  restart can name a pane in a new, unrelated workspace afterwards. A new
  tab's default label is its number (`1`), not a name.

## Agent model

- Detects 24 agent kinds (claude, codex, gemini, …) by process name plus
  output heuristics; `herdr integration install <agent>` adds hooks for
  sharper state.
- Lifecycle states: `idle`, `working`, `blocked` (approval/question UI),
  `done`, `unknown`.
- Live agents can be named (`[a-z][a-z0-9_-]{0,31}`, unique per server).
- `agent list` exposes `agent_session.value` — for Claude this is the
  session ID usable with `claude --resume`.
- Panes get `HERDR_ENV=1`, `HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`,
  `HERDR_PANE_ID` injected.

### When an agent's name or session is missing

Checked 2026-11-17 against herdr 0.9.5 in a throwaway session, with herdr's
Claude integration hook installed:

- `agent start <name> --kind claude` returns with the name and the session
  (`source: herdr:claude`) both set.
- `agent rename <target> --clear` leaves `name: null`; the session stays.
- A Claude typed by hand in a pane shows `name: null` with its session set
  by the first `agent list`.
- A Claude that doesn't run the integration hook (here, one started with an
  empty `CLAUDE_CONFIG_DIR`) shows `name: null` and `agent_session: null`
  for as long as it runs.
- After `server stop` and a restart, resumed agents come back with
  `name: null` and their session set: names don't survive a restart.

## Control surface (everything we need to layer on top)

All commands return JSON over the socket API.

| Need | Command |
|---|---|
| Observe everything | `herdr api snapshot`, `agent list`, `workspace list`, `pane list` |
| Build layout | `workspace create`, `tab create`, `pane split --cwd … --no-focus` |
| Worktrees | `worktree create/open/list/remove` |
| Launch agent | `agent start <name> --kind claude --pane <id> -- <args>` (pane must be at a shell prompt; a new tab's shell is in the foreground while its rc files run, so wait for `pane process-info` to list only the shell *and* `pane read --source visible` to show the prompt, else `agent_pane_busy`) |
| Talk to agent | `agent prompt <name> "…" --wait --timeout <ms>`, `agent send-keys` |
| Read output | `agent read`, `pane` read commands |
| Synchronize | `agent wait <name> --until <states>` |
| Subscribe | socket `events.subscribe` (no CLI wrapper), see [herdr-events.md](herdr-events.md) |
| Remote | global `--machine <label>` prefix; IDs are per-server |
| Display metadata | `workspace report-metadata`, `pane report-metadata` (see below) |

Gotchas from the bundled skill (`herdr --skill`):
- `agent prompt` refuses a `blocked` agent (`agent_blocked`); `--wait` can
  return `agent_prompt_stalled` if no activity appears within 5 s.
- `idle`/`done` don't prove an unrecognized dialog isn't showing.
- A pane moved between workspaces gets a new ID — track agents by name.
- Remote connection failures don't prove a mutation wasn't applied.

## Labelling a Claude pane (0.9.5, LH-206)

Tried on `scripts/herdr-dev` against a Haiku Claude started with
`agent start`, with herdr's Claude integration installed.

**Lifecycle state is herdr's alone.** `pane report-agent --state blocked`
from any source other than the pane's owner exits 0 and changes nothing:
`agent_status` and `state_change_seq` stay put, with or without `--seq`,
for `--agent claude` or `loopherd`, and for every `--state`. Once
`herdr:claude` reports the session it owns the pane's lifecycle, and herdr
accepts a takeover only from an official `herdr:*` source. herdr closed the
request to change that as not planned
([#2824](https://github.com/herdrdev/herdr/issues/2824)); the opt-in idea is
[discussion #3625](https://github.com/herdrdev/herdr/discussions/3625).

**Display metadata is stored.** `pane report-metadata` is accepted on the
same pane and never touches `agent_status`, `name` or waits:

- The pane ID must come before the options, despite the usage line:
  `herdr pane report-metadata <pane> --source loopherd ...`. With the pane
  last, 0.9.5 fails with `unknown option: loopherd`.
- Metadata is kept per `--source`; each report replaces that source's
  presentation fields (`--title`, `--display-agent`, `--state-label`), and
  another source's `--clear-*` leaves them alone.
- `--agent claude` guards the fields to a pane whose agent is Claude.
- Text is trimmed, stripped of control characters and cut to 80 characters.
- `--ttl-ms` expires the fields; without it they stay until cleared or the
  pane closes.
- Success prints nothing (no JSON envelope); a missing pane fails with
  `pane_not_found`.

How each field shows, with herdr's default sidebar
(`ui.sidebar.agents.rows = [["state_icon", "machine", "workspace", "tab"], ["agent"]]`):

| Field | Sidebar | `agent list` / `pane get` |
|---|---|---|
| `--display-agent` | replaces the agent's name on its second row | `display_agent`; `name` unchanged |
| `--title` | not shown: no row token reads it | `title` |
| `--state-label idle=...` | only where a layout uses `state_text` | `state_labels` |
| `--token name=...` | only where a layout uses `$name` | `tokens` |

The state icon stays herdr's own throughout, so a label can't show the
`blocked` icon.

The right-hand column for `--display-agent` is asserted live by
`a_leads_question_labels_its_claude_pane_until_a_typed_prompt`, through
both `pane get` and `agent list`. The sidebar column comes from herdr's
sidebar docs and its default `rows`; no test reads the rendered sidebar.

**Clearing on the human's answer.** Claude Code fires `UserPromptSubmit` for
a finished background task too, not only for a typed prompt: its `prompt`
is the `<task-notification>...</task-notification>` block Claude hands the
model. A shepherd's `loopherd watch` runs as such a task, so a hook that
clears on any prompt would clear the label the moment the watch that set it
exits.

What the hook receives, captured on Claude Code 2.1.327 with a logging
`UserPromptSubmit` hook in a dev pane (pinned as
`tests/fixtures/prompt-hook/`):

- A finished background task's `prompt` is exactly one
  `<task-notification>...</task-notification>` block, with nothing before or
  after it. The input's keys are the same as a typed prompt's, so the text is
  the only way to tell them apart.
- A typed prompt queued while a background task finished mid-turn fired its
  own hook call, apart from the notification's. Claude didn't batch them.
- Transcripts also show notifications with text around the block: a
  `<system-reminder>` wrapper, or a harness nudge such as "The user hasn't
  heard from you in a while" after it. Those are `api-request-blob` entries,
  the request sent to the model. Each one matched to its task's queued `user`
  entry (6 of 6 in a lead's transcripts) held the bare block, so the extra
  text is added after the hook runs.

loopherd keeps the label only for a prompt whose trimmed text starts with
`<task-notification>` and ends with `</task-notification>`; anything else
clears it.

## Plugins (0.9.5, LH-223)

Checked 2026-11-18 against herdr 0.9.5, with herdr's
[plugin docs](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/plugins.mdx),
the [0.7.0 plugin docs](https://raw.githubusercontent.com/herdrdev/herdr/v0.7.0/website/src/content/docs/plugins.mdx)
for what 0.7.0 already had, and the changelog.

- A plugin is a directory with `herdr-plugin.toml`. `id`, `name`,
  `version` and `min_herdr_version` are required; herdr refuses to link or
  install one whose minimum is newer than itself. Actions, event hooks,
  panes, link handlers, top-level and per-item `platforms`, and `[[build]]`
  all date from 0.7.0. `[[startup]]` hooks came in 0.7.5 and `plugin update`
  in 0.9.4.
- Commands are argv arrays run without a shell, with the plugin directory
  as their working directory, and get `HERDR_BIN_PATH`,
  `HERDR_SOCKET_PATH`, `HERDR_PLUGIN_ROOT`, `HERDR_PLUGIN_CONFIG_DIR`,
  `HERDR_PLUGIN_STATE_DIR` and `HERDR_PLUGIN_CONTEXT_JSON`. They inherit the
  server's environment, so its `PATH`, not the user's shell's.
- herdr pipes an action's stdout and stderr into its plugin command log
  (`herdr plugin log list --plugin <id>`) and shows nothing on screen.
- `plugin install owner/repo/subdir` clones, runs `[[build]]` and registers.
  `plugin link <dir>` registers a local directory and runs no build.
- Since 0.7.5 plugins are **global to the user**, not per session: the
  registry is `$XDG_CONFIG_HOME/herdr/plugins.json` (else
  `~/.config/herdr/`), with config dirs under `herdr/plugins/config/<id>`
  there and state under `$XDG_STATE_HOME/herdr/plugins/<id>`. Even
  `plugin config-dir <id>` creates both dirs.
- **Isolation.** With no server reachable, `HOME` or `XDG_CONFIG_HOME`
  moves the whole registry: `plugin link` and `plugin list` work against a
  temp home and the real list stays empty. But when `HERDR_SOCKET_PATH`
  reaches a running server, the CLI sends `link` to that server, which
  writes the server's own registry. Linking through `scripts/herdr-dev`
  with a temp `HOME` registered the plugin in the human's
  `~/.config/herdr/plugins.json`, because the dev server runs as the human.
  So an isolated link also points `HERDR_SOCKET_PATH` at a socket nothing
  listens on.

## Known gaps (our opportunity)

- After reboot / server stop, panes return as shells; **agents are not
  restarted and work is not resumed**.
- No project templates (tmuxinator-style) or resurrect equivalent.
- Grouping is only by workspace (≈ repo); nothing cross-cutting.
- Coordination exists as primitives (prompt/wait/read), no task model.
- Reported render lag with 10+ panes; pre-1.0 API churn; single maintainer.

## License note

AGPL-3.0: use it as an external tool via its CLI/socket API. Do not copy or
vendor its source.

## Sources

- https://herdr.dev/
- https://www.bitdoze.com/herdr-agent-multiplexer/
- https://betterstack.com/community/guides/ai/herdr-ai-agent/
- `herdr --help`, `herdr --skill`, `herdr agent start --help` (0.9.5)
- herdr 0.9.5 docs: CLI reference (panes), socket API (`pane.report_metadata`),
  configuration (agent sidebar rows), https://herdr.dev/llms.txt
