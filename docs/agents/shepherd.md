# Shepherd

The **shepherd** is the agent working live with the human to run the herd:
it grills and writes cards, feeds members, watches them, verifies their PRs,
and cleans up after merges. Members work cards per
[issue-tracking.md](issue-tracking.md); the shepherd keeps them moving. (Slice
6's *lead* is a different thing: an agent loopherd itself would run.)

The shepherd runs in a herdr pane (`$HERDR_PANE_ID`) of the human's live
session. Read `herdr --skill` before controlling herdr.

## A card's life

1. **Feed.** From the main checkout, `git pull --ff-only`, then refresh the
   `loopherd` on PATH, which members use for `loopherd report`:
   ```bash
   cargo build -q && install -m 755 target/debug/loopherd ~/.local/bin/.loopherd.tmp \
     && mv -f ~/.local/bin/.loopherd.tmp ~/.local/bin/loopherd
   ```
   It's a copy, not a symlink, so a failed build keeps the last good one, and
   the rename swaps it in one step. Then
   `loopherd feed loopherd-cards --dry-run` and, if it reads right,
   `loopherd feed loopherd-cards`. The herd file
   (`~/.config/loopherd/herds/loopherd-cards.toml`) turns claimable Backlog
   cards into members `lh-<n>` on `<type>/LH-<n>-<slug>`, at most `max` open.
   Hand-spawn (`loopherd spawn … --prompt-file`) only for work that isn't a
   card; check the prompt's placeholders with `grep -o '{[a-z]*}'`.
2. **Watch.** Keep one wait in the background for the whole herd:
   ```bash
   loopherd watch --timeout 3600
   ```
   (`loopherd watch loopherd-cards` narrows it to a herd.) It returns when a
   member leaves a report, goes `blocked`, or goes from `working` to `done`,
   `idle` or `gone` (its pane disappeared), printing a line per event and
   marking the reports it prints read. Only changes wake it: a member
   already `blocked` when the watch starts wakes it only by a report, so on
   starting a session run `loopherd status` first to catch members already
   blocked. A stop recorded since the last watch judged it still wakes the
   next one, so a member that finished between watches isn't lost. Exit 2
   means the timeout passed with nothing to do. Start the next watch after
   handling every wake. It dies with the shepherd's session.
   A line ending `(paused: 1 shell)` means the member stopped with
   background work still running (shells, subagents, crons), so it isn't
   finished: leave it. It stays quiet while it remains paused, so a `/loop`
   member wakes the watch once, not every turn. Its real finish wakes a
   later watch as `(was working…)`. `background unknown` in that line means
   the stop didn't say what was in flight. Only members loopherd started
   (`spawn`, `add`, `feed`, `revive`) have the Stop hook. One started before
   the hook existed, or one herdr auto-resumed after a server restart
   (herdr relaunches it as bare `claude --resume <id>`), never shows as
   paused.
3. **On wake,** read the watch's lines and the card (`loopherd reports
   --all` for report history). `(was working, no report)` means the watch
   read no `done` or `question` from that member in the poll where it
   stopped. A report an earlier watch printed doesn't count, so check the
   history before calling it a false stop. A plain `loopherd reports` while
   a watch is armed consumes reports the watch would have used, so look with
   `reports --all`. A member that left none: read
   its screen (`herdr agent read lh-<n> --source recent-unwrapped --lines 40`).
   `blocked` means a dialog only the human answers (D10): tell them.
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
- Check the highest D-row before adding a wayfinding decision. Card numbers
  are never reused.
- Push as junderdo:
  `GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push …`.
