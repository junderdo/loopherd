# Telling "finished" from "paused on a background shell"

Researched 2026-11-02 against herdr 0.9.5 (claude detection manifest
2026.10.11.1, claude integration v11) and Claude Code 2.1.316. Answers card
LH-62.

## Answer

**Use Claude Code's `Stop` hook.** Its input carries a `background_tasks`
array, documented as the way to "distinguish 'session is done' from 'session
is paused waiting for background work to wake it back up'". A
loopherd-installed Stop hook can record that array on every stop. Then `watch`
can show `lh-60 done  (1 shell running)` instead of the bare `done`, or hold
the wake back.

herdr shows nothing of this today. `pane process-info` can tell in principle,
but only through Claude Code's undocumented process layout. The "no
`done`/`question` report" fallback is still worth showing, because it also
catches agents that stop without reporting for other reasons.

## Live observation

Run in the `loopherd-dev` session. A claude agent `probe` was started with
`--settings` adding a Stop hook (`cat >> stop.jsonl`) and allow rule
`Bash(sleep:*)`, then prompted to run `sleep 240` with `run_in_background`
and end its turn.

| Time | herdr `agent_status` | Stop hook `background_tasks` |
|---|---|---|
| 08:05:4x | `done` | `[{"id":"brz1vs8ol","type":"shell","status":"running","description":"Sleep for 240 seconds","command":"sleep 240"}]` |
| 08:09:51 | `working` (the shell exited and Claude resumed itself) | — |
| 08:09:52 | `done` | `[]` (`last_assistant_message`: "The background `sleep 240` finished with exit code 0.") |

At the first stop, the screen footer read
`✻ Crunched for 5s · done 8:05 AM · 1 shell still running` and the status bar
read `1 shell`. `herdr agent explain probe` gave `state: idle`,
`rule: live_prompt_box`. The resume showed as `working` for about one second,
so a poll-based `watch` can miss it entirely.

## Candidate signals

### 1. `herdr pane process-info`: no, not on its own

It lists only the pane's **foreground process group** (`pane.process_info`
"returns the pane's shell pid, foreground process group id when available,
and foreground processes", [socket-api.mdx][api]). Observed with the
background `sleep` running:

```json
{"foreground_process_group_id":15641,"foreground_processes":[{"name":"claude","pid":15641,…}],"shell_pid":14516}
```

Claude Code runs every Bash tool call, foreground or background, as a child
`/bin/bash -c source ~/.claude/shell-snapshots/snapshot-bash-….sh … ` in its
own session (`pid == pgid == sid`). Its stdout goes to
`$TMPDIR/claude-<uid>/<project>/<session>/tasks/<task-id>.output`. So the
process isn't in the foreground group, and herdr never sees it. Observed
under the probe's claude pid:

```
  PID  PPID  PGID   SID CMD
16107 15641 16107 16107 /bin/bash -c source /home/jeffu/.claude/shell-snapshots/sn…
16112 16107 …            sleep 240
```

loopherd could walk `/proc` from the claude pid that `process-info` gives.
While the agent is stopped, any surviving `shell-snapshots` child is a
background shell. MCP stdio servers have their own command lines, so they
can be told apart. This works on Linux only, and it rests on an
undocumented Claude Code layout that can change in any release. Use it only
as a fallback.

### 2. herdr agent detection: no; possible upstream

- herdr has no Claude state hooks. The v11 integration
  (`~/.claude/hooks/herdr-agent-state.sh`) runs only on `SessionStart` and
  reports only the session id (`pane.report_agent_session`). State comes
  from screen rules in the detection manifest ([agents.mdx "How status
  works"][agents]).
- The manifest already treats `· N MCP tasks still running` and
  `Waiting for N background agents to finish` as `working`
  (`background_mcp_task_working`, `background_agents_working` in
  `~/.local/state/herdr/agent-detection/remote/claude.toml`). It has no rule
  for `· N shell still running`, so the probe read `done`/`idle`.
- herdr's docs say the general principle outright: some agents "count as
  idle once they return to their prompt, even if background tasks … are
  still running. `agent wait` … observe[s] the agent's state, not completion
  of those jobs" ([agents.mdx][agents]).
- A `shell still running` rule upstream would make herdr itself report
  `working`. That would fix `agent wait` and every other herdr consumer.
  But it is herdr's call, and screen text is fragile.
- herdr also offers display-only pane tokens (`pane report-metadata --token
  name=value`, which `agent get`/`list` return; [socket-api.mdx
  `pane.report_metadata`][api]). A hook could publish `bg_tasks=1` there
  without touching lifecycle state.

### 3. Claude Code `Stop` hook: yes

[Hooks → Stop input][hooks]: Stop hooks receive `background_tasks` and
`session_crons`. Each task has `id`, `type` (`shell`, `subagent`,
`monitor`, `workflow`, `teammate`, `cloud session`, `MCP task`), `status`,
`description`, and `command` (shell only). Both arrays "are present when the
task registry is reachable and are empty when nothing is in flight or
scheduled". `SubagentStop` gets the same arrays, scoped to the parent
session. The live run above confirms both the populated and the empty case
on 2.1.316.

`session_crons` matters for the same reason: a member that used
`ScheduleWakeup`/`/loop` is also paused, not finished.

Caveats:

- A missing array means "unknown", not "none". Treat it that way.
- loopherd must install the hook. A `--settings <file>` argument at spawn
  merges with the user's settings, and the live probe used exactly that.
  The hook runs with the member's env (`HERDR_PANE_ID`), so it can find its
  member the way `loopherd report` does.

Other Claude-side signals we checked and rejected:

- **Status line input** has no shell/background field. Its `tasks` array
  covers only subagent rows (`type: local_agent`) ([statusline][sl]).
- **Session transcript** (`~/.claude/projects/…/<session>.jsonl`) records
  `backgroundTaskId` when a task launches. Completion arrives only as a later
  notification, so rebuilding the set means parsing an undocumented format.
- **Screen footer** `· 1 shell still running` (via `pane read`) works, but
  it's the same fragile text that herdr's rules match. It could serve as a
  cheap fallback.

### 4. loopherd fallback: a stop with no report: yes, as a hint

`watch` (`src/watch.rs`) already labels the wake `lh-60 idle  (was
working)`. It also reads each member's report mailbox (`src/report.rs`) in
the same poll. Adding "no `done`/`question` report since it last started
working" to the wake line costs nothing and needs no Claude cooperation. It
can't tell a background shell from a member that stopped without reporting
for some other reason, so the shepherd still judges.

## Recommendation

1. **Primary:** loopherd installs a Stop hook into every member it spawns
   (via `--settings`). The hook records `{at, background_tasks,
   session_crons}` for the member under the loopherd state dir.
   `watch` reads the record. A stop with in-flight tasks or crons renders as
   `lh-60 done  (paused: 1 shell)` and does not count as finished.
2. **Always:** a wake for a member with no `done`/`question` report since it
   started working says so (`(was working, no report)`). That covers members
   spawned before the hook existed, and covers the hook failing.
3. **Not:** `/proc` walking or screen-text matching inside loopherd. Proposing
   a `shell still running` manifest rule upstream to herdr is optional and
   independent of the two cards above.

[hooks]: https://code.claude.com/docs/en/hooks#stop-input
[sl]: https://code.claude.com/docs/en/statusline#task-fields
[agents]: https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/agents.mdx
[api]: https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/socket-api.mdx
