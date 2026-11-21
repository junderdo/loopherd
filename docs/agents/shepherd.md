# Shepherd runbook: loopherd-cards

Read `loopherd --skill` first: it is the shepherd's and lead's guide for any
herd (D19). This file adds only what's specific to this repo's herd,
`loopherd-cards`, whose members work Trello cards per
[issue-tracker.md](issue-tracker.md). Setting up a herd in another repo
after `loopherd init` is the skill's "Setting up a project".

## Each session

Start with `loopherd doctor`, as the skill's Shepherd section says, and relay
any failure to the human with the fix it names. `scripts/install` also runs it
last, so a failure there is the same report.

## Feed

From the main checkout, `git pull --ff-only`, then refresh the `loopherd` on
PATH:

```bash
scripts/install
```

It needs `jq`. It builds, copies the binary to `~/.local/bin/loopherd` through
a temp file and a rename, so a failed build keeps the last good one, and runs
`loopherd install-hook`, which installs the hooks and the skill
(`~/.agents/skills/loopherd`, linked from `~/.claude/skills/loopherd`). Then:

```bash
loopherd feed loopherd-cards --dry-run
loopherd feed loopherd-cards
```

The herd file (`~/.config/loopherd/herds/loopherd-cards.toml`) turns claimable
Backlog cards into members `lh-<n>` on `<type>/LH-<n>-<slug>`.

## Cards through the loop

- A member's `done` report carries its PR; the card sits in Review. A card
  back in Backlog with `need-human` carries the question.
- **Verify** with a read-only subagent: no `LOOPHERD_LIVE=1`, its worktrees
  and `CARGO_TARGET_DIR` under one `<scratch>/pr<n>/` (see the skill's Verify).
- **After merge:** move the card to Done, run `scripts/trello unblock`, then
  `loopherd feed loopherd-cards`. With the herd file's `after_merge` set (see
  the lead example), feed itself runs the pull, install and unblock once per
  merged member, after releasing the feed lock, and notes the result in the
  member's mailbox. In a led herd the lead's watch usually feeds first; with
  its `install` switch off that run is held back and counted as done for
  good, so your later feed runs nothing: read the member's note and run the
  steps by hand. Run feed in the background when after_merge is set, since
  `scripts/install` can take minutes. If a feed dies partway, the next feed
  notes the after_merge as interrupted, with no outcome; check what ran and
  finish the steps by hand. Once a merge that changes the broker is
  installed, run `loopherd broker-restart <lead>` for each lead in the
  background: it waits for the request its broker is serving, then starts
  one on the new binary. `loopherd doctor`, which `scripts/install` runs
  last, warns for each broker still on another binary.

