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
- `agent rename <pane-id> <name>` takes a pane ID as its target and
  names a Claude typed by hand there (checked 2026-11-19, herdr 0.9.5).
  Renaming to the name it already has succeeds; renaming another agent to
  a held name fails with `agent_name_taken`, naming the holder's pane.
- `agent prompt <pane-id> <text>` takes a pane ID as its target too, so
  an agent is prompted with or without a name (checked 2026-11-19, herdr
  0.9.5, by the shepherd self-restart live test).

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
- **Popups** (LH-226). Plugin panes with `placement = "popup"` came in
  0.7.4. A `herdr server` started with a temp `HOME`, `XDG_CONFIG_HOME`,
  `XDG_STATE_HOME` and `HERDR_SOCKET_PATH` keeps its own registry, so a plugin linked through it
  never reaches the human's. Opening a pane needs a workspace (else
  `no active workspace`), but no attached client. The popup's command gets
  `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH` and `HERDR_PLUGIN_ENTRYPOINT_ID`,
  not `HERDR_PANE_ID`. It closes when the command exits, and the socket's
  `popup.close` ends the command; then `popup.close` answers
  `popup_not_open`. No API sends input to a popup, and keys typed into a
  client attached under `script` didn't reach it.

- **Split plugin panes** (LH-286, 0.9.5, probed on an isolated server).
  `plugin pane open --placement split --target-pane <p> --direction right
  --no-focus` overrides a manifest `popup` pane, leaves focus on the
  target, and answers with the new pane in `result.plugin_pane.pane`; an
  unknown target is `pane_not_found`. It opens at ratio 0.5 and takes no
  ratio. `pane resize --pane <p> --direction <d> --amount <f>` moves the
  divider nearest `p` on its `d` edge (else its opposite edge), taking
  `|f|` capped at 0.5 and clamping ratios to 0.1–0.9. On the target, its
  right edge is the new divider; the new pane's right edge, inside an
  existing split, is the outer one. The pane closes when its command
  exits, as a popup does. An action invoked from the CLI gets
  `HERDR_PANE_ID` and `focused_pane_id` in its context: the focused pane.

## Handoff or restart in a startup hook (0.9.5, LH-263)

Checked 2026-11-18 against herdr 0.9.5, the latest release, with its source
at tag `v0.9.5` (read only) and probes on the `loopherd-dev` server through
`scripts/herdr-dev`. No plugin was linked, so no real hook ran (D62); what a
hook receives comes from the docs and source. Unreleased `master`
(`ee5bfb50`) calls the hook the same way, and the changelog has no entry
about telling the cases apart.

herdr runs `[[startup]]` hooks after a restart and again when a new server
takes over a live handoff ([plugins docs](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/plugins.mdx),
"Startup hooks"). Both paths call one argument-less
`run_plugin_startup_hooks()` (`src/server/headless/bootstrap.rs`, normal
start and `run_handoff_import_server`).

| Candidate | Source or probe | Signal? |
|---|---|---|
| Env vars, argv, `HERDR_PLUGIN_CONTEXT_JSON` | `src/app/api/plugins/runtime.rs` builds them with no reason; the context has `invocation_source = "startup"` and `correlation_id = "plugin.startup"` on both paths, and the struct (`src/api/schema/plugins.rs`) has no restored, handoff or start-time field | No |
| herdr event, CLI or socket field | `status server --json`, `ping`, `PaneInfo` and the event kinds have no start time, generation or handoff flag. `capabilities.live_handoff` only says the server can hand off. `detached_server_daemon` flipped false, true, false across a handoff and a restart, but only means the server is a session leader | No |
| Session or pane field that changes only on restart | `terminal_id` changes on both. `pane_id`, `workspace_id` and `tab_id` are stable on both. `session.json` was identical across a handoff. `revision` went 1, 0, 1, mechanism unread | No |
| Pane shell's parent pid is the server | Probe below | **Yes** |
| Pane shell older than the server | Probe below | **Yes** |
| `--handoff-import` in the server's argv | Internal flag, below | Cross-check |

**Shell parent pid.** The simplest test needs no clocks. herdr spawns
every shell it starts itself, so after a restart a pane's shell has the
server as its parent. A handoff's new server is spawned by the old one and is
not the panes' parent, so once the old server exits the surviving shells are
orphaned and reparented (to pid 1 here). Probe on the dev server, `shell_pid` from
`herdr pane process-info --pane <id>`, `ps -o ppid= -p <shell_pid>`:

| moment | server pid | shell pid | shell's ppid |
|---|---|---|---|
| baseline | 1980449 | 1981151 | 1980449 |
| after a live handoff | new, argv has `--handoff-import` | 1981151 (unchanged) | 1 |
| after `server stop` and a restart | 1985304 | 1985384 (new) | 1985304 |

**Process start times.** The same probe, with `/proc/<pid>/stat` field 22
(clock ticks since boot, in `sysconf(_SC_CLK_TCK)` units, 100/s here; a
comparison needs no conversion). An earlier run:

