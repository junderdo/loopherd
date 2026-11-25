# Writing a shepherd runbook

The loopherd skill (`loopherd --skill`) is the shepherd's guide for any
herd. A project's runbook, `docs/agents/shepherd.md` in its repo, holds only
what is specific to that project: its herd, tracker, install, checks and
accounts. `loopherd init` writes a stub; fill it in with the sections
below, keeping each to the facts a fresh shepherd would otherwise have to
ask the human for. Point the lead's prompt at it too (see
[herd-file.md](herd-file.md)).

Leave out anything the skill already says: how to feed, watch, verify and
retire members is the same in every project.

## Opening

One paragraph: the herd's name, what its members work (issues, cards,
PRs) and where the tracker's own guide lives. Say to read
`loopherd --skill` first.

## Each session

What the shepherd does at the start of every session, after the skill's
`loopherd doctor`: anything the project adds, such as a status command or
a dashboard to check.

## Feed

- How the project installs and updates loopherd (`loopherd update`, or
  its own script), and what to restart after.
- The feed commands with the herd's name filled in:

  ```bash
  loopherd feed <herd> --dry-run
  loopherd feed <herd>
  ```

- Which tasks the queue turns into members, and the member and branch
  names they get.

## Tasks through the loop

The life of one task in this project's tracker, from claimable to closed:

- Where a member's finished work shows up (a PR, a branch) and the state
  the task sits in meanwhile.
- How to verify it: the project's checks, and any limits on a review
  subagent (no live tests, its own scratch dir).
- **After merge:** the exact steps to close the task out (move it, unblock
  what waited on it, pull, reinstall), and which of them the herd file's
  `after_merge` already runs.
- Triage: the commands that mark a backlog task ready, ask for more spec,
  or close a stale one, with their quoting rules.

## Lead

When the herd has a lead:

- Where its `[lead]` table and prompt file live, and which switches the
  human has turned on.
- The merge line the human pastes, with the project's account and flags:

  ```text
  ! gh pr merge <n> --merge --match-head-commit <full sha>
  ```

- Anything the lead can't do from its sandbox that falls to the shepherd.

## Gotchas

Short bullets for what has bitten a shepherd here before: an account to
push as, a test suite to run after a certain change, a command the tracker's
tool can't do and its workaround.
