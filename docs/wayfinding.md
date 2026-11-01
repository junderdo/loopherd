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
| D6 | loopherd covers work (zagops) and personal projects on one machine, and stays a personal tool: teammates never consume herd definitions. Multi-machine (`herdr --machine`) is out of v1; the `Herdr` trait must not rule it out. No special guards for work repos: loopherd sends only prompts you wrote, under the permission mode you chose. | Work PRs are the D3 pain, and they already run in the same herdr session. Teammates don't run herdr. There's no second machine yet. Grilled in LH-4. |
| D7 | Herd files live only in `$XDG_CONFIG_HOME/loopherd/herds/<herd>.toml` (default `~/.config/loopherd/herds/`), one file per herd, named by its filename. `LOOPHERD_CONFIG_DIR` overrides the directory for tests and dev. No repo-local lookup, no merging. Versioning them (e.g. in dotfiles) is the user's business. | They're personal config spanning repos (D6). Committing them to a work repo would be noise for teammates. Grilled in LH-5. |
| D8 | Revive (slice 2) is a manual, repeatable `loopherd revive [--dry-run] [--name]` over ledger entries only. It waits for herdr's own resume to settle, then for each entry that isn't retired and whose cwd and workspace still exist: a live agent with its session ID is `alive`; otherwise it reuses a matching shell pane or opens a tab named for the agent and runs `claude --resume <session> <claude_args>`. It never answers dialogs, re-sends prompts, replaces a name held by another session, or recreates workspaces. The ledger gains `claude_args`, `workspace_label`, and `retired`; `loopherd retire <name>` only marks an entry finished. Built as LH-17 (retire) then LH-18 (revive). | herdr already resumes agents it started but drops their args, and closed tabs aren't resumed at all (LH-10). Session ID is the only handle that survives everything. Retire stops merged agents being resurrected. Grilled in LH-16. |
| D9 | Slice 3: a herd file (`<herd>.toml`, D7) holds `repo`, an optional `workspace` label (default: current workspace; an unknown label is an error), optional `args`, and `[template.<t>]` entries with `params`, a `name` pattern (default `{template}-{params}`), an optional `branch` pattern (unset: no worktree) or instead a `worktree` pattern (`{param}`, `{herd}`, `{name}`) for a worktree at that slug detached at origin's default branch, with no local branch and an empty `{branch}` (LH-24), `prompt` or `prompt_file`, and `args`. `loopherd add <herd> <template> <params…> [--dry-run] [-- args]` takes positional params and spawns through spawn's code path; args go herd, then template, then CLI; the ledger records `herd` and `template`. Placeholders are `{param}` plus the built-ins `{name}` `{herd}` `{branch}` `{worktree}` `{pane}`, with `{{`/`}}` escapes; spawn prompts get the built-ins. Unknown placeholders fail before anything is created. loopherd stays free of GitHub knowledge: a PR template uses `worktree` and its prompt runs `gh pr checkout --detach`, which works even when the PR's branch is checked out elsewhere. `loopherd herds` lists and validates herd files. Built as LH-14, LH-20, LH-21, LH-24. | Matches how workspaces are already used (by domain: `milk lab`, `zagops`), and several herds can share one. Members are tracked by name and session, so they may move. Grilled in LH-6, with LH-14. |
| D10 | loopherd never answers `blocked` agents: spawn, revive, prompt and add only report them. Revisit with a per-dialog allowlist only if a real loop keeps stalling on the same dialog. | The only routine blocker, the trust dialog, has a setup fix (LH-15). Answering on the human's behalf is the riskiest thing loopherd could do. Grilled in LH-7. |
| D11 | The ledger is a JSON file at `$XDG_STATE_HOME/loopherd/ledger.json`, written by temp file + rename. Lost updates from concurrent writers are a known gap, closed with `flock` when needed. Revisit SQLite only if the queue slice needs many concurrent writers. | A few loopherd processes on one machine (D6). The file is easy to inspect. LH-8, [ADR 0001](adr/0001-json-file-ledger.md). |

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
| Q4 | LH-2 | Does herdr offer an event stream, or do we poll `agent list`? **Answer:** yes, `events.subscribe` on the socket pushes per-pane agent status; bootstrap and reconcile with `session.snapshot` ([research](research/herdr-events.md)). |
