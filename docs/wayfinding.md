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

Each one is usable end to end on its own.

1. **Spawn from a template.** `loopherd add <herd> <template> <params>` creates the
   worktree, tab, and agent via herdr, sends the prompt, and records the
   member in the ledger. Kills D3.
2. **Herd view.** A TUI listing members grouped by herd with live state
   (`blocked` / `done` first). It is read-only.
3. **Revive.** Rebuilds missing members from the ledger after a herdr
   restart, using `claude --resume`.
4. **Queue.** Each herd gets a task list, and idle members are fed the next
   task, with checks and stop conditions.
5. **Cooperate.** Lead and worker roles: a lead dispatches to workers and
   collects their reports.

## Dev-loop constraints (loop engineering applied to loopherd itself)

- **Never touch the live herdr session in tests.** Put herdr behind a trait,
  so tests use a fake and manual runs use `herdr --session loopherd-dev`.
- Back-pressure: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- Prefer CLI subcommands first, with the TUI as a view over the same core.
  Agents can drive and test a CLI, but not a TUI.

## Open questions

- [ ] Q1: Where do herd files live: in each repo, in `~/.config/loopherd/`, or both?
- [ ] Q2: Is this for work (zagops) as well as personal projects? That decides
      multi-machine and `--machine` support, and how careful we are about
      prompts.
- [ ] Q3: Ledger storage: a TOML/JSON file or SQLite?
- [ ] Q4: Does herdr's API offer an event stream, or do we poll `agent list`?
      (`herdr api schema` is worth reading.)
- [ ] Q5: Should loopherd be able to answer `blocked` agents, or only surface them?
- [ ] Q6: Should a herd map to one herdr workspace, or can members be scattered?
- [ ] Q7: Is slice 1 really first? Revive is the gap herdr can't fill, while
      templates are something herdr might add itself.