- **Triage** (the skill's Triage, D60): the herd file's `[queue] backlog`
  (see [examples/loopherd-cards-routed.toml](../examples/loopherd-cards-routed.toml))
  lists the unready cards. Once the human approves the lead's triage:

  ```bash
  scripts/trello label LH-<n> +ready-for-agent      # each ready card
  scripts/trello comment LH-<n> "<what's missing>"  # each needs-spec card
  ```

  `scripts/trello` can't archive, so archive each stale card the human
  confirms through the API:

  ```bash
  set -a; . "${XDG_CONFIG_HOME:-$HOME/.config}/loopherd/trello.env"; set +a
  curl -fsS -X PUT "https://api.trello.com/1/cards/<shortLink>?closed=true" \
    -H "Authorization: OAuth oauth_consumer_key=\"$TRELLO_API_KEY\", oauth_token=\"$TRELLO_TOKEN\""
  ```

  `<shortLink>` is the last part of the card's URL in `scripts/trello show`.

## Lead

[examples/loopherd-cards-lead.toml](../examples/loopherd-cards-lead.toml) is
the `[lead]` table and prompt for `loopherd-cards`. Its merge line, for the
human to paste:

```
! GH_TOKEN=$(gh auth token --user junderdo) gh pr merge <n> --merge --match-head-commit <full sha>
```

A switch its `[lead]` leaves unset comes from `[lead_defaults]` in
`~/.config/loopherd/config.toml` (`scripts/install --lead-defaults` to change
them), else is off; `loopherd herds` shows each switch's value and source.

Each lead runs in the Claude Code sandbox from settings loopherd writes
from its switches (D89), a guard rail rather than containment (D89 lists
what gets past it). `loopherd doctor` fails when the sandbox can't start
here while a herd has a lead, and `[lead] sandbox = "require"`, the
default, then refuses to start the lead; `prefer` starts it guarded by its
hook alone, flagged in `loopherd view`, and only the human sets it. The
same mode governs what the human's own `~/.claude/settings.json` loosens
(D100): `require` refuses the lead naming each file and key, `prefer`
starts it sandboxed with them as warnings; doctor reports them. A lead
gets no MCP servers but those its `[lead] mcp_servers` names, which run
unsandboxed; only the human adds one. A lead gets no Unix sockets, so no
herdr socket, ssh agent or WSL2 interop, unless its herd sets
`[lead] unix_sockets = "all"`, which `lead`, `revive` and doctor warn of
(D105). Where the sandbox can't block them (no seccomp filter; doctor
names the npm package), `require` refuses the lead and `prefer` starts it
as `(lead, sockets open)`; `restart` applies the same rule. A `credentials`
lead is refused under both. Under `unix_sockets = "all"` the check refuses
and flags nothing; installing the package is the human's. What still gets past the sandbox: workers run unsandboxed (LH-284)
and most of the state dir is writable (LH-298). A lead's `tell`, `read`, `status`,
`feed`, `add`, `prompt`, `restart`, `retire` and `release` run in its
broker, outside the sandbox and only on its own herd (D98); its watch
still calls herdr itself, run bare outside the sandbox (D104); doctor fails
for a lead with no broker running, and `loopherd revive --name <lead>`
from an unsandboxed shell starts it. Doctor
names each running lead without its sandbox; `loopherd revive --name
<lead>` after quitting its Claude resumes it in one. A lead started
before loopherd kept its record (`<config dir>/sandbox/<lead>.lead.json`)
fails a plain or startup `revive` until that `--name` run adopts it. A
lead can't make worktrees beside its own, so `loopherd spawn --worktree`
for its herd is yours; it uses `add`. A lead can still write most of
its repo's shared `.git`, so git you run in that repo or its worktrees
may run a command it planted; loopherd's git for a herd's members
doesn't (D89). Check a hand-spawned entry's `repo` in the ledger before
`retire --close` or `release` on it.

## Notify

The human's `[notify]` table in `~/.config/loopherd/config.toml` decides
whether questions and blocked dialogs also reach them outside herdr (see
"Notifying the human" in `loopherd --skill`). Each lead's watch sends its
workers' and its own; yours sends the rest and, deduplicated by per-event
marks in the reports directory, the leads' too. Tests and manual runs set a
temp `LOOPHERD_CONFIG_DIR` (`scripts/herdr-dev` exports
`$XDG_RUNTIME_DIR/loopherd-dev/config`, else a fresh `mktemp -d`, unless one
is set), so they never send to the human; give such a run its own
`[notify]` with a stub `command` to see a send.

## Gotchas

- Run the live suite after changing retire or feed output.
- `scripts/trello` can't reorder, edit descriptions or delete; use curl with
  the OAuth header from `~/.config/loopherd/trello.env`.
- A card that settles a decision, of any kind, adds a new
  `docs/decisions/D<n>.md` file plus an index row in `docs/wayfinding.md`.
  Check the highest D file first; card numbers are never reused. Two PRs
  colliding on a D number follow the renumber rule in
  [issue-tracker.md](issue-tracker.md).
- Push as junderdo:
  `GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push …`.
