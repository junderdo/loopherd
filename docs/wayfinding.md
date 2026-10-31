# loopherd — wayfinding

A living doc: where we're headed, what's decided, what's still open.
Background on herdr is in [research/herdr.md](research/herdr.md).

## North star

A Rust TUI for Linux that groups coding agents into **herds** and keeps
them working together across sessions, by layering on top of herdr.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Layer on herdr via its CLI/socket API; don't build a multiplexer. | herdr already owns PTYs, detection, persistence of shells. Coordination is the unsolved part. |
| D2 | All four outcomes are in scope: survive restarts, group by goal, agents cooperate, work queue/loops. | Stated goals; sequenced below. |
| D3 | First pain to kill: **repeating setup**. | Today: a tab + worktree + agent per PR, and a "next priorities" agent per personal repo, all set up by hand. |
| D4 | Name: `loopherd`. | — |
| D5 | Slice 1 is `loopherd spawn` (LH-9): a herd-less tracer bullet that creates an optional worktree, a herdr tab, a named Claude agent, and its opening prompt, and records the agent in a ledger. CLI only. herdr sits behind a trait over its CLI. Tests use a fake, plus live tests gated on `LOOPHERD_LIVE=1`. | Proves the loop on a small, testable slice that fixes the most frequent pain (D3). Recording agents at birth feeds revive without a background recorder. Grilled in LH-3. |

## Emerging model

One concept seems to carry all four outcomes: a **herd** declared in a file.

- **Herd** — a named, cross-cutting group with a purpose
  (e.g. `zagops-prs`, `personal-priorities`). Not tied to one herdr workspace.
- **Member template** — how to spawn one member: repo, worktree naming,
  agent kind, starting prompt, parameters (e.g. a PR number).
- **Member** — a live instance: herdr agent name, pane, Claude session ID,
  current task, status.
- **Ledger** — loopherd's persisted record of members. It is what makes
  restarts survivable (`claude --resume <session-id>`).
- **Task / queue** — later: work items a herd's idle members pull from.

Sketch (format not decided):

```toml
# herds/zagops-prs.toml
name = "zagops-prs"
repo = "~/work/projects/zagops/zagops"

[template.pr]
params = ["number"]
worktree = "pr-{number}"
agent = "claude"
prompt = "Check out PR {number}, read review comments and CI, and report what's needed."
```

```bash
loopherd add zagops-prs pr 1050   # worktree + herdr tab + named agent + prompt
loopherd                          # TUI: members grouped by herd, what needs me
loopherd revive                   # after reboot: resume every member from the ledger
```

## Candidate vertical slices

Each one is usable end to end on its own, and they're in build order.

1. **Spawn** (LH-9, D5). `loopherd spawn` sets up and starts one named agent
   and records it in the ledger. Kills D3.
2. **Revive.** After a herdr restart, rebuilds missing agents from the ledger
   using `claude --resume`, falling back to herdr's own session file for
   agents spawned elsewhere. Open fact: do pane IDs survive a herdr restart?
3. **Herds and templates.** Herd files and `loopherd add <herd> <template>
   <params>`, built as sugar over spawn.
4. **Herd view.** A TUI listing members grouped by herd with live state
   (`blocked` / `done` first). It is read-only.
5. **Queue.** Each herd gets a task list, and idle members are fed the next
   task, with checks and stop conditions.
6. **Cooperate.** Lead and worker roles: a lead dispatches to workers and
   collects their reports.

## Dev-loop constraints (loop engineering applied to loopherd itself)

- **Never touch the live herdr session in tests.** Put herdr behind a trait,
  so tests use a fake and manual runs use `herdr --session loopherd-dev`.
- Back-pressure: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- Prefer CLI subcommands first, with the TUI as a view over the same core.
  Agents can drive and test a CLI, but not a TUI.

## Open questions

Each question is tracked as a card on the
[board](https://trello.com/b/MtiTIrp1/loopherd), where its details and
discussion live. When one is resolved, record it under Decisions and remove
its row here.

| Q | Card | Question |
|---|---|---|
| Q1 | LH-5 | Where do herd files live: in each repo, in `~/.config/loopherd/`, or both? |
| Q2 | LH-4 | Is this for work (zagops) as well as personal projects? |
| Q3 | LH-8 | Ledger storage: a TOML/JSON file or SQLite? Blocked on LH-4 and LH-5. |
| Q4 | LH-2 | Does herdr offer an event stream, or do we poll `agent list`? |
| Q5 | LH-7 | May loopherd answer `blocked` agents, or only surface them? |
| Q6 | LH-6 | Should a herd map to one herdr workspace, or can members be scattered? |
