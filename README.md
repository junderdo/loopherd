# loopherd

Herd your coding agents. loopherd is a terminal UI for Linux that groups
coding agents into **herds** and keeps them working together across
sessions and restarts.

It doesn't replace your terminal multiplexer. It layers on top of
[herdr](https://herdr.dev/), which already runs the terminals and knows
whether each agent is working, blocked, or done. loopherd adds the parts
herdr leaves to you:

- **Templates**: declare once how to spawn a herd member (repo, worktree,
  agent, opening prompt), then `loopherd add zagops-prs pr 1050`.
- **Groups that span repos**: e.g. every open-PR agent, or every personal
  project's "next priorities" agent, in one view sorted by what needs you.
- **Revival**: a ledger of each member's task and agent session, so a reboot
  doesn't lose your agents (`claude --resume`).
- **Queues and cooperation** (later): idle members pull the next task, and a
  lead agent dispatches to workers.

## Status

Early wayfinding: nothing runs yet. Direction, decisions, and open questions
are in [docs/wayfinding.md](docs/wayfinding.md), and what herdr offers is in
[docs/research/herdr.md](docs/research/herdr.md).

## Install

```bash
scripts/install
```

(needs `cargo` and `jq`) builds loopherd, installs it as `~/.local/bin/loopherd`, and adds its Stop
and StopFailure hooks to `~/.claude/settings.json` (backing the file up
first). The hooks record each stop of a herd member in its mailbox and do
nothing in any other Claude session. Re-run it after pulling; it changes
only what's out of date. `loopherd install-hook --remove` takes the hooks out.

## One-time setup: trust each repo

Claude Code asks "Is this a project you created or one you trust?" the first
time it starts in a repo, and `loopherd spawn` stops there (exit 3). Trust is
keyed on the repo's **main checkout**, and every worktree of the repo shares
it ([details](docs/research/claude-workspace-trust.md)). So trust each repo
once, before spawning into its worktrees:

```bash
cd <project>/<project>   # the main checkout
claude                   # accept the trust dialog, then exit
```

Trusting a parent directory such as `<project>/` or `<project>/worktrees/`
doesn't count for the repos inside it.

If a spawn still blocks, answer the dialog in the pane it names, then send
the stored prompt with `loopherd prompt <name>`.

## How it's built

loopherd is also a loop-engineering experiment: much of it is meant to be
written by agents pulling cards from the
[Trello board](https://trello.com/b/MtiTIrp1/loopherd), with tests and
lints as the back-pressure. Agent instructions start at
[AGENTS.md](AGENTS.md), and the card workflow is in
[docs/agents/issue-tracking.md](docs/agents/issue-tracking.md).

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test
scripts/trello help    # board access; needs TRELLO_API_KEY / TRELLO_TOKEN
```

## License

GPL-3.0. See [LICENSE](LICENSE).
