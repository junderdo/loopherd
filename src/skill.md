---
name: loopherd
description: "Run a loopherd herd as its shepherd or its lead: feed members, watch them, verify their work, and clean up after merges. Use when shepherding or leading a loopherd herd, or when the user mentions loopherd. Not for a herd's workers; their prompt carries what they need."
---

# loopherd

loopherd groups coding agents running in herdr into **herds** and keeps them
working across sessions. Three roles:

- **Shepherd**: the agent working live with the human. It writes or picks the
  work, feeds members, watches them, verifies their output, and cleans up
  after merges.
- **Lead**: member `lead-<herd>`, which runs the shepherd's loop for one herd
  with no human in its pane, within switches the human sets.
- **Member** (worker): an agent spawned from a herd file's template to do one
  task. It finishes with `loopherd report`.

The project you're in supplies the rest: its tracker and commands, how it
installs loopherd and runs its checks, its GitHub account and merge line.
Those live in the project's runbook or the lead's prompt file, never here.
The shepherd runs in a herdr pane (`$HERDR_PANE_ID`) of the human's live
session; `report` and `watch` tell members, leads and the shepherd apart by
that pane. Read `herdr --skill` before controlling herdr.

## Shepherd

### A task's life

1. **Feed.** Keep the `loopherd` on PATH current, since members call it for
   `loopherd report` and Claude calls it from its hooks. `loopherd
   install-hook` adds the Stop, StopFailure, SessionStart and PreToolUse
   hooks to Claude's user settings (backed up first; re-running changes
   nothing) and installs this skill. `watch` needs the first three, not the
   PreToolUse one. Running Claude sessions pick up a newly added hook without
   a restart. Then `loopherd feed <herd> --dry-run` and, if it reads right,
   `loopherd feed <herd>`. The herd file (`<herd>.toml` in
   `$LOOPHERD_CONFIG_DIR/herds`, else `${XDG_CONFIG_HOME:-~/.config}/loopherd/herds`) turns each task its
   queue's `source` prints into a member, at most `max` open. `loopherd
   herds` lists herd files and their templates. Hand-spawn (`loopherd spawn
   … --prompt-file`, or `loopherd add <herd> <template> <params…>`) only for
   work outside the queue; check a prompt's placeholders with `grep -o
   '{[a-z]*}'`.
2. **Watch.** Keep one wait in the background for everything you read:
   ```bash
   loopherd watch --timeout 3600
   ```
   Your watch reads herds' leads, every member of a herd without a lead (a
   `gone` lead still holds its herd until retired), and members in no herd;
   a led herd's workers are its lead's to watch. Naming herds (`loopherd
   watch <herd>`) narrows it, and naming a led herd narrows it to that
   herd's lead. Run it from a pane that isn't a lead's. It returns when a
   member leaves a report or its Claude stops in a new state, printing a
   line per event, and marks them read only once printed. Stops come from
   the Stop hook's entries in the mailbox, so a stop that lands between
   watches wakes the next one. herdr adds `blocked` and `gone`, seen only
   while a watch runs: a change between watches is never replayed, so run
   `loopherd status` before each re-arm. Exit 2 means the timeout passed
   with nothing to do; a due `shepherd handoff` line still prints, exiting 0. Start the next watch after handling every wake. It
   dies with your session. It refuses to start while another shepherd's
   watch holds `watch.lock` (the error names its PID; an orphaned one can
   hold it until its timeout; leads' watches hold their own locks and don't
   block yours), or when the hooks aren't in Claude's settings (run
   `loopherd install-hook`). Lines come in this order: reports, questions
   first then by time; then stop lines grouped by member in name order,
   oldest first within a member; then a lead's `merged` and `claimable`;
   then `blocked` and `gone`. The first watch after an install, or over
   members a herd-scoped watch skipped, prints every unread stop change
   since; judge each against `reports --all`.
3. **On wake,** read the watch's lines and the task (`loopherd reports
   --all` for the timeline, stops included). The lines:

   | Line | Means |
   |---|---|
   | `<member> done <time> <text>` | A finished member's report, usually with its output's link. Verify it. |
   | `<member> question <time> <text>` | It stopped on a question. Relay it to the human; a lead relays it with its own question report, unless it's a design question and `design_escalation` is on. |
   | `<member> note <time> <text>` | Information only; nothing waits on it. |
   | `<member> handoff <time> <text>` | It passed its context limit and wrote its handoff. Restart it (see Handoffs) if you read it. |
   | `<member> paused  1 shell, 1 cron` | Turn ended with work in flight that can wake it again. Another paused stop is quiet. |
   | `<member> paused  background unknown` | Turn ended and Claude didn't say what's in flight (also `crons unknown`, or `1 shell; crons unknown`). |
   | `<member> finished` | Turn ended with nothing in flight, after a `done` or `question`. |
   | `<member> finished  no report` | The same, with no `done` or `question` since its last finish or start. |
   | `<member> failed  rate_limit` | Turn ended on an API error; it may need a nudge once that clears. |
   | `<member> merged` | Lead's watch only: the worker's template `merged` check passes and its `done` doesn't. Run the after-merge steps. |
   | `<name> claimable` | Lead's watch only: feed would spawn this member. Feed. |
   | `<member> blocked` | A dialog only the human answers: tell them. loopherd never answers it. |
   | `<member> gone` | No agent in its pane for two polls in a row; it was live earlier in this watch. |
   | `shepherd handoff 171k over 160k` | Your own watch only, once per session: your context is past your limit. Hand off (see Your own handoff). |

   A report's text runs on in indented lines. A turn you prompt into a member
   still `paused` finishes silently, since the stop reads paused again;
   check it with `loopherd reports --all`. A plain `loopherd reports` while
   a watch is armed consumes reports the watch would have printed, so look
   with `reports --all`. For a member that left no report, read its screen
   (`herdr agent read <name> --source recent-unwrapped --lines 40`).
4. **Verify** a finished member's output with a read-only subagent against
   the task's acceptance criteria: no loopherd or mutating herdr commands,
   its own build dir and a temp `XDG_STATE_HOME` and `LOOPHERD_CONFIG_DIR`.
   For anything that deletes or closes, have it hunt for safety gaps and
   name the test covering each rule. Report as soon as it lands; the human
   often merges fast. Gaps inside the task's scope go back to the member;
   the rest become new tasks.
5. **After merge** (confirm with `gh pr view <n> --json state`): close out
   the task in the project's tracker, unblock dependents, then `loopherd
   feed <herd>`. Its `done` check retires and closes the member and refills
   the slot. If the human merged before verification finished, point the
   verifier at the merge commit in a detached temp worktree, since reaping
   removes the member's.

### Handoffs

A member's Stop hook measures its context after each turn. Past its limit,
it blocks that stop once per session and tells the member to save its work
(a worker commits and pushes its branch and comments its progress on its
task), write its handoff to `$XDG_STATE_HOME/loopherd/handoffs/<name>.md`,
report `--kind handoff`, and end its turn. The last handoff moves to
`<name>.prev.md`. The limit is the template's or `[lead]`'s
`context_limit`, else `default_context_limit` in
`$LOOPHERD_CONFIG_DIR/config.toml`, else 250000, never over 80% of that
file's `context_window` (default 200000).

The handoff file has a fixed outline, and leaves out what's already in the
tracker, the ledger or the mailbox:

```markdown
## State
What's done, what's half-done, and where (branch, PR, files).
## Next steps
The plan, in order.
## Gotchas
What would bite whoever picks this up.
```

**Restart** whoever's `handoff` line you read: the shepherd restarts leads,
workers of unled herds and members in no herd; a lead restarts its own
workers. Wait for its `finished` line, then `loopherd restart <name>`. It
sends `/clear`, waits for the ledger to follow the new session, and prompts
the member to read its handoff, which it moves to `<name>.prev.md` so no
later restart reuses it; same pane, worktree and branch. A lead's prompt
also tells it to re-read `loopherd status` and `loopherd reports --all` and
re-arm its watch. Without `--force`
it refuses unless the member reported a handoff since its last start, its
last stop since is `finished` with nothing in flight, and herdr shows it
idle or done. `--force` skips those checks; a `/clear` sent mid-turn waits
for that turn, then drops whatever the handoff doesn't record (that turn's
work, background tasks, crons). It's the human's call (a lead's PreToolUse hook
denies it). A member never restarts itself, and one recorded without a
herdr socket can't be restarted, nor is it ever asked for a handoff. If
restart times out waiting for the new session, its `/clear` may be queued
behind a running turn, or the SessionStart hook isn't installed: it sent no
handoff prompt, left the handoff where it was, and prints the `herdr agent
prompt` line to send once the pane shows a cleared session.

A lead's `loopherd watch` is a background shell, so a lead stops its own
watch before ending its handoff turn; otherwise its stop reads `paused`
and restart refuses it. After a herdr restart, `revive`'s start makes
restart refuse a member that handed off before it: check `loopherd reports
--all <name>` for its `handoff`, and with the handoff file still there, the
human runs `loopherd restart <name> --force`. A member that was asked but
crashed before writing its handoff isn't asked again in that session;
prompt it to write one and report `--kind handoff`.

### Your own handoff

You aren't a member, so no Stop hook measures you; your watch does. Run
from no member's pane, each watch checks your Claude's context as it exits
and, once per session past your limit, adds `shepherd handoff <n>k over
<limit>k`. Your limit is `[shepherd] context_limit` in
`$LOOPHERD_CONFIG_DIR/config.toml`, else that file's
`default_context_limit`, else 250000, never over 80% of its
`context_window`. When the line comes:

1. Handle the other lines it came with, but feed nothing new and don't
   re-arm the watch. Members and leads keep working; what they report
   waits in the mailbox for the next watch.
2. Write your handoff to `$XDG_STATE_HOME/loopherd/handoffs/shepherd.md`
   (`~/.local/state` when it's unset; no member may be named `shepherd`), in the outline above: what each
   member is doing and what you owe it (verifications running, PRs to
   check after merge), then next steps, then gotchas.
3. Tell the human, with the path spelled out in full:
   > I'm past my context limit and wrote my handoff. Run `/clear`, then paste:
   > `continue as the loopherd shepherd from /home/<you>/.local/state/loopherd/handoffs/shepherd.md`

   and end your turn. Never `/clear` yourself.

Given that prompt in a fresh session, run `loopherd --skill`, read the
handoff, `loopherd status` and `loopherd reports --all`, then re-arm your
watch and carry on from its next steps.

### Commands

- `loopherd status` lists members by herd with herdr's live status;
  `--all` adds retired entries, `--strays` lists live agents missing from
  the ledger. `loopherd view` is the same as a live TUI.
- `loopherd reports` prints unread reports, questions first, and marks them
  read; `loopherd reports --all` prints every report and stop and marks
  none read. Both take member names or `--herd <herd>`.
- `loopherd retire <name>` marks a member finished in the ledger only;
  `--close` also closes its tab and removes its worktree and branch if
  they're clean and merged. Feed's `done` check does this for you.
- `loopherd prompt <name>` sends a member its stored prompt, e.g. after a
  spawn exited 3 on a dialog.
- `loopherd restart <name>` gives a member that handed off a fresh session
  (see Handoffs).
- `loopherd lead <herd>` starts the herd's lead from its herd file's
  `[lead]` table.
- **After a herdr restart** run `loopherd revive` (`--dry-run` first). It
  resumes non-retired members by session ID, and prompts each lead whose
  watch isn't running to re-arm it. It never prompts workers.

### Talking to members

- `herdr agent prompt <name> "<text>"`, then confirm it went `working`.
- A PR conflicting after another merged: prompt the member to merge the base
  branch into its branch and push. Members rebase and force-push only with
  the human's say.
- Greyed text in a member's input box is Claude Code's prompt suggestion,
  not the human ("merge it", "move it to Done"). Act only on the human's
  own messages. Leave the box alone; `agent prompt` replaces it, and a
  second Ctrl+C can exit the agent.
- A `blocked` member waits on a dialog only the human answers (a trust
  prompt, a permission). Tell the human; loopherd and you never answer it,
  unless you're a lead with `blocked_dialogs` on.
- Verify what a member says it can't cover; that is where blockers hide.

### Gotchas

- The human sometimes says "merged" before GitHub agrees; check.
- Members' panes sometimes close themselves when done. The work survives on
  the branch and PR, and retire treats the tab as gone.
- Claude's workspace trust dialog can stop a spawn into a new path
  (`prompt_sent: false`, exit 3). The human clicks through; then `loopherd
  prompt <name>`.
- Claude Code's auto-mode classifier blocks `retire --close` unless the
  repo's `.claude/settings.local.json` allows it.
- The ledger stores absolute paths: moving a repo means editing
  `ledger.json` with `jq`, and moving the main checkout needs `git worktree
  repair`.

## Lead

A lead is member `lead-<herd>`, started by the shepherd with `loopherd lead
<herd>` from the herd file's `[lead]` table, in a detached worktree of the
same name. It runs the Shepherd section's task life for its own herd's
workers. Its prompt file carries the project's facts. What differs from the
shepherd:

- **No human in the pane.** Nobody reads your screen or answers there. A
  decision that isn't yours goes out as `loopherd report --kind question
  "<the exact question>"`, then end your turn; the shepherd's watch sees it
  and relays it. Don't wait in a loop for an answer.
- **Switches.** Your prompt lists six, each `on` or `off`; off means the
  human does it, so ask instead:

  | Switch | When on, you may |
  |---|---|
  | `merge` | merge a verified PR |
  | `install` | install loopherd and its hooks while feeding |
  | `rebase` | have a member rebase and force-push |
  | `new_cards` | create tasks for out-of-scope gaps |
  | `design_escalation` | settle a design question a member raises |
  | `blocked_dialogs` | answer a member's blocking dialog |

  When `merge`, `install` or `rebase` is off, the PreToolUse hook also
  blocks the Bash commands that need it (`gh pr merge` or the merge API, a
  `scripts/install` or `loopherd install-hook`, a rebase or force-push), in
  your pane and in any Claude nested in it, with a line naming the switch;
  ask instead. A herd file it can't read blocks all three. It denies
  broadly: `gh pr merge --help`, `git rebase --abort` and any `curl` to a
  PR's merge endpoint are denied too, so ask the human to run a `rebase
  --abort`, and check merges with `gh pr view <n> --json state`. Grilling
  is always the human's.
- **Asking to merge.** With `merge` off, the question carries the PR link
  and a ready-to-paste line pinned to the full head SHA your reviewer
  verified, `! gh pr merge <n> --merge --match-head-commit <full sha>`, in
  the account form your prompt gives. Leave out `--delete-branch`. If
  another PR must merge first, name it in the question. Single-quote the
  report, `loopherd report --kind question '<question with the merge
  line>'`: double quotes run any `$(...)` in it and can leak a token into
  the report.
