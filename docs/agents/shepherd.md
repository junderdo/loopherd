# Shepherd runbook: loopherd-cards

Read `loopherd --skill` first: it is the shepherd's and lead's guide for any
herd (D19). This file adds only what's specific to this repo's herd,
`loopherd-cards`, whose members work Trello cards per
[issue-tracking.md](issue-tracking.md).

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
- **After merge:** move the card to Done, unblock dependents (drop `blocked`,
  add `ready-for-agent`, comment), then `loopherd feed loopherd-cards`.

## Lead

[examples/loopherd-cards-lead.toml](../examples/loopherd-cards-lead.toml) is
the `[lead]` table and prompt for `loopherd-cards`. Its merge line, for the
human to paste:

```
! GH_TOKEN=$(gh auth token --user junderdo) gh pr merge <n> --merge --match-head-commit <full sha>
```

## Gotchas

- Run the live suite after changing retire or feed output.
- `scripts/trello` can't reorder, edit descriptions or delete; use curl with
  the OAuth header from `~/.config/loopherd/trello.env`.
- A card that settles a decision, of any kind, adds a new
  `docs/decisions/D<n>.md` file plus an index row in `docs/wayfinding.md`.
  Check the highest D file first; card numbers are never reused. Two PRs
  colliding on a D number follow the renumber rule in
  [issue-tracking.md](issue-tracking.md).
- Push as junderdo:
  `GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push …`.