| moment | server start | pane shell start |
|---|---|---|
| after a live handoff | 1436203 | 1433130 (same pid as before, `sleep` child alive) |
| after `server stop` and a restart | 1440157 | 1440158 (new shell, `sleep` gone) |

A handoff leaves the shell about 30 s older than the new server. After a
restart a plain pane's shell is spawned inside `App::try_new`, before the
hook runs, so it starts at or after the server; this held with no client
attached. A pane with a resumable agent session is different: in
`src/persist/restore.rs` `restore_tab` it gets a pending resume plan and no
runtime, and its shell starts later from `src/app/agent_resume.rs`
`start_pending_agent_resume`. So at hook time such a pane may have no
`shell_pid` yet. It counts toward neither case.

The rule: a pane shell that started before the server, or whose parent is
not the server, means a handoff; a shell that started at or after the
server, or whose parent is the server, means a restart. A pane with no
`shell_pid` says nothing; with no pane left to decide, treat it as a restart.
Do not read `restore_error`: `restore_handoff` goes through the same
`restore_tab`, and a pane with no runtime on the old server (one already
showing an error, or pending agent resume) gets `restore_error` after a
handoff too if its directory is gone. A tie in ticks counts as a restart.
Use the shell, not the agent: herdr resumes agents only after a client
attaches, and a user may start one later in a surviving pane.

Parsing: `/proc/<pid>/stat` has `comm` in parentheses, which can hold spaces
and parens, so split after the last `)`. macOS has no `/proc`; `ps -o
lstart=` is wall-clock time to the second, locale-dependent and open to
clock steps (a pane under 1 s old at handoff reads as a restart), so prefer
the ppid test, or `sysctl` `KERN_PROC_PID` `p_starttime` in microseconds.

**`--handoff-import`.** A handoff-imported server keeps
`--handoff-import <socket> <token>` in its `/proc/<pid>/cmdline` for its
whole life; a restarted one does not. It is an internal flag, not
documented, so use it only as a cross-check.

**Not settled.** The hook's `$PPID` being the server (a plain
`Command::spawn` from a server thread) is read from the source and needs
checking in the first real hook run. macOS (the ppid test is its choice, via `ps -o ppid=`, which
was not probed; there is no `/proc`), a client-autostarted server and a failed handoff re-running hooks
were not tried; handoff is unsupported on Windows, where a startup hook is
always a restart.

**Verdict.** herdr passes the hook no documented handoff flag, and no status, event
or field carries one; an internal argv marker (`--handoff-import`) exists. So
LH-246 derives it: for each pane with a `shell_pid`, the hook checks that
shell's parent pid and start time against the server's, and `revive
--startup` skips the re-arm prompt when the shells survived.
LH-246 built it (D81) against herdr 0.9.5.

## Bringing up the shepherd from the view (0.9.5, LH-287)

Checked 2026-11-19 against herdr 0.9.5 on the `loopherd-dev` server through
`scripts/herdr-dev`; the live session was only read.

- **The view's own pane.** A plain pane's process gets `HERDR_PANE_ID`
  (read from a running view's environment). A plugin pane opened with
  `plugin pane open --placement split` gets it too, naming its own new
  pane, beside `HERDR_PLUGIN_*` and `HERDR_PLUGIN_CONTEXT_JSON`; only a
  popup goes without (see Plugins above). So the view needs no `pane list`
  lookup: `HERDR_PANE_ID` is its pane, and its absence means a popup. A
  plugin pane's cwd is the plugin's root; the context JSON's
  `workspace_cwd` is the workspace's.
- **Focusing by ID.** `agent focus <pane-id>` takes a pane ID as its target
  and brings that pane's agent to the front, as the view's Enter already
  does. `pane focus` only moves to a neighbor by direction.
- **Splitting below.** `pane split <pane> --direction down --cwd <dir>
  --no-focus` answers the new pane as `.result.pane.pane_id`. Its shell
  gets the server's environment plus each `--env K=V`, not the caller's: a
  variable set on the calling `herdr` was unset in the new pane, and one
  passed with `--env` was set.
- **Starting the shepherd.** `agent start shepherd --kind claude --pane
  <new> -- <prompt>` once the new shell is at its prompt. The prompt resumes
  from `handoffs/shepherd.md` when it was written after `shepherd.json`'s
  time, or when there's no record; otherwise it is the plain shepherd start.
  The live test shows the stacked layout in `pane layout`: the shepherd's
  pane has the view's `x` and a larger `y`.

## Oldest supported herdr (LH-247)

Still unknown. `loopherd doctor` fails below `MIN_HERDR` in
`src/doctor.rs`, which is 0.9.5 because that is the only herdr loopherd
has been run against: its live tests, pane labelling (`pane
report-metadata`) and the plugin notes above were all checked on 0.9.5.
Lowering it needs each feature loopherd uses dated in herdr's changelog
and tried on that release: `agent start`/`wait`/`list` with
`launch_pending`, `display_agent` and `terminal_title_stripped`, the
socket event subscriptions, `pane report-metadata`, and `status server
--json`'s `running` and `version`, which `doctor` reads.

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
