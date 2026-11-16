# Shepherd runbook: loopherd-cards

Read `loopherd --skill` first: it is the shepherd's and lead's guide for any
herd (D19). This file adds only what's specific to this repo's herd,
`loopherd-cards`, whose members work Trello cards per
[issue-tracker.md](issue-tracker.md). Setting up a herd in another repo
after `loopherd init` is the skill's "Setting up a project".

## Feed

From the main checkout, `git pull --ff-only`, then refresh the `loopherd` on
PATH:

```bash
scripts/install
```

It needs `jq`. It builds, copies the binary to `~/.local/bin/loopherd` through
a temp file and a rename, so a failed build keeps the last good one, and runs
`loopherd install-hook`, which installs the hooks and the skill. Then:

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
  finish the steps by hand.

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
