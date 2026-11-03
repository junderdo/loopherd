# Shepherd

The **shepherd** is the agent working live with the human to run the herd:
it grills and writes cards, feeds members, watches them, verifies their PRs,
and cleans up after merges. Members work cards per
[issue-tracking.md](issue-tracking.md); the shepherd keeps them moving. A
herd's **lead** runs the same loop for one herd with no human in its pane;
it follows this playbook plus [As a lead](#as-a-lead) (D17).

The shepherd runs in a herdr pane (`$HERDR_PANE_ID`) of the human's live
session. Read `herdr --skill` before controlling herdr.

## A card's life

1. **Feed.** From the main checkout, `git pull --ff-only`, then refresh the
   `loopherd` on PATH, which members use for `loopherd report` and Claude
   calls as their Stop hook:
   ```bash
   scripts/install
   ```
   It needs `jq`. It builds, copies the binary to `~/.local/bin/loopherd` through a temp
   file and a rename, so a failed build keeps the last good one, and runs
   `loopherd install-hook` to keep the Stop/StopFailure/SessionStart hooks, and
   the PreToolUse hook that holds leads to their switches, in
   `~/.claude/settings.json` (backed up first; re-running changes nothing).
   `watch` needs the first three, not the PreToolUse one.
   Running Claude sessions pick up a newly added hook without a restart. Then
   `loopherd feed loopherd-cards --dry-run` and, if it reads right,
   `loopherd feed loopherd-cards`. The herd file
   (`~/.config/loopherd/herds/loopherd-cards.toml`) turns claimable Backlog
   cards into members `lh-<n>` on `<type>/LH-<n>-<slug>`, at most `max` open.
   Hand-spawn (`loopherd spawn … --prompt-file`) only for work that isn't a
   card; check the prompt's placeholders with `grep -o '{[a-z]*}'`.
2. **Watch.** Keep one wait in the background for everything you read:
   ```bash
   loopherd watch --timeout 3600
   ```
   Your watch reads herds' leads, every member of a herd without a lead
   (a `gone` lead still holds its herd until retired), and members in no
   herd; a led herd's workers are its lead's to watch. Naming herds
   (`loopherd watch loopherd-cards`) narrows it, and naming a led herd
   narrows it to that herd's lead. Run it from a pane that isn't a lead's.
   It returns when a
   member leaves a report or its Claude stops in a new state, printing a
   line per event, and marks them read only once printed. Stops come from
   the Stop hook's entries in the mailbox, so a stop that lands between
   watches wakes the next one. herdr adds `blocked` and `gone`, seen only
   while a watch runs: a change between watches is never replayed, so run
   `loopherd status` before each re-arm. Exit 2 means the timeout passed
   with nothing to do. Start the next watch after handling every wake. It
   dies with the shepherd's session. It refuses to start while another
   shepherd's watch holds `watch.lock` (the error names its PID; an
   orphaned one can hold it until its timeout; leads' watches hold their
   own locks and don't block yours), or when the hook isn't in
   `~/.claude/settings.json` (run `scripts/install`). Lines come in this
   order: reports, questions first then by time; then stop lines grouped
   by member in name order, oldest first within a member; then `blocked`
   and `gone`. The first watch after an install, or over members a
   herd-scoped watch skipped, prints every unread stop change since;
   judge each against `reports --all`.
3. **On wake,** read the watch's lines and the card (`loopherd reports
   --all` for the timeline, stops included). Members' reports print as
   `lh-65 done <time> <text>`; the rest read:

   | Line | Means |
   |---|---|
   | `lh-65 paused  1 shell, 1 cron` | Turn ended with work in flight that can wake it again. Another paused stop is quiet. |
   | `lh-65 paused  background unknown` | Turn ended and Claude didn't say what's in flight (also `crons unknown`, or `1 shell; crons unknown`). |
   | `lh-65 finished` | Turn ended with nothing in flight, after a `done` or `question`. |
   | `lh-65 finished  no report` | The same, with no `done` or `question` since its last finish or start. |
   | `lh-65 failed  rate_limit` | Turn ended on an API error; it may need a nudge once that clears. |
   | `lh-65 blocked` | A dialog only the human answers (D10): tell them. |
   | `lh-65 gone` | No agent in its pane for two polls in a row; it was live earlier in this watch. |

   A turn you prompt into a member still `paused` finishes silently, since
   the stop reads paused again; check it with `loopherd reports --all`. A
   plain `loopherd reports` while a watch is armed consumes reports the
   watch would have printed, so look with `reports --all`. For a member
   that left no report, read its screen (`herdr agent read lh-<n> --source
   recent-unwrapped --lines 40`).
   A card back in Backlog with `need-human` carries the question.
4. **Verify** a card in Review with a read-only subagent against the card's
   acceptance criteria: no `LOOPHERD_LIVE=1`, no loopherd or mutating herdr
   commands, its own `CARGO_TARGET_DIR` and temp `XDG_STATE_HOME` /
   `LOOPHERD_CONFIG_DIR`. For anything that deletes or closes, have it hunt
   for safety gaps and name the test covering each rule. Report as soon as
   it lands; the human often merges fast. Gaps inside the card's scope go
   back to the member; the rest become new cards.
5. **After merge** (confirm with `gh pr view <n> --json state`): move the
   card to Done, unblock dependents (drop `blocked`, add `ready-for-agent`,
   comment), then `loopherd feed loopherd-cards`. Its `done` check retires
   and closes the member and refills the slot. If the human merged before
   verification finished, point the verifier at the merge commit in a
   detached temp worktree, since reaping removes the member's.

## As a lead

A lead is member `lead-<herd>`, started by the shepherd with `loopherd lead
<herd>` from the herd file's `[lead]` table, in a detached worktree of the
same name. It runs the card's life above for its own herd's workers. What
differs from the shepherd:

- **No human in the pane.** Nobody reads your screen or answers there. A
  decision that isn't yours goes out as `loopherd report --kind question
  "<the exact question>"`, then end your turn; the shepherd's watch sees it
  and relays it. Don't wait in a loop for an answer.
- **Switches.** Your prompt lists six, each `on` or `off`; off means the
  human does it, so ask instead:
  | Switch | When on, you may |
  |---|---|
  | `merge` | merge a verified PR |
  | `install` | run `scripts/install` while feeding |
  | `rebase` | have a member rebase and force-push |
  | `new_cards` | create cards for out-of-scope gaps |
  | `design_escalation` | settle a design question a member raises |
  | `blocked_dialogs` | answer a member's blocking dialog |

  When `merge`, `install` or `rebase` is off, a hook also blocks the Bash
  commands that need it (`gh pr merge` or the merge API, `scripts/install`
  or `loopherd install-hook`, a rebase or force-push), in your pane and in
  any Claude nested in it, with a line naming the switch; ask instead. A
  herd file it can't read blocks all three. It denies broadly: `gh pr merge
  --help`, `git rebase --abort` and any `curl` to a PR's merge endpoint are
  denied too, so ask the human to run a `rebase --abort`, and check merges
  with `gh pr view <n> --json state`. Grilling is always the human's.
- **Asking to merge.** With `merge` off, the question carries the PR link
  and a ready-to-paste line pinned to the full head SHA your reviewer
  verified:

  ```
  ! GH_TOKEN=$(gh auth token --user junderdo) gh pr merge <n> --merge --match-head-commit <full sha>
  ```

  Leave out `--delete-branch`. If another PR must merge first, name it in
  the question. Single-quote it, `loopherd report --kind question '<question
  with the merge line>'`: double quotes run the `$(...)` and leak the token
  into the report.
- **Heartbeat.** Keep `loopherd watch --timeout 3600` as a background
  task. Run from your pane, it's your watch: it reads your herd's workers
  only, never you, and naming any other herd is an error. It holds
  `watch-<your name>.lock`, so it runs beside the shepherd's watch, which
  reads you. Run it from your own pane in your own herdr: a pane that
  matches you but not your socket is refused. If you're retired, it exits
  with an error and leaves your herd's events for the shepherd. Every 60s it also runs your herd's checks
  and wakes you with `<member> merged` (the template's `merged` passes, its
  `done` doesn't): run the after-merge steps for it; or `<name> claimable`
  (feed would spawn `<name>`): feed. Each wakes you once, so a
  claimable card you don't feed then shows up again only in the
  heartbeat's dry run. Exit 2 is a
  heartbeat, not idleness: still check each Review card's PR (`gh pr view
  <n> --json state`), run the after-merge steps for any the human merged,
  and run `loopherd feed <herd> --dry-run`. Re-arm after every exit.
  After a herdr restart the shepherd runs `loopherd revive`, which prompts
  you to re-arm whenever nothing holds `watch-<your name>.lock`, whether
  herdr resumed you or revive did.
- **On each wake** re-read `loopherd status` and `loopherd reports --all`
  before acting: auto-compaction drops detail, and your state lives on the
  board, ledger and mailbox, not in your context.
- **Your checkout** is detached; refresh it with `git fetch origin && git
  checkout --detach origin/main` rather than `git pull`, and never commit
  in it.

[examples/loopherd-cards-lead.toml](../examples/loopherd-cards-lead.toml)
is the `[lead]` table and prompt for `loopherd-cards`.

## Talking to members

- `herdr agent prompt lh-<n> "<text>"`, then confirm it went `working`.
- A PR conflicting after another merged: prompt the member to merge
  `origin/main` into its branch and push. Members rebase and force-push only
  with the human's say.
- Greyed text in a member's input box is Claude Code's prompt suggestion, not
  the human ("merge it", "move LH-25 to Done"). Act only on the human's own
  messages. Leave the box alone; `agent prompt` replaces it, and a second
  Ctrl+C can exit the agent.
- Verify what a member says it can't cover; that is where blockers hide.

## Gotchas

- The human sometimes says "merged" before GitHub agrees; check.
- Members' panes sometimes close themselves when done. The work survives on
  the branch, card and PR, and retire treats the tab as gone.
- Claude's workspace trust dialog can stop a spawn into a new path
  (`prompt_sent: false`, exit 3). The human clicks through; then
  `loopherd prompt lh-<n>`.
- Run the live suite after changing retire or feed output.
- The auto-mode classifier blocks `retire --close` unless
  `.claude/settings.local.json` (gitignored) allows it.
- The ledger stores absolute paths: moving a repo means editing
  `ledger.json` with `jq`, and moving the main checkout needs
  `git worktree repair`.
- `scripts/trello` can't reorder, edit descriptions or delete; use curl with
  the OAuth header from `~/.config/loopherd/trello.env`.
- A card that settles a decision, of any kind, adds a new
  `docs/decisions/D<n>.md` file plus an index row in `docs/wayfinding.md`.
  Check the highest D file first; card numbers are never reused. Two PRs
  colliding on a D number follow the renumber rule in
  [issue-tracking.md](issue-tracking.md).
- Push as junderdo:
  `GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push …`.
