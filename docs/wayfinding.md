# loopherd — wayfinding

A living doc: where we're headed, what's decided, what's still open.
Background on herdr is in [research/herdr.md](research/herdr.md).

## North star

A Rust TUI for Linux that groups coding agents into **herds** and keeps
them working together across sessions, by layering on top of herdr.

## Decisions

Each decision lives in its own file under [decisions/](decisions/). A new
decision is a new `decisions/D<n>.md` plus a row here.

| # | Decision |
|---|---|
| [D1](decisions/D1.md) | Layer on herdr via its CLI/socket API; don't build a multiplexer. |
| [D2](decisions/D2.md) | All four outcomes are in scope: survive restarts, group by goal, agents cooperate, work queue/loops. |
| [D3](decisions/D3.md) | First pain to kill: repeating setup. |
| [D4](decisions/D4.md) | Name: `loopherd`. |
| [D5](decisions/D5.md) | Slice 1 is `loopherd spawn`: a herd-less tracer bullet that starts one agent and records it in a ledger. |
| [D6](decisions/D6.md) | One machine, work and personal projects; a personal tool that teammates never consume. |
| [D7](decisions/D7.md) | Herd files live only in `$XDG_CONFIG_HOME/loopherd/herds/<herd>.toml`, one file per herd. |
| [D8](decisions/D8.md) | Slice 2: `loopherd revive` resumes non-retired ledger entries by session ID; `retire` marks one finished. |
| [D9](decisions/D9.md) | Slice 3: herd files with member templates, and `loopherd add <herd> <template> <params…>` over spawn. |
| [D10](decisions/D10.md) | loopherd never answers `blocked` agents; it only reports them. |
| [D11](decisions/D11.md) | The ledger is one JSON file, written by temp file + rename under `ledger.lock`. |
| [D12](decisions/D12.md) | Slice 4a: `loopherd status`, a read-only view of members by herd, plus strays. |
| [D13](decisions/D13.md) | Slice 4b: `loopherd view`, a ratatui TUI that polls status's core every 2s. |
| [D14](decisions/D14.md) | Slice 5: `loopherd feed <herd>`, a one-shot run that reaps finished members and spawns fresh ones up to `max`. |
| [D15](decisions/D15.md) | Slice 6: the shepherd's tools first: a mailbox (`report`/`reports`) and `watch`. |
| [D16](decisions/D16.md) | Stops land in the mailbox through user-level Claude hooks, matched to members by socket, pane and process. |
| [D17](decisions/D17.md) | A lead is a loopherd-run member that runs one herd's loop unattended, within switches the human sets. |
| [D18](decisions/D18.md) | A D-number collision between two PRs is fixed by the PR that merges second, renumbering on the add/add conflict. |
| [D19](decisions/D19.md) | The shepherd's and lead's guide ships with the binary as `loopherd --skill`, installed as a Claude skill; repos keep only their runbook. |
| [D20](decisions/D20.md) | A member past its context limit writes a handoff when its Stop hook asks; `loopherd restart` clears its session and has it resume from the handoff; the shepherd's own watch tells it when to hand off. |
| [D21](decisions/D21.md) | A ledger entry matches a caller only on pane and the same herdr socket; a socketless entry matches only a caller without one. |
| [D22](decisions/D22.md) | `watch`'s lead lookup and `retire --close`'s reuse check match on pane and herdr socket like D21; a pane id in another herdr blocks neither. |

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
- **Shepherd** — the agent working live with the human to run the herd: feeds, watches, verifies and cleans up members (`loopherd --skill`; this repo's runbook is [agents/shepherd.md](agents/shepherd.md), D19). Distinct from a herd's **lead**, a member loopherd itself runs (D17).
- **Task / queue** — a line from a herd's `source` command; each task gets its own member, up to the herd's `max` (D14).

Herd file (D9):

```toml
# ~/.config/loopherd/herds/zagops-prs.toml  (herd name = filename)
repo = "~/work/projects/zagops/zagops"
workspace = "zagops"
args = ["--permission-mode", "auto"]

[template.pr]
params = ["number"]
name = "zpr-{number}"
worktree = "pr-review-{number}"
prompt = "You're {name}. Run `gh pr checkout --detach {number}`, read review comments and CI, and report what's needed."
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
2. **Revive** (LH-17, LH-18, D8). `loopherd revive` brings non-retired
   ledger agents back after a herdr restart or a closed tab, keyed on the
   Claude session ID, and re-passes the launch args herdr drops
   ([research](research/herdr-restart-ids.md)).
3. **Herds and templates** (LH-14, LH-20, LH-21, D9). Herd files and
   `loopherd add <herd> <template> <params>`, built as sugar over spawn.
4. **Herd view** (LH-42, LH-43, D12). `loopherd status` lists members
   grouped by herd with live state (`blocked` / `done` first), then a TUI
   over the same core. It is read-only.
5. **Queue** (LH-49, LH-50, D14). `loopherd feed <herd>` reaps members
   whose `done` check passes, then spawns one fresh member per task from the
   herd's source command, up to `max` open members.
6. **Cooperate** (LH-58, LH-59, D15). Members report to a mailbox
   (`loopherd report`) and `loopherd watch` wakes the shepherd when one
   reports or stops. A lead member runs a herd's loop unattended on the same
   tools (LH-76, D17).

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
| Q4 | LH-2 | Does herdr offer an event stream, or do we poll `agent list`? **Answer:** yes, `events.subscribe` on the socket pushes per-pane agent status; bootstrap and reconcile with `session.snapshot` ([research](research/herdr-events.md)). |