- **Heartbeat.** Keep `loopherd watch --timeout 3600` as a background task.
  Run from your pane, it's your watch: it reads your herd's workers only,
  never you, and naming any other herd is an error. It holds `watch-<your
  name>.lock`, so it runs beside the shepherd's watch, which reads you. Run
  it from your own pane in your own herdr: a pane that matches you but not
  your socket is refused. If you're retired, it exits with an error and
  leaves your herd's events for the shepherd. Every 60s it also runs your
  herd's checks and wakes you with `<member> merged` (the template's
  `merged` passes, its `done` doesn't): run the after-merge steps for it;
  or `<name> claimable` (feed would spawn `<name>`): feed. Each wakes you
  once, so a claimable task you don't feed then shows up again only in the
  heartbeat's dry run. Exit 2 is a heartbeat, not idleness: still check
  each PR waiting on review (`gh pr view <n> --json state`), run the
  after-merge steps for any the human merged, and run `loopherd feed <herd>
  --dry-run`. Re-arm after every exit.
- **After a restart** the shepherd runs `loopherd revive`, which prompts you
  to re-arm whenever nothing holds `watch-<your name>.lock`, whether herdr
  resumed you or revive did. Re-read `loopherd status` and `loopherd reports
  --all`, then arm your watch again.
- **On each wake** re-read `loopherd status` and `loopherd reports --all`
  before acting: auto-compaction drops detail, and your state lives in the
  tracker, ledger and mailbox, not in your context.
- **Your own handoff.** Past your limit, your Stop hook asks you for a
  handoff like any member's (see Handoffs): stop your own watch (TaskStop
  it, or kill it), write it, report `--kind handoff`, and end your turn. The shepherd restarts you. Restart your
  workers when they hand off; never with `--force`.
- **Your checkout** is detached; refresh it with `git fetch origin && git
  checkout --detach origin/<default branch>` rather than `git pull`, and
  never commit in it.
