# herdr — research notes

Researched 2026-10-31 against herdr 0.9.5 (installed at `~/.local/bin/herdr`).

## What it is

Agent-aware terminal multiplexer, Rust + Ratatui, AGPL-3.0. A background
server owns the PTYs; TUI clients attach/detach. Topology is
workspace → tab → pane, with opaque stable IDs (`w1`, `w1:t1`, `w1:p1`;
closed IDs are never reused).

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

## Control surface (everything we need to layer on top)

All commands return JSON over the socket API.

| Need | Command |
|---|---|
| Observe everything | `herdr api snapshot`, `agent list`, `workspace list`, `pane list` |
| Build layout | `workspace create`, `tab create`, `pane split --cwd … --no-focus` |
| Worktrees | `worktree create/open/list/remove` |
| Launch agent | `agent start <name> --kind claude --pane <id> -- <args>` (pane must be at a shell prompt) |
| Talk to agent | `agent prompt <name> "…" --wait --timeout <ms>`, `agent send-keys` |
| Read output | `agent read`, `pane` read commands |
| Synchronize | `agent wait <name> --until <states>` |
| Subscribe | socket `events.subscribe` (no CLI wrapper), see [herdr-events.md](herdr-events.md) |
| Remote | global `--machine <label>` prefix; IDs are per-server |
| Display metadata | `workspace report-metadata` |

Gotchas from the bundled skill (`herdr --skill`):
- `agent prompt` refuses a `blocked` agent (`agent_blocked`); `--wait` can
  return `agent_prompt_stalled` if no activity appears within 5 s.
- `idle`/`done` don't prove an unrecognized dialog isn't showing.
- A pane moved between workspaces gets a new ID — track agents by name.
- Remote connection failures don't prove a mutation wasn't applied.

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
