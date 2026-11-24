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

Refresh the `loopherd` on PATH:

```bash
loopherd update
```

For a source install it fast-forwards the recorded checkout's default branch
and runs `scripts/install --update`; it refuses a dirty tree or another
branch checked out, naming why. By hand, from the main checkout:
`git pull --ff-only && scripts/install --update`. `scripts/install` needs
`jq`. It builds, copies the binary to `~/.local/bin/loopherd` through a temp
file and a rename, so a failed build keeps the last good one, and runs
`loopherd install-hook`, which installs the hooks and the skill
(`~/.agents/skills/loopherd`, linked from `~/.claude/skills/loopherd`). A
release install (the one-line installer) also refreshes with
`loopherd update`. Restart any open `loopherd view` after it. Then:

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
  installed, each lead's broker moves onto it by itself (D124).
  `loopherd doctor`, which `scripts/install` runs last, warns for each
  broker still on another binary, and a broker that couldn't move notes its
  lead's mailbox: for those, run `loopherd broker-restart <lead>` in the
  background; it waits for the request its broker is serving, then starts
  one on the new binary.
- **Brokers after a host restart.** When a host restarts under herdr
  (WSL shutting down), herdr restores each lead's pane and Claude, and the
  plugin's startup hook starts each alive lead's broker, whether or not
  `[herdr_plugin] revive_on_startup` is on (D134). If a broker still isn't
  running (the plugin isn't installed, or it died later), the lead's
  `feed`, `status` and `report` fail and your watch prints
  `<lead> no broker`. Run `loopherd broker-restart <lead>`, which starts
  one when none runs.

- **Stalled** (D152): a watch's `<member> stalled  paused <n>m: 1 shell`
  means the member stopped paused on its own background work and herdr has
  shown it idle since, past its herd's `stalled_after` (default 30m). Read
  its pane with `loopherd read <member>`. If the shell it waits on is hung
  (a `cargo test` well past its timeout, say), `loopherd tell` it to drop
  that shell and carry on, usually by rerunning the step in the foreground.
  A lead handles its own workers' stalls; you get a lead's own (only when
  its watch isn't running) and those of unled herds' members.

- **Triage** (the skill's Triage, D60, D116): the herd file's `[queue] backlog`
  (see [examples/loopherd-cards-routed.toml](../examples/loopherd-cards-routed.toml))
  lists the unready cards. A herd with that file's `[template.groom]` gets
  a `groom-<herd>` member on `running low`, started by its lead, or by you
  with `loopherd add <herd> groom` for an unled herd. Its report ends in a
  numbered batch, each line `<n>. ` and then one of the commands below,
  with its card and text filled in. Relay the report; when the human
  approves the batch, or all but some lines, run each approved line's
  command, as written after its number, and no other, then `loopherd
  retire --close groom-<herd>` if no lead did. Without a batch, once the
  human approves the lead's triage, run the same commands yourself:

  ```bash
  scripts/trello label LH-<n> +ready-for-agent      # each ready card
  scripts/trello comment LH-<n> "<what's missing>"  # each needs-spec card
  scripts/trello archive LH-<n>                     # <reason>: each stale card the human confirms
  ```

  Comment text is double-quoted with any `"`, `$`, `` ` `` or `\` inside
  it escaped by a backslash.

## Grilling

A `wayfinding:grilling` card runs as an attended member of `loopherd-cards`
(D112), from the `[template.grill]` in
[examples/loopherd-cards-grill.toml](../examples/loopherd-cards-grill.toml),
whose prompt is [grill-prompt.md](grill-prompt.md). When the human asks to
grill a card, run from your own pane:

```bash
loopherd add --no-focus loopherd-cards grill <n> docs <slug>
```

`add` makes the tab, worktree and branch, starts and prompts the grill, and
prints its `pane <id>`. The grill claims the card and waits for your
background before its first question, so always send it, then take the human
there (D145). Sending it before the focus keeps `tell` out of an input box
the human is typing in. The background starts `Background for LH-<n>`, then
one line each for the decisions (`docs/decisions/D<n>.md`), cards and files
it should read and what each settles, or `none beyond the card`:

```bash
background=$(mktemp)
cat > "$background" <<'LOOPHERD_GRILL_BACKGROUND_END'
Background for LH-<n>
<the background>
LOOPHERD_GRILL_BACKGROUND_END
loopherd tell grill-<n> --text-file "$background"
rm -f "$background"
herdr agent focus <pane>
```

The quoted heredoc keeps the background's backticks and `$(…)` from running in
your shell; never pass it inline in double quotes. A line of the background
that is exactly the delimiter would end it early and run the rest as shell, so
check for one before sending. The grill sends the human back to your pane
when the conversation ends.

- **`add` printed `blocked: answer it in pane <p>, …`**: the grill was never
  prompted. Focus that pane for the human to answer the dialog, run
  `loopherd prompt grill-<n>`, then the block above.
- **`tell` failed after a prompted start**: still focus the pane, and have
  the human give the grill the background themselves.

### Interactive or batch

A grill runs in one of two modes (D132), picked by the human with the
`grill:batch` label (docs/agents/issue-tracker.md says when each fits):

- **Interactive**, no label: the attended grill above, one question at a
  time. The human can say "batch the rest" partway.
- **Batch, attended**: the same `add`, on a card with `grill:batch` or when
  the human asks for batch (the grill claims the card, which keeps feed's
  `grill-batch` off it); the grill answers every question, then shows the
  human the whole list in its pane to accept or override by number.
- **Batch, unattended**: a `grill:batch` card in Backlog is claimable, and
  the herd's routed queue
  ([examples/loopherd-cards-routed.toml](../examples/loopherd-cards-routed.toml))
  sends it to `grill-batch`, an ordinary member a lead's feed may start. It
  posts its batch as a card comment, adds `need-human`, moves the card to
  Backlog and reports a question; its `done` check then passes and feed
  reaps it. When the human has answered on the card, restart it:

  ```bash
  scripts/trello label LH-<n> -need-human
  loopherd release grill-batch-<n>
  ```

  The next feed starts a fresh `grill-batch-<n>` (feed skips a retired name
  until it's released), which reads the answer and writes the decision.
  Never restart it without the human's answer on the card. `loopherd stats`
  doesn't compare the two modes (D132).

After its PR merges, the herd's watch prints `grill-<n> merged`. The grill
moves the card to Done, runs `scripts/trello unblock` and sends you
`LH-<n> merged and closed out: …`, but only if it notices the merge first.
In a led herd the lead's watch usually wins: the lead moves the card to Done,
feeds, and its reap (`retire --close grill-<n>`) closes the grill's pane,
worktree and branch before the grill runs its close-out. The lead then leaves
you a note, `grill-<n> merged: LH-<n> closed out`, which your watch prints as
`lead-loopherd-cards note …`; take that as the close-out and do nothing more.
The lead runs its own copy of its prompt
(`~/.config/loopherd/herds/loopherd-cards-lead.md`), so once D145 lands, have
the human carry its after-merge note sentence from
[examples/loopherd-cards-lead.md](../examples/loopherd-cards-lead.md) into
that copy by hand (a copy-over drops their local edits), then restart the
lead. For an unled herd, close out either way below; feed's `after_merge` and
the reap (once the card is in Done) then close the grill's pane, worktree and
branch.

- **On the grill's line**: check
  `GH_TOKEN=$(gh auth token --user junderdo) gh pr view <pr> --json state --jq .state`
  reads MERGED, then run `loopherd feed loopherd-cards`.
- **On your watch's `grill-<n> merged`** with no line from the grill, or the
  human reports the merge with no line from the grill or note from the lead:
  find the branch in grill-<n>'s row under
  loopherd-cards in `loopherd status` (or `git worktree list | grep LH-<n>`) and
  its PR with
  `GH_TOKEN=$(gh auth token --user junderdo) gh pr list --head <branch> --state all`.
  Check `gh pr view <pr> --json state` (as junderdo) reads MERGED, then move
  the card to Done, run `scripts/trello unblock`, then
  `loopherd feed loopherd-cards`.

- **grill-<n> gone from `loopherd status`** with no line from the grill or
  note from the lead (a lead prompt without the D145 note, or a pane closed
  before its close-out): check its PR as above. When it reads MERGED and the
  card isn't in Done, close out as in the bullet above; when the card is in
  Done, the lead already did.

Check grill-<n>'s row in `loopherd status` on each watch wake until one of
these closes it out. Neither path needs the tab ID: the reap finds grill-<n>'s tab from its record.
If the PR reads anything but MERGED, close nothing out.

Closing a grill tab by hand (`herdr tab close`) while the human is in it
lets herdr focus the next tab, often a worker's. Focus your own pane first:
`herdr agent focus "$HERDR_PANE_ID"`. The reap does this itself.

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
starts it sandboxed with them as warnings; doctor reports them. A lead's
args are allowlisted the same way (D100); under `require`, a lead started
with an arg now off the list can't be restarted or revived until the human
retires it and runs `loopherd lead` again. A lead
gets no MCP servers but those its `[lead] mcp_servers` names, which run
unsandboxed; only the human adds one. A lead gets no Unix sockets, so no
herdr socket, ssh agent or WSL2 interop, unless its herd sets
`[lead] unix_sockets = "all"`, which `lead`, `revive` and doctor warn of
(D105). Beside `merge_guard = "credentials"` (D114), `lead`, `revive` and
`restart` refuse it, `herd set` and `herd unset` refuse an edit that makes
the pair, and doctor fails the herd. Where the sandbox can't block them (no seccomp filter; doctor
names the npm package), `require` refuses the lead and `prefer` starts it
as `(lead, sockets open)`; `restart` applies the same rule. A `credentials`
lead is refused under both. Under `unix_sockets = "all"` the socket check
is skipped: the lead isn't refused or flagged for open sockets, though
`require` still refuses it when the sandbox can't start at all. Installing
the package is the human's. What still gets past the sandbox: workers run
unsandboxed unless `[worker] sandbox` turns theirs on (LH-284); a
`credentials` lead starts only with `[worker] sandbox = "require"` (D131), and most of the state dir is writable (LH-298). A lead's
`tell`, `read`, `status`, `feed`, `add`, `prompt`, `restart`, `retire`,
`release` and `gate` run in its broker, outside the sandbox and only on its own herd
(D98). A lead gates its own merges: `loopherd gate <sha>` runs the herd
file's `[lead] gate` commands on that head merged with the base (D149), so
socket tests pass without `unix_sockets = "all"` and you don't gate for it; its watch still calls herdr itself, run bare outside the sandbox
(D104); doctor fails for a lead with no broker running, and
`loopherd revive --name <lead>` from an unsandboxed shell starts it. Doctor
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
