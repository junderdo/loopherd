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

At the start of each session, run `loopherd doctor` (with `--settings
<file>` if you installed with it). It prints only problems; relay each
failure to the human with the fix it names (D63).

### Setting up a project

`loopherd init` writes a herd's structure; you tailor its prose to the repo.
Init never launches an agent.

1. Run `loopherd init` in the repo, or check it already ran: `loopherd
   herds` lists the herd file it wrote.
2. Read the repo's `AGENTS.md`, `docs/agents/issue-tracker.md` and
   `docs/agents/loopherd.md`.
3. If the tracker is neither GitHub nor local markdown, init left `[queue]
   source` empty. Fill it from the tracker doc with a command printing one
   task per line, its words in the template's `params` order. Then fill
   every other placeholder init marked `(fill in)` and listed in its
   steps: the template's `done` check, the worker prompt's steps that read,
   claim, escalate a question and close the task, the lead prompt's line, and `docs/agents/loopherd.md`.
   Fill `[queue] backlog` too, or drop it (see A task's life). Keep `merged` if
   init wrote one (the GitHub PR check); without it, the lead feeds after
   each merge.
4. Adjust the worker prompt and the checks it lists to how the repo builds,
   tests and reviews; for a herd with a lead, its prompt file too.
5. Edit only what init wrote: the herd file (with `loopherd herd set`), the
   lead's prompt file beside it, and the repo files init listed. Leave the
   rest of the repo and other herds alone.
6. You're done when `loopherd herds` calls the herd valid and
   `loopherd feed <herd> --dry-run` lists the tasks you expect from the
   tracker.
7. Have the human review and commit the repo files you and init changed.
8. Remind the human to open Claude in the repo once and accept its
   trust dialog, since members can't start past it, and that
   `loopherd lead <herd>` starts the herd's lead when they want one.

### A task's life

1. **Feed.** Keep the `loopherd` on PATH current, since members call it for
   `loopherd report` and Claude calls it from its hooks. `loopherd
   install-hook` adds the Stop, StopFailure, SessionStart, PreToolUse and
   UserPromptSubmit hooks to Claude's user settings (backed up first;
   re-running changes nothing) and installs this skill. `watch` needs the
   first three, not the last two. Running Claude sessions pick up a newly added hook without
   a restart. Then `loopherd feed <herd> --dry-run` and, if it reads right,
   `loopherd feed <herd>`. The herd file (`<herd>.toml` in
   `$LOOPHERD_CONFIG_DIR/herds`, else `${XDG_CONFIG_HOME:-~/.config}/loopherd/herds`) turns each task its
   queue's `source` prints into a member, at most `max` open. A `[queue]`
   with `templates = ["card", "wayfinding"]` in place of `template` routes
   each task: the line's first word names its template (one of the list),
   the rest are its params, and one `max` covers them all
   (D25). A member is known by its rendered name, so a task re-routed to
   another template while its first member is open gets a second member.
   `--dry-run` names each member's template, and `status --json` gives each member's `template`.
   A top-level `after_merge = ["<cmd>", …]` is run by feed in `repo`, in
   order and stopping at the first failure, once per member the first time
   its template's `merged` check passes, paused or not. The member's mailbox
   gets a `note` (`after_merge ok`, or the failing command, its exit and last
   20 lines), and its `done` check waits for a feed after that note (D28).
   It runs after feed releases its lock. If that feed dies first, the next
   feed notes it interrupted with no outcome, so run feed in the background
   when a herd has after_merge.
   An optional `[queue] backlog = "<cmd>"`, run in `repo` like `source`,
   prints one unready task per line (key, then title): every open task
   `source` leaves out but blocked ones, keeping those waiting on the human.
   It changes nothing feed does; `status` counts its lines (D60).
   `loopherd herds` lists herd files, their templates and `after_merge`. Hand-spawn (`loopherd spawn
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
   watches wakes the next one. herdr adds `blocked` and `gone`, which
   the herdr plugin's event hooks also record in the mailbox, so with the
   plugin installed a change between watches wakes the next one too; each
   prints once, whether a hook or a watch's poll saw it first. Without the
   plugin they're seen only while a watch runs. After a
   wake, run `loopherd status` and re-arm in the same message as your first
   action, unless the wake needs a handoff or says `all drained` (then
   don't re-arm). Read the
   watch's output once; don't read it again after acting. Exit 2 means the
   timeout passed with nothing to do; a due `shepherd handoff` line still
   prints, exiting 0. It
   dies with your session. It refuses to start while another shepherd's
   watch holds `watch.lock` (the error names its PID; an orphaned one can
   hold it until its timeout; leads' watches hold their own locks and don't
   block yours), or when the hooks aren't in Claude's settings (run
   `loopherd install-hook`). Lines come in this order: reports, questions
   first then by time; then stop lines grouped by member in name order,
   oldest first within a member; then a lead's `failing` and `recovered`,
   then its `running low`, then its `interrupted` and `merged`, by member
   in name order, then its `claimable`; then `blocked` and `gone`. A
   `drained` line prints only on a poll nothing else woke, alone but for a
   due `shepherd handoff`. The
   first watch after an install, or over members a herd-scoped watch
   skipped, prints every unread stop change since; judge each against
   `reports --all`.
3. **On wake,** read the watch's lines and the task (`loopherd reports
   --all --tail 40` for the timeline, stops included; for a fuller
   history, `--since <time>` or plain `--all`, or `--all <name>` for one
   member). The lines:

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
   | `<member> merged` | Lead's watch only: the worker's template `merged` check passes and its `done` doesn't, or its herd's after_merge is still due. Run the after-merge steps. |
   | `<name> claimable` | Lead's watch only: feed would spawn this member. Feed. |
   | `<herd> failing  source "<command>": <stderr line>` | Lead's watch only: the herd's source failed (also `<herd> failing  backlog "<command>": …` for its `backlog`, and `<herd> failing  <member> done "<command>": …` for a member's `done` or `merged` check), with its last stderr line. Once per streak; one cut off by the 60s round is no event. Fix the cause, or tell the shepherd. |
   | `<herd> recovered  source` | Lead's watch only: what last printed `failing` passes again (also `<herd> recovered  backlog` and `<herd> recovered  <member> done`). A later failure prints `failing` again. A member that retires mid-streak gets none. |
   | `<herd> running low  <n> claimable, <m> unready` | Lead's watch only: the herd's `backlog` lists unready tasks and claimable (as `status` counts it) is at or below `max`. Once per streak: it prints again only after claimable rises above `max` or unready reaches 0. A paused herd prints none and keeps its streak. Triage (see Lead). |
   | `<member> interrupted` | Lead's watch only: the feed running the worker's `after_merge` died before noting it. Feed, which notes it, then read the note. |
   | `<member> blocked` | A dialog only the human answers: tell them. loopherd never answers it. |
   | `<member> gone` | Its pane exited or closed, its tab or workspace closed, or it had no agent for two polls in a row after this watch saw it live. |
   | `<herd> drained` | Lead's watch only: your herd is paused and has no open (non-retired) worker. Report done, stop, and don't re-arm. |
   | `all drained` | Every herd your watch reads is paused with no open worker, and no member it reads may still work: none is working, blocked or unknown (now or on the poll before), died since the watch started without its `gone` line yet, or last stopped failed or with work in flight. Tell the human; don't re-arm. |
   | `shepherd handoff 171k over 160k` | Your own watch only, once per session: your context is past your limit. Hand off (see Your own handoff). |

   A report's text runs on in indented lines. A turn you prompt into a member
   still `paused` finishes silently, since the stop reads paused again;
   check it with `loopherd reports --all`. A plain `loopherd reports` while
   a watch is armed consumes reports the watch would have printed, so look
   with `reports --all`. For a member that left no report, read its screen
   (`herdr agent read <name> --source recent-unwrapped --lines 40`).
4. **Verify** a finished member's output with a read-only reviewer
   subagent against the task's acceptance criteria. Ask for it by role:
   the `reviewer` agent type, else `loopherd-reviewer`, else a
   general-purpose subagent told to stay read-only. A first review uses
   the reviewer as defined; later rounds pass `model: sonnet` to the
   subagent, since they only recheck findings and a small diff. Give it
   no loopherd or mutating herdr commands, and a temp `XDG_STATE_HOME`
   and `LOOPHERD_CONFIG_DIR`. Put everything it
   builds under one directory per PR, `<scratch>/pr<n>/`, where `<scratch>`
   is your session's own temp directory: its detached worktrees, its build
   target dirs and any copies (mutation runs), so one removal cleans up.
   Have it write its findings to `<scratch>/pr<n>/review-<round>.md`,
   each with severity, failure scenario and file:line, and return only
   one line: `pass <head sha>`, or `fail <head sha>: <b> blocking, <m>
   minor` plus the file's path. Its reply lands in your context and the
   file doesn't, so read the file only to forward findings or to write a
   merge question. Parallel reviews of one PR write
   `review-<round>-<axis>.md`. Members can't read your scratch dir, so
   forward a member's findings as a PR comment (`gh pr comment <n>
   --body-file <file>`), then `loopherd tell` the member to read that comment.
   Keep the files until the PR merges or closes.
   For anything that deletes or closes, have it hunt for safety gaps and
   name the test covering each rule. Report as soon as it lands; the human
   often merges fast.

   A finding is blocking when it's inside the task's acceptance criteria,
   or a correctness or safety bug the PR introduces; give the reviewer this
   bar, so its `<b> blocking` counts only these. Blocking findings, and
   any other finding that should be fixed and fits within the task's scope,
   go back to the member in the PR comment. Out-of-scope findings become
   new tasks (or, when no task is warranted, a PR comment marked
   non-blocking); the PR doesn't wait on them. Each round after the first
   checks the previous round's findings and the diff since that round's
   head, not the whole PR: hand the reviewer `review-<round - 1>.md` and
   `git diff <its head sha>..<new head>`, so fixes don't open findings in
   settled code. `<round>` counts per PR across handoffs; the handoff's
   current review file carries it. After round 3, start no other round:
   ask the human whether to merge as-is, narrow the scope, or change
   approach, listing the open blocking findings from the last review file.
5. **After merge** (confirm with `gh pr view <n> --json state`): close out
   the task in the project's tracker, unblock dependents, then `loopherd
   feed <herd>`, which first runs the herd file's `after_merge` (read its
   note, and do by hand whatever the note says didn't run). Its `done` check retires and closes the member and refills
   the slot. If the human merged before verification finished, point the
   verifier at the merge commit in a detached worktree under the PR's
   directory, since reaping removes the member's. Last, on every merge,
   once the tracker, dependents, feed and any post-merge verification are
   done: `git worktree remove --force` each verify worktree under
   `<scratch>/pr<n>/`, then delete that directory. Same for a PR closed
   unmerged.
6. **Triage** a herd running low on ready work (D60). A lead's triage
   arrives as its `question`: relay it to the human. Once the human
   approves, mark the ready tasks ready in the tracker, archive the stale
   ones the human confirms, and comment what's missing on each needs-spec
   task. In an unled herd, triage yourself when `status` shows claimable at
   or below `max` with unready above 0: run the herd's `backlog` and sort
   each task the same way.

### Handoffs

A member's Stop hook measures its context after each turn. Past its limit,
it blocks that stop once per session and tells the member to save its work
(a worker commits and pushes its branch and comments its progress on its
task), write its handoff to `$XDG_STATE_HOME/loopherd/handoffs/<name>.md`,
report `--kind handoff`, and end its turn. A stop with subagents still
running isn't asked until the first later stop without them, unless the
context is past 1.5x the limit; background shells alone don't defer it.
A handoff written before the member's last spawn, restart or `handoff`
report moves to `<name>.prev.md`, and the ask says so; one written since
stays, and a revive doesn't count, since it keeps the conversation. The
limit is the template's or `[lead]`'s `context_limit`, else `default_context_limit` in
`$LOOPHERD_CONFIG_DIR/config.toml`, else 250000, never over 80% of that
file's `context_window` (default 200000). `loopherd setup --context` asks
for the three `config.toml` values and `loopherd config get|set|unset
<key>` reads or writes one (`context_window`, `default_context_limit`,
`shepherd.context_limit`); change them only when the human says so. A
broken `config.toml` or herd
file falls back to 160000, and the hook leaves a `note` from the member once
per session naming the file; fix it (`loopherd herds` validates both).

Before handing off, a member that verifies PRs removes the `<scratch>/pr<n>/`
directories of PRs that already merged or closed (see After merge) and lists
each one still in use by absolute path under State, naming each PR's
current review file. A restart gets a new
`<scratch>`, so the successor removes those listed directories the same way
once their PRs merge or close.

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
also tells it to re-read `loopherd status` and `loopherd reports --all
--tail 40` and re-arm its watch. Without `--force`
it refuses unless the member reported a handoff since its last start, its
last stop since is `finished` with nothing in flight, and herdr shows it
idle or done. `--force` skips those checks; a `/clear` sent mid-turn waits
for that turn, then drops whatever the handoff doesn't record (that turn's
work, background tasks, crons). Forcing is the human's call (a lead's
PreToolUse hook denies it); a plain restart is not, so run it as soon as the
`finished` line arrives. A member never restarts itself, and one recorded without a
herdr socket can't be restarted, nor is it ever asked for a handoff. If
restart times out waiting for the new session, its `/clear` may be queued
behind a running turn, or the SessionStart hook isn't installed: it sent no
handoff prompt, left the handoff where it was, and prints the `loopherd
tell` line to send once the pane shows a cleared session.

Restart also refuses, even with `--force`, a member whose pane shows a
subagent's view (prompt line `❯ Message @…`, or a picker whose selected
`●` entry isn't `main`), since `/clear` there opens Claude's "/clear acts on the main
conversation" dialog instead. It sends nothing and keeps the handoff: switch
the pane back to the main conversation, then run restart again. If that
dialog shows up anyway when restart times out, restart says so: pick
Cancel, switch back to main, and rerun it.

A lead's `loopherd watch` is a background shell, so a lead stops its own
watch before ending its handoff turn; otherwise its stop reads `paused`
and restart refuses it. After a herdr restart, `revive`'s start makes
restart refuse a member that handed off before it: check `loopherd reports
--all <name>` for its `handoff`, and with the handoff file still there, the
human runs `loopherd restart <name> --force`. With the plugin's startup
revive on, this follows every herdr restart without anyone running
`revive`, so check for a `handoff` before any restart that refuses. A member that was asked but
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
   waits in the mailbox for the next watch. First restart any lead whose
   `handoff` and `finished` lines you've already read, so the herd doesn't
   wait on the next shepherd.
2. Write your handoff to `$XDG_STATE_HOME/loopherd/handoffs/shepherd.md`
   (`~/.local/state` when it's unset; no member may be named `shepherd`), in the outline above: what each
   member is doing and what you owe it (verifications running, PRs to
   check after merge), then next steps, then gotchas.
3. Tell the human, with the path spelled out in full:
   > I'm past my context limit and wrote my handoff. Run `/clear`, then paste:
   > `continue as the loopherd shepherd from /home/<you>/.local/state/loopherd/handoffs/shepherd.md`

   and end your turn. Never `/clear` yourself.

Given that prompt in a fresh session, run `loopherd --skill`, read the
handoff, `loopherd status` and `loopherd reports --all --tail 40` (for a
fuller history, `--since <time>` or plain `--all`), then re-arm your watch
and carry on from its next steps.

### Notifying the human

With `[notify] enabled = true` in `$LOOPHERD_CONFIG_DIR/config.toml`, the
watch that first prints a `question` report or a `blocked` line also tells
the human outside your pane, once per event. A lead's watch sends for its
workers and for the lead itself, so a lead's questions and blocks reach the
human with no shepherd session running; the shepherd's watch still prints
the lead's, and a mark per event keeps it to one send:

```toml
[notify]
enabled = true                    # default false: nothing is sent
kinds = ["question", "blocked"]   # the default
herdr = true                      # herdr's toast on the watched socket; default true
command = "notify-send {herd}/{member} {text}"
pane_label = true                 # label the relaying pane in herdr; default false
```

`command` is optional: words split on whitespace (no quotes; use an array
of arguments for spaces), each placeholder (`{herd}`, `{member}`, `{kind}`,
`{text}`) filled within its own argument, never through a shell. `{text}`
is the question's first non-blank line, cut to 200 characters with token-like words
and URL passwords `[redacted]` (best-effort: a secret in an unknown shape
gets through), or `blocked`. Each send runs detached within 30s; a failing
one leaves a `note` in the member's mailbox, once until it next succeeds.
`loopherd herds` reports a broken table, which sends nothing, and warns of
an enabled one with `herdr = false`, no `command` and no `pane_label`.
Notifying doesn't replace relaying: still tell the human in your pane. Not
sent: a question you read with `loopherd reports` before a watch saw it, and
a member already blocked when every watch started. A lead relaying a worker's
question as its own sends a second notification.

`pane_label = true` (default false) also labels, in herdr's sidebar, the
pane that must relay or answer: yours when your watch prints a `question`
(`<member> asks: <text>`) or `blocked` line, whatever `kinds` says, and a
lead's own when it reports `--kind question`. The label takes the place of
the agent's name and is display-only: herdr still shows the pane's own
status, never `blocked`, so no watch reads it as a dialog. The human's next
prompt in that pane, or a new, resumed or cleared session there, clears it;
a background task finishing, such as your watch, or a compaction doesn't.
A lead's watch never labels for its workers. Clearing needs the
UserPromptSubmit hook from `install-hook`; `loopherd herds` warns when
`pane_label` is on without it. If you installed with `install-hook
--settings <file>`, pass the same `--settings <file>` to `herds`.

### Commands

- `loopherd doctor [<herd>]` checks the install, herdr, config and state,
  changing nothing: each loopherd hook in Claude's settings (a missing
  Stop, StopFailure, SessionStart or PreToolUse hook fails; a missing
  UserPromptSubmit hook fails while `[notify] pane_label` is on, else
  warns), hooks calling a binary other than the `loopherd` on PATH
  (fails), running a binary other than PATH's (warns), herdr unreachable
  on its socket, silent for 5s, or older than 0.9.5 (fails). The loopherd
  herdr plugin missing, disabled, or at another version than loopherd
  warns only. A
  `config.toml` or `[notify]` that doesn't load fails; unset lead
  defaults or limits warn, naming `loopherd setup`. An invalid herd file,
  or a herd `repo` missing or not a git repo, fails. An unparseable
  ledger, pause file or member's mailbox fails; an unretired member with
  no live agent warns, naming `revive` or `retire`, and members on
  another herdr socket are counted, not checked. `--trackers` also runs
  each herd's `source` and `backlog` from its repo (paused herds too, 30s
  each), failing on a nonzero exit or timeout; `--verbose` or `--json`
  shows each command's exit status and task count, never the tasks.
  `<herd>` limits the herd, repo, state and tracker checks to that herd.
  It prints only problems, each with its fix, then `doctor: <n> ok, <n>
  warnings, <n> failures`; `--verbose` lists every check, `--json` gives
  them as data, `--settings <file>` checks another settings file. Exits 0
  clean or warnings only, 1 on any failure, 2 when it couldn't run or
  `<herd>` is unknown (D63).
- `loopherd status` lists members by herd with herdr's live status;
  `--all` adds retired entries, each row's status reading `retired <time>`
  (a bare `retired` when the ledger has no time), and `--strays` lists live agents missing from the ledger. `--json` gives
  each member a `retired` field: its retirement time (`unknown` if the
  ledger has none), or null while it's open; check that field, not
  `status`, before calling a member open. `loopherd view` shows the same
  members as a live TUI, without task counts. Each herd whose queue has a
  `backlog` gets `<n> claimable, <m> unready` beside its name, even with no
  members: claimable counts the `source` tasks feed would take with no cap
  on slots (a task whose name an unreleased member holds counts as
  neither), and unready counts `backlog`'s lines. A failing command shows
  `claimable unknown (<error>)` (or `unready unknown`) and `status` still
  succeeds. `--json` gives each herd `claimable` and `unready`, integers or
  null without a `backlog`, with `claimable_error` or `unready_error` beside
  a failed one (D60, D61).
- `loopherd reports` prints unread reports, questions first, and marks them
  read; `loopherd reports --all` prints every report and stop and marks
  none read. Both take member names or `--herd <herd>`. `--all` also takes
  `--since <date, time or span such as 2h>` and `--tail <n>` (the last n
  entries after the other filters).
- `loopherd stats` shows how each herd's work flowed: per retired worker
  (a released and re-fed name gets a row per run), time to its first `done`, review rounds, merge wait and total, with
  medians per herd and open workers apart. `--herd`, `--since <date or time>`,
  `--json`; it marks nothing read.
- `loopherd retire <name>` marks a member finished in the ledger only;
  `--close` also closes its tab and removes its worktree and branch if
  they're clean and merged. Feed's `done` check does this for you.
- **Re-feed a retired task** (its member was retired before it finished,
  e.g. a tab closed by accident or a rescoped card) with `loopherd release
  <name>`, then `loopherd feed <herd>`. Feed skips every name in the
  ledger until it's released, so a merged task isn't spawned again while
  its card is still open. Release refuses a member not retired, a lead, or
  one whose tab may be open (`loopherd retire --close <name>` first). It
  removes the member's worktree, any leftover at the path the new member
  spawns into, its local branch and its branch on origin, so the new member
  starts afresh; when they hold commits not on the base (pushed or not) it
  refuses until you pass `--keep-branch` (the new member continues from
  them) or `--drop-branch` (deleted; the lines printed name each head sha).
  It never removes uncommitted changes, a locked worktree, the main
  checkout, a worktree loopherd didn't make, or a live member's worktree;
  it refuses and names them, and refuses while a feed, add or release
  runs (retry once it ends). It won't delete an origin branch that an open
  PR is on, naming the PR, unless you pass `--drop-branch`, which closes
  the PR. It deletes the origin branch after the local ones, and marks
  the name released only once origin's branch is gone, so feed skips the
  name until then. If origin kept it because
  someone pushed to it meanwhile, check who before anything else: rerun
  with `--keep-branch`, never `--drop-branch`, unless those commits should
  go. Any other failed delete leaves the rest done, and rerunning the
  same release command once the cause is fixed deletes it and releases;
  while origin can't be asked, that rerun refuses. The delete runs the repo's pre-push hooks,
  within `LOOPHERD_GIT_TIMEOUT`; when one refuses, release prints its
  output. `--keep-branch` never contacts origin. A lead
  may run it; it's part of feeding. `status --all` marks the entry
  `(released)`, its mailbox gets a `released` note, and its lead's watch
  announces it claimable again.
- `loopherd prompt <name>` sends a member its stored prompt, e.g. after a
  spawn exited 3 on a dialog. Like `tell`, it refuses and sends nothing
  while the member's pane shows a subagent's view.
- `loopherd tell <name> "<text>"` messages a member (see Talking to
  members).
- `loopherd restart <name>` gives a member that handed off a fresh session
  (see Handoffs).
- `loopherd lead <herd>` starts the herd's lead from its herd file's
  `[lead]` table.
- **Change a herd file's values** with `loopherd herd set <herd> <key>
  <value>`, `loopherd herd get <herd> <key>` and `loopherd herd unset <herd>
  <key>`, not by hand-editing TOML. Keys are dotted (`queue.max`,
  `lead.merge`, `template.card.args`); the value is TOML, so quote strings
  (`'"x"'`). It keeps the file's comments and layout, creates missing
  tables, and refuses, changing nothing, any edit `loopherd herds` would
  call invalid. `herd unset <herd> lead.<switch>` hands the switch back to
  `[lead_defaults]`, not to off. Turn a lead's switches on only when the
  human says so.
- **Remove a finished herd** with `loopherd herd remove <herd>` once the
  human says so: it deletes the herd file, its pause, its `[lead]`
  `prompt_file` (when that lies in the herds dir and no other herd file
  names it), and every run of the herd in the ledger, archived ones
  included, with each member's mailbox and handoffs, so the herd leaves
  `herds`, `status --all` and `stats`. A name that still holds another
  herd's run keeps that run as its entry, and its mailbox and handoffs. It refuses while any
  member, lead included, isn't retired, ran on another herdr session, or
  may still be open (its agent or pane shows, even busy or moved), and
  while herdr can't be reached; `loopherd retire --close <name>` each
  first, which also cleans up tabs, worktrees and branches that
  `herd remove` never touches. `--dry-run` lists what would go;
  `--keep-history` keeps the ledger entries and mailboxes for `stats`.
- **Lead defaults** are `[lead_defaults]` in
  `$LOOPHERD_CONFIG_DIR/config.toml`: each lead's switches wherever its herd
  file's `[lead]` doesn't set one. `loopherd setup`, which
  `scripts/install` runs, asks for them on a terminal while they're unset;
  `loopherd setup --lead-defaults` asks again and `--lead-defaults
  merge,install` (or `none`) sets them without asking, as do the same
  flags on `scripts/install`.
  `loopherd lead-defaults` prints them. `review_rounds`, how many review
  rounds a lead runs on a PR before asking the human (a positive integer,
  default 3), resolves the same way: `lead.review_rounds` in the herd
  file, else `loopherd config set lead_defaults.review_rounds <n>`.
  `loopherd herds` shows each lead's value and its source. Change them only
  when the human says so.
- **When the human asks to pause or resume a herd**, run `loopherd pause
  <herd> [--reason <text>]` or `loopherd resume <herd>` (`--all` for every
  herd), not a prose prompt to its lead. A paused herd's `feed` retires
  finished members and spawns none; in-flight workers carry on, so it
  drains by itself. `resume` prints the next step. Neither prompts anyone.
  A watch that exited `drained` stays down: after `loopherd resume`, re-arm
  yours by hand, and `loopherd tell` a lead that stopped on `<herd> drained`
  to re-arm its own.
- **After a herdr restart** run `loopherd revive` (`--dry-run` first). It
  resumes non-retired members by session ID, and prompts each lead whose
  watch isn't running to re-arm it. It never prompts workers. It skips a
  lead whose pane shows a subagent's view and exits 1 (`--dry-run` says
  so too): switch that pane back to the main conversation, then run
  `loopherd revive --name <lead>`. Two revives never overlap: one waits
  for the other, then finds its members alive. It touches only members
  recorded on the herdr server it reaches, printing `skipped <name>:
  recorded on <socket>` for the rest and still exiting 0, so a revive in
  the dev session never starts the live session's members, and the other
  way round; run it in the member's own session. Members with no recorded
  socket are revived and take that server's. Neither holds when herdr
  can't name its socket: then nothing is skipped or filled in (D68).
- **herdr plugin.** `herdr plugin install <owner>/loopherd/herdr-plugin`
  (loopherd's README names the repo) adds plugin `loopherd`,
  whose `status` action runs `loopherd status`; its output and errors go
  only to `herdr plugin log list --plugin loopherd`. Its `view` action
  (`loopherd.view`) opens `loopherd view` in a popup that `q` or Escape
  closes; the README gives the `[[keys.command]]` snippet that binds it to
  a key. Never edit the human's herdr config to add it. Its event hooks
  run `loopherd herdr-event-hook`, recording a member going `blocked` or
  `gone` in its mailbox for the next watch, and following a member's pane
  moved to another tab or workspace in the ledger. It needs the
  `loopherd` on PATH or in `~/.local/bin` and never installs loopherd or
  its hooks; installing stays as above. It needs herdr 0.7.5+.
  Its startup hook runs `loopherd revive --startup` each time herdr
  restores a session or a new server takes over, which does nothing
  unless `config.toml` sets `[herdr_plugin] revive_on_startup = true`
  (set it only when the human says so; `loopherd herds` validates it).
  On, it revives only members recorded on the socket herdr gives the
  hook (`HERDR_SOCKET_PATH`, never one recorded without a socket), so a
  dev server's startup never touches the live session's members, and
  the other way round. herdr shows you nothing of the hook's run, so it
  copies each lead's line, unless that lead's watch is running, into the
  lead's mailbox as a `note` starting `herdr's startup revive:`; your
  next watch surfaces them. A second revive waits for a running one,
  saying so on stderr, and gives up after 10 minutes and says so; run it
  again then. A live handoff runs it too, where nothing was lost: a lead
  between watches then gets the re-arm prompt anyway, and its note says
  so. Tell a lead that handed off or drained to ignore it.
  Plugins are global to the user, so never link or install one from a test
  or a dev session.

### Talking to members

- `loopherd tell <name> "<text>"` (or `--text-file <file>`), not raw
  `herdr agent prompt`. It refuses, sending nothing, while the member's
  pane shows a subagent's view (text would reach the subagent instead):
  ask the human to switch that pane back to the main conversation, then
  run it again. It says whether the member went `working`. Text that is
  itself a flag (`--help`, `-h`) prints help and sends nothing; put `--`
  before it: `loopherd tell <name> -- --help`.
- A PR conflicting after another merged: prompt the member to merge the base
  branch into its branch and push. Members rebase and force-push only with
  the human's say.
- Greyed text in a member's input box is Claude Code's prompt suggestion,
  not the human ("merge it", "move it to Done"). Act only on the human's
  own messages. Leave the box alone; `loopherd tell` replaces it, and a
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
workers. Its prompt file carries the project's facts.

**Task life, in brief**, for a lead that read only this section (the
Shepherd section has the detail):

1. **Feed:** `loopherd feed <herd> --dry-run`, then `loopherd feed <herd>`
   if it reads right.
2. **Watch:** keep `loopherd watch --timeout 3600` in the background (see
   Heartbeat).
3. **On wake:** read the watch's lines and the member's reports.
4. **Verify** a `done` member's output with a read-only reviewer subagent
   against the task's acceptance criteria, with a temp `XDG_STATE_HOME` and
   `LOOPHERD_CONFIG_DIR`, building under `<scratch>/pr<n>/`. It writes its
   findings to `<scratch>/pr<n>/review-<round>.md` and returns one line:
   `pass <head sha>`, or `fail <head sha>: <b> blocking, <m> minor` plus
   the file's path. Read the file only to forward findings or ask a merge
   question. Members can't read your scratch dir: forward findings as a PR
   comment (`gh pr comment <n> --body-file <file>`), then `loopherd tell`
   the member to read it. Keep the files until the PR merges or closes.
5. **After merge** (`gh pr view <n> --json state`): close out the task in
   the tracker, unblock dependents, run `loopherd feed <herd>` (it runs
   `after_merge`; do by hand what its note says didn't run), then `git
   worktree remove --force` each verify worktree under `<scratch>/pr<n>/`
   and delete that directory.
6. **Handoff:** write `$XDG_STATE_HOME/loopherd/handoffs/<your name>.md` in
   the outline under Handoffs (`## State`, `## Next steps`, `## Gotchas`),
   listing each `<scratch>/pr<n>/` still in use and naming each PR's
   current review file.

What differs from the shepherd:

- **No human in the pane.** Nobody reads your screen or answers there. A
  decision that isn't yours goes out as `loopherd report --kind question
  "<the exact question>"`, then end your turn; the shepherd's watch sees it
  and relays it. Don't wait in a loop for an answer.
- **Switches.** Your prompt lists six, each `on` or `off`; off means the
  human does it, so ask instead. Each comes from your herd file's `[lead]`,
  else `[lead_defaults]` in `$LOOPHERD_CONFIG_DIR/config.toml` (set by
  `scripts/install`), else off; `loopherd herds` shows which:

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
  ask instead. A herd file or `config.toml` it can't read blocks all three. A `feed` from
  your pane runs none of `after_merge` when the hook would deny any command
  in it, leaving a `note` naming why; that run counts as done, so run the
  commands it held back by hand or ask the human to. It denies
  broadly: `gh pr merge --help`, `git rebase --abort` and any `curl` to a
  PR's merge endpoint are denied too, so ask the human to run a `rebase
  --abort`, and check merges with `gh pr view <n> --json state`. Grilling
  is always the human's.
- **Herd files and loopherd's config are the human's.** The hook always
  denies `loopherd herd set`, `unset` and `remove` in your pane, and also `loopherd
  lead-defaults --set`, `loopherd config set` and `unset`, `loopherd
  setup`, and `scripts/install --lead-defaults` or `--context`, whatever
  your switches, so you can't turn your own switches on or raise your own
  handoff or review-round limit; `loopherd herd get`, `loopherd config get` and `loopherd
  lead-defaults` are fine. Ask for a change instead.
- **Retiring a lead is the shepherd's or the human's.** `loopherd retire`
  refuses to retire you from your own pane, and the hook also denies
  `loopherd retire` of a `lead-` name, or of a name it can't read such as
  `"$m"`, there; retire members by their spelled-out name.
  Your gates outlast your retirement: a retired lead's pane is still
  gated.
- **Pausing is the human's.** The hook always denies `loopherd pause` and
  `loopherd resume` in your pane, whatever your switches. While your herd
  is paused your watch prints no `claimable` and `feed` spawns nothing;
  `merged`, reports, stops, `blocked` and `gone` wake you as before, so
  keep verifying and running after-merge steps. On resume your watch
  announces the claimable tasks again within 60s.
- **Running low.** On `<herd> running low`, have a read-only subagent
  run your herd's `backlog` and sort each task into ready, needs a
  decision (naming it), needs spec (saying what's missing), or stale
  (saying why). Report the result as one `loopherd report --kind question`
  and end your turn. Never mark a task ready yourself, whatever your
  switches; the shepherd does it once the human approves.
- **Asking to merge.** With `merge` off, the question carries the PR link
  and a ready-to-paste line pinned to the full head SHA your reviewer
  verified, `! gh pr merge <n> --merge --match-head-commit <full sha>`, in
  the account form your prompt gives. Leave out `--delete-branch`. If
  another PR must merge first, name it in the question. Single-quote the
  report, `loopherd report --kind question '<question with the merge
  line>'`: double quotes run any `$(...)` in it and can leak a token into
  the report.
- **Review rounds.** A finding is blocking when it's inside the task's
  acceptance criteria, or a correctness or safety bug the PR introduces;
  give the reviewer this bar. Blocking and other in-scope findings go back
  to the worker; out-of-scope ones become tasks. Each round after the
  first checks only the last round's findings and the diff since its head:
  hand the reviewer `review-<round - 1>.md` and `git diff <its head
  sha>..<new head>`. A first review uses the reviewer as defined; later
  rounds pass `model: sonnet` to the subagent. `<round>` counts per PR across handoffs; your
  handoff's current review file carries it. After your herd's
  review-round limit (in your prompt; 3 when unset) start no other round;
  send `loopherd report --kind question`
  asking whether to merge as-is, narrow the scope, or change approach,
  with the PR link and the open blocking findings, then end your turn.
  With `new_cards` off, list out-of-scope findings in a question rather
  than creating tasks.
- **Heartbeat.** Keep `loopherd watch --timeout 3600` as a background task.
  Run from your pane, it's your watch: it reads your herd's workers only,
  never you, and naming any other herd is an error. It holds `watch-<your
  name>.lock`, so it runs beside the shepherd's watch, which reads you. Run
  it from your own pane in your own herdr, or it runs as the shepherd's
  watch instead: a pane that matches you but not your socket prints
  `warning: pane <pane> holds lead <you> on socket <recorded>, but this
  watch's socket is <this>; running the shepherd's watch` on start. Stop it
  if you see that: it holds the shepherd's lock and wakes you with other
  herds' events. If you're retired, it exits with an error and
  leaves your herd's events for the shepherd. Every 60s it also runs your
  herd's checks and wakes you with `<member> merged` (the template's
  `merged` passes, and its `done` doesn't or its after_merge is still due):
  run the after-merge steps for it;
  or `<name> claimable` (feed would spawn `<name>`): feed; or `<member>
  interrupted` (a dead feed left its after_merge unnoted): feed, then do by
  hand what its note says. Each wakes you
  once, so a claimable task you don't feed then shows up again only in the
  heartbeat's dry run. On `<herd> drained` (your herd is paused and no
  worker is open), report done, stop, and don't re-arm. Exit 2 is a
  heartbeat, not idleness: still check each PR waiting on review (`gh pr
  view <n> --json state`), run the after-merge steps for any the human
  merged, and run `loopherd feed <herd> --dry-run`. After every exit, run
  `loopherd status` and re-arm in the same message as your first action,
  unless the wake needs a handoff or says `<herd> drained` (then don't
  re-arm). Read the watch's output once; don't read it again after acting.
- **After a restart** the shepherd, or the herdr plugin's startup hook,
  runs `loopherd revive`, which prompts you
  to re-arm whenever nothing holds `watch-<your name>.lock`, whether herdr
  resumed you or revive did. Re-read `loopherd status` and `loopherd reports
  --all --tail 40` (for a fuller history, `--since <time>` or plain
  `--all`) and the skill's Lead section (`loopherd --skill --section
  lead`, not the whole skill), then arm your watch again.
- **On each wake** re-read `loopherd status` and `loopherd reports --all
  --tail 40` (for a fuller history, `--since <time>` or plain `--all`)
  before acting: auto-compaction drops detail, and your state lives in the
  tracker, ledger and mailbox, not in your context. After a compaction,
  re-read the Lead section too (`loopherd --skill --section lead`, not the
  whole skill).
- **Your own handoff.** Past your limit, your Stop hook asks you for a
  handoff like any member's (see Handoffs): stop your own watch (TaskStop
  it, or kill it), write it, report `--kind handoff`, and end your turn. The shepherd restarts you. Restart your
  workers when they hand off; never with `--force`.
- **Your checkout** is detached; refresh it with `git fetch origin && git
  checkout --detach origin/<default branch>` rather than `git pull`, and
  never commit in it.
