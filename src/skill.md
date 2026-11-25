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

## References

loopherd's user docs sit beside this file in `references/`; read one when
its topic comes up. Without the files (this guide read from `loopherd
--skill`), `loopherd docs <topic>` prints the same text.

- Every command and its flags: `references/tour.md`.
- Setting up a project and a first herd with `loopherd init`:
  `references/setup.md`.
- Installing, updating, the herdr plugin and key bindings:
  `references/install.md`.
- What a term means (herd, member, mailbox, handoff and the rest):
  `references/glossary.md`.
- Writing or tailoring a herd file or lead prompt: `references/herd-file.md`.
- What a project's shepherd runbook holds: `references/runbook.md`.

## Shepherd

At the start of each session, run `loopherd doctor` (with `--settings
<file>` if you installed with it). It prints only problems; relay each
failure to the human with the fix it names (D63).

The human's shepherd only, never a lead or member: also at the start of
each session, name your herdr agent so the herd view's `s` focuses you
instead of starting a second shepherd: `herdr agent rename
"$HERDR_PANE_ID" shepherd`. If herdr answers `agent_name_taken`, another
shepherd already runs in the pane it names; stop and tell the human. If
it answers `agent_launch_pending`, find your entry in `herdr agent list`
(its `pane_id` is `$HERDR_PANE_ID`): if its name is already `shepherd`,
carry on without a warning, since `s` started you under that name;
otherwise retry the rename once its `launch_pending` clears. On any other
error, carry on and tell the human that `s` can't find you (D88, D146).

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
   `references/herd-file.md` explains each key of the herd file and the
   lead prompt.
5. Edit only what init wrote: the herd file (with `loopherd herd set`), the
   lead's and grill's prompt files beside it, and the repo files init listed. Leave the
   rest of the repo and other herds alone.
6. You're done when `loopherd herds` calls the herd valid and
   `loopherd feed <herd> --dry-run` lists the tasks you expect from the
   tracker.
7. Fill in the `docs/agents/shepherd.md` stub init wrote, the runbook a
   later shepherd reads, per `references/runbook.md`.
8. Have the human review and commit the repo files you and init changed.
9. Remind the human to open Claude in the repo once and accept its
   trust dialog, since members can't start past it, and that
   `loopherd lead <herd>` starts the herd's lead when they want one.

### A task's life

1. **Feed.** Keep the `loopherd` on PATH current, since members call it for
   `loopherd report` and Claude calls it from its hooks. `loopherd
   install-hook` adds the Stop, StopFailure, SessionStart, PreToolUse and
   UserPromptSubmit hooks to Claude's user settings (backed up first;
   re-running changes nothing) and installs this skill in
   `~/.agents/skills/loopherd`, linked from `~/.claude/skills/loopherd`. `watch` needs the
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
   An optional `[queue] low_at = <n>`, default `2 × max` and at least
   `max`, is the claimable count at or below which a lead's watch says the
   herd is `running low` (D116).
   An optional top-level `stalled_after = <minutes>`, default 30 and at
   least 1, is how long a member may sit idle on a paused stop before a
   watch says it `stalled` (D152).
   `loopherd herds` lists herd files, their templates and `after_merge`. Hand-spawn only for
   work outside the queue, with `loopherd add <herd> <template> <params…>`.
   A lead always uses `add`: `loopherd spawn … --prompt-file` records no
   herd, so a sandboxed lead can't `retire --close` what it spawns. Check a
   prompt's placeholders with `grep -o '{[a-z]*}'`.
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
   then its `running low` (either form) and `out of work`, then its
   `interrupted` and `merged`, by member in name order, then its
   `claimable`; then `blocked` and `gone`; then `no broker`. The shepherd's own `out of
   work` lines for unled herds, and a `drained` line, print only on a poll
   nothing else woke, alone but for a due `shepherd handoff`. The first
   watch after an install, or over members a herd-scoped watch skipped,
   prints every unread stop change since; judge each against `reports
   --all`.
3. **On wake,** read the watch's lines and the task (`loopherd reports
   --all --tail 40` for the timeline, stops included; for a fuller
   history, `--since <time>` or plain `--all`, or `--all <name>` for one
   member). The lines:

   | Line | Means |
   |---|---|
   | `<member> done <time> <text>` | A finished member's report, usually with its output's link. Verify it. |
   | `<member> question <time> <text>` | It stopped on a question. Relay it to the human; a lead relays it with its own question report, unless it's a design question and `design_escalation` is on. A lead's question waits until a prompt reaches its pane (yours by `tell`, or the human typing), which `reports --all` records as `<lead> answered <time>`; until then `status` shows `question waiting <age>` on its row (D143). |
   | `<member> note <time> <text>` | Information only, except a `self-restart …` note: that member's handoff is still waiting on a restart (see Handoffs), and a lead's note naming a follow-up it filed ready: tell the human, who may veto it; on a veto, drop the task's ready label, or retire its worker once one claimed it. |
   | `<lead> gate <time> <text>` | A lead's gate request, for a herd whose shepherd runs its gates: run the gate on the named PR and sha and send the lead the result. Until a prompt reaches its pane, `status` shows `gate waiting <age>` on its row. |
   | `<member> handoff <time> <text>` | It passed its context limit and wrote its handoff. Restart it (see Handoffs) if you read it; a lead restarts itself. |
   | `<member> paused  1 shell, 1 cron` | Turn ended with work in flight that can wake it again. Another paused stop is quiet. |
   | `<member> paused  background unknown` | Turn ended and Claude didn't say what's in flight (also `crons unknown`, or `1 shell; crons unknown`). |
   | `<member> finished` | Turn ended with nothing in flight, after a `done` or `question`. |
   | `<member> finished  no report` | The same, with no `done` or `question` since its last finish or start. |
   | `<member> failed  rate_limit` | Turn ended on an API error; it may need a nudge once that clears. |
   | `<member> merged` | Lead's watch: the worker's template `merged` check passes and its `done` doesn't, or its herd's after_merge is still due. Run the after-merge steps. The shepherd's watch, in its 60s round of checks, prints it for an attended member of an unled herd it reads, done or not, with that check's `failing` and `recovered` lines; it runs no other member check (D112). |
   | `<name> claimable` | Lead's watch only: feed would spawn this member. Feed. |
   | `<herd> failing  source "<command>": <stderr line>` | Lead's watch (the shepherd's prints only its `<member> merged` form, as that row says): the herd's source failed (also `<herd> failing  backlog "<command>": …` for its `backlog`, and `<herd> failing  <member> done "<command>": …` for a member's `done` or `merged` check), with its last stderr line. Once per streak; one cut off by the 60s round is no event. Fix the cause, or tell the shepherd. |
   | `<herd> recovered  source` | Lead's watch (the shepherd's, as for `failing`): what last printed `failing` passes again (also `<herd> recovered  backlog` and `<herd> recovered  <member> done`). A later failure prints `failing` again. A member that retires mid-streak gets none. |
   | `<herd> running low  <n> claimable, <m> unready` | Lead's watch only: the herd's `backlog` lists unready tasks and claimable (as `status` counts it) is at or below `[queue] low_at` (default `2 × max`). Once per streak: it prints again only after claimable rises above `low_at` or unready reaches 0. A paused herd prints none and keeps its streak. Triage (see Lead). |
   | `<herd> running low  0 claimable, <m> unready, no workers open` | Lead's watch only: as above, but claimable is 0 and no worker is open, so nothing runs (D80). Prints even mid-streak of the plain form, and in its place when both start together. Once per streak: it prints again only after a worker opens (even one that opened and retired while no watch ran), claimable rises above 0, or unready reaches 0. A paused herd prints none and keeps its streak. Triage (see Lead). |
   | `<herd> out of work` | The herd isn't paused, claimable is 0, its `backlog` lists nothing unready (or it has none), and no worker is open. A lead's watch prints it for its herd; the shepherd's, every 60s, for each unled herd with a herd file it reads. Once per streak: it prints again only after claimable or unready rises above 0 or a worker opens, even one that opened and retired while no watch ran. The shepherd's doesn't repeat what a retired lead's watch already said. A paused herd prints none and keeps its streak. A lead reports it (see Lead); the shepherd tells the human, who pauses the herd or files and approves work. |
   | `<member> interrupted` | Lead's watch only: the feed running the worker's `after_merge` died before noting it. Feed, which notes it, then read the note. |
   | `<member> stalled  paused 47m: 1 shell` | In its 60s round, a lead's watch for its workers, and the shepherd's for leads and members of unled herds: the member's last stop was paused (a lead's own running watch doesn't count) (with what it left in flight), nothing came from it since, and herdr has shown it idle for its herd's `stalled_after` (default 30m). Once per streak: a new stop, report, or herdr showing it busy ends the streak, and a later stall prints again. `status` shows it as `stalled <n>m`. Read its pane (`loopherd read <member>`); if the shell or task it waits on is hung, `loopherd tell` it to drop that and carry on (D152). |
   | `<member> blocked` | A dialog only the human answers: tell them. loopherd never answers it. |
   | `<member> gone` | Its pane exited or closed, its tab or workspace closed with it inside, or it had no agent for two polls in a row after this watch saw it live. A `gone` recorded when a tab or workspace closed, and then retracted because the member's pane had moved out of it, prints nothing; `reports --all` shows it followed by `moved`. |
   | `<lead> no broker` | Your own watch only: an unretired lead on your herdr has had no broker running for two polls in a row (one is a start or `broker-restart` gap), so its `feed`, `status` and `report` fail and it can't even ask. Once per streak: it prints again only after its broker runs again. Run `loopherd broker-restart <lead>`, which starts one when none runs. |
   | `<herd> drained` | Lead's watch only: your herd is paused and has no open (non-retired) worker. A member of an attended or `slot = false` template is no worker here, nor for `no workers open`, `out of work` or `all drained` (D116), but while a `slot = false` one may still work (as `all drained` says) it holds both drained lines off; an attended one only until its `gone` line prints (D140). Report done, stop, and don't re-arm. |
   | `all drained` | Every herd your watch reads is paused with no open worker, and no member it reads, a slot-less one included but not an attended one, may still work: none is working, blocked or unknown (now or on the poll before), died since the watch started without its `gone` line yet, or last stopped failed or with work in flight. Tell the human; don't re-arm. |
   | `shepherd handoff 171k over 160k` | Your own watch only, once per session: your context is past your limit. Hand off (see Your own handoff). |

   A report's text runs on in indented lines. A turn you prompt into a member
   still `paused` finishes silently, since the stop reads paused again;
   check it with `loopherd reports --all`. A plain `loopherd reports` while
   a watch is armed consumes reports the watch would have printed, so look
   with `reports --all`. For a member that left no report, read its screen
   with `loopherd read <name>` (its last 40 lines; `--lines <n>` up to 1000).
4. **Verify** a finished member's output with a read-only reviewer
   subagent against the task's acceptance criteria. Ask for it by role:
   the `reviewer` agent type, else `loopherd-reviewer`, else a
   general-purpose subagent told to stay read-only. A first review uses
   the reviewer as defined; later rounds pass `model: sonnet` to the
   subagent, since they only recheck findings and a small diff. Give it
   no loopherd or mutating herdr commands, and a temp `XDG_STATE_HOME`
   and `LOOPHERD_CONFIG_DIR`. Tell it every wait it runs has a timeout,
   and it stops every background shell it started before it returns: a
   leftover shell holds your stops at `paused` and blocks your restart.
   Put everything it
   builds under one directory per PR, `<scratch>/pr<n>/`, where `<scratch>`
   is your session's own temp directory: its detached worktrees, its build
   target dirs and any copies (mutation runs), so one removal cleans up.
   Have it write its findings to `<scratch>/pr<n>/review-<round>.md`,
   each with severity (blocking, minor or nit), failure scenario and
   file:line, and return only one line: `pass <head sha>: <m> minor, <k>
   nit` when nothing is blocking, else `fail <head sha>: <b> blocking, <m>
   minor, <k> nit`, plus the file's path. Its reply lands in your context
   and the file doesn't, so read the file only to forward findings, to
   gather minors and nits, or to write a merge question. Gather minors and
   nits once, when the PR passes or you write its merge question: from
   every round's review file, not only the passing one, into one follow-up
   and one nit comment, each routed as below. Later rounds don't repeat
   earlier rounds' minors and nits, so a failed round's live only in its
   file. Your handoff records each PR whose minors and nits are gathered,
   so whoever picks it up doesn't refile them. Parallel reviews of one PR
   write `review-<round>-<axis>.md`.
   Members can't read your scratch dir, so forward only a failing round's
   blocking findings, in a PR comment holding just those: copy them to
   `<scratch>/pr<n>/review-<round>-blocking.md`, run `gh pr comment <n>
   --body-file` on that file, then `loopherd tell` the member to read that
   comment. Keep the files until the PR merges or closes. For anything
   that deletes or closes,
   have it hunt for safety gaps and name the test covering each rule. Report
   as soon as it lands; the human often merges fast.

   A finding is blocking when it's inside the task's acceptance criteria,
   or a correctness or safety bug the PR introduces; give the reviewer this
   bar, so its `<b> blocking` counts only these. Every other finding,
   whatever its severity, is handled as a minor or, when trivial, a nit
   (a pre-existing bug the PR didn't introduce is a minor). Only blocking
   findings fail a round, and only they go back to the member while it
   fails, in the PR comment above. Minor findings from any round never
   hold the merge: they go to one follow-up task per PR (a lead with
   `new_cards` off lists the follow-up minors in a question to the human
   instead), or, when the member has a next PR in the same card's stack,
   into that PR's scope. Nits go on the PR as a comment marked
   non-blocking and are not tasks; the PR doesn't wait on them. Each
   round after the first checks the previous round's blocking findings
   and the diff since that round's head, not the whole PR: hand
   the reviewer `review-<round - 1>.md` and `git diff <its head
   sha>..<new head>`, and give it a narrower bar: is each previous
   blocking finding fixed, and does the fix diff introduce a new
   correctness or safety bug? Its `<b> blocking` counts only those. A new
   minor or nit it notices is reported as non-blocking and handled as
   above; it never fails the round, so fixes don't open findings in
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
   done: stop (TaskStop) every background shell or Monitor still running for
   that PR's review, then `git worktree remove --force` each verify worktree
   under `<scratch>/pr<n>/`, then delete that directory. A shell left waiting
   on a file there never finishes and holds your stops open. Same for a PR
   closed unmerged.
6. **Triage** a herd running low on ready work (D60, D116). A lead's
   triage arrives as its `question`: relay it to the human. In an unled
   herd, act when `status` shows claimable at or below `low_at` (as
   `herds` prints it beside `max`) with unready above 0. If it has a
   `[template.groom]`, run `loopherd add <herd> groom` (nothing while
   `groom-<herd>` is open); on its `done`, relay its report to the human,
   then `loopherd retire --close groom-<herd>`. Without the template, run the herd's `backlog` and sort
   each task the same way.

   A groomer's report ends in a numbered batch, one command per line. The
   human approves it whole or all but some lines: run the command of each
   batch line the human approved, as written after its number, and no
   others. A triage without a batch:
   once the human approves, mark the ready tasks ready in the tracker,
   archive the stale ones the human confirms, and comment what's missing
   on each needs-spec task. Marking ready and archiving stay yours, on the
   human's word.
7. **Out of work.** A lead's `<herd> out of work` arrives as its
   `question`, and your own watch prints the line for an unled herd (D72):
   either way, tell the human the herd has nothing left and ask whether to
   pause it or file and approve work. Don't pause it yourself.

### Handoffs

A member's Stop hook measures its context after each turn. Past its limit,
it blocks that stop once per session and tells the member to save its work
(a worker commits and pushes its branch and comments its progress on its
task), write its handoff to `$XDG_STATE_HOME/loopherd/handoffs/<name>/handoff.md`,
report `--kind handoff`, and end its turn. A stop with subagents still
running isn't asked until the first later stop without them, unless the
context is past 1.5x the limit; background shells alone don't defer it.
A handoff written before the member's last spawn, restart or `handoff`
report moves to `prev.md` beside it, and the ask says so; one written since
stays, and a revive doesn't count, since it keeps the conversation.
`loopherd handoff` writes the handoff from stdin to that path. A
handoff an older loopherd left flat at `handoffs/<name>.md` is still read,
and moves to `handoffs/<name>/prev.md` once used. The
limit is the template's or `[lead]`'s `context_limit`, else `default_context_limit` in
`$LOOPHERD_CONFIG_DIR/config.toml`, else 250000, never over 80% of the
context window: that file's `context_window` if set, else the session's
model's (1M for 4.7 and later, else 200000). An unknown model gets 200000
and a `note` once per session. A limit is tokens or a percentage of that
window (`context_limit = "65%"`). `loopherd setup --context` sets
the three `config.toml` values and `loopherd config get|set|unset
<key>` reads or writes one (`context_window`, `default_context_limit`,
`shepherd.context_limit`); change them only when the human says so. A
broken `config.toml` or herd
file falls back to 160000, and the hook leaves a `note` from the member once
per session naming the file; fix it (`loopherd herds` validates both).

Before handing off, a member that verifies PRs removes the `<scratch>/pr<n>/`
directories of PRs that already merged or closed (see After merge): first
stop (TaskStop) every background shell or Monitor still running for that PR's
review, then remove its worktrees and the directory. It also lists
each one still in use by absolute path under State, naming each PR's
current review file and whether its minors and nits are gathered. A
restart gets a new
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

**Restart** whoever's `handoff` line you read: a lead restarts its own
workers, and the shepherd restarts workers of unled herds and members in no
herd. A lead restarts itself (below): after its `handoff` and `finished`
lines, a `start` from it should follow within a few watch polls. If none
does (it forgot `--self`, or `--self` refused), or a `self-restart` note
arrives, restart the lead yourself; the locks make that safe beside a
waiter. For anyone else, wait for its `finished` line, then `loopherd
restart <name>`. It
sends `/clear`, waits for the ledger to follow the new session, and prompts
the member to read its handoff, which it moves to `prev.md` beside it so no
later restart reuses it; same pane, worktree and branch. A lead's prompt
also carries its switches and review-round limit, resolved from its herd
file and `[lead_defaults]` at restart, and tells it to re-read `loopherd status` and `loopherd reports --all
--tail 40` and re-arm its watch. Without `--force`
it refuses unless the member reported a handoff since its last start, its
last stop since is `finished` with nothing in flight, and herdr shows it
idle or done. `--force` skips those checks; a `/clear` sent mid-turn waits
for that turn, then drops whatever the handoff doesn't record (that turn's
work, background tasks, crons). Forcing is the human's call (a lead's
PreToolUse hook denies it); a plain restart is not, so run it as soon as the
`finished` line arrives. A member never runs `loopherd restart <its own
name>`, since its `/clear` would land mid-turn; one recorded without a
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

It also refuses, even with `--force` and before any `/clear`, a lead whose
herd file or `config.toml` can't be read, or that has no herd or `[lead]`:
fix the herd, then run restart again.

**A lead restarts itself.** After reporting `--kind handoff` and stopping
every background shell and Monitor it started, it runs `loopherd restart --self` from its own pane and ends its
turn. That checks it is a lead with a handoff file and a `handoff` report
since its last start with no stop since (so run it in the handoff turn),
starts a detached waiter, and returns at once. The
waiter runs the plain restart, with every check `--force` skips, once the
stop after the handoff reads `finished` and herdr shows it idle or done:
same pane, worktree and ledger entry. One waiter per member: a second
`--self` refuses while one waits, and a plain `loopherd restart` and the
waiter never both send `/clear`: the waiter exits quietly once another
restart used the handoff, and stops with a note if one ended without using
it, since that one may have sent `/clear`. After 30 minutes, on a revive's
start, or on any failure, the waiter leaves a `self-restart …` note in the member's mailbox
naming why: act on it as on the `handoff` line. Unless the note says the
restart failed after its `/clear`, the handoff is still in place, so a plain
`loopherd restart <name>` works once the member is finished (after a
revive, the human forces it). Only a lead runs `--self`; for any other
member it refuses, and whoever reads its `handoff` line restarts it.

A lead stops every background shell and Monitor it started (TaskStop
each), its `loopherd watch` among them, before ending its handoff turn:
any one left running, a wait loop as much as the watch, makes its stop read
`paused`, and restart refuses it. After a herdr restart, `revive`'s start makes
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
`context_window`, else of your model's window. When the line comes:

1. Handle the other lines it came with, but feed nothing new and don't
   re-arm the watch. Members and leads keep working; what they report
   waits in the mailbox for the next watch. First restart any member
   whose `handoff` and `finished` lines you've already read and that is
   yours to restart, so the herd doesn't wait on the next shepherd.
2. Read `loopherd reports --all --since <your last watch wake, or your
   session start>` and note every `question` or `done` report you haven't
   acted on, gate requests included, and who is waiting on what. Do this
   whether or not a watch is armed: a report that arrived while none was
   never woke you, and the next shepherd's watch may not print it for
   hours. `reports --all` marks nothing read, so it takes nothing from
   the next watch.
3. Write your handoff to `$XDG_STATE_HOME/loopherd/handoffs/shepherd/handoff.md`
   (`~/.local/state` when it's unset; no member may be named `shepherd`), in the outline above: what each
   member is doing and what you owe it (verifications running, PRs to
   check after merge), with each report from step 2 listed under State,
   then next steps, then gotchas.
4. Tell the human, with the path spelled out in full:
   > I'm past my context limit and wrote my handoff.
   > Run `/clear`, then send any message (e.g. `go`), and I resume from it.
   >
   > If I don't, paste this after the `/clear` instead:
   > `continue as the loopherd shepherd from /home/<you>/.local/state/loopherd/handoffs/shepherd/handoff.md`

   and end your turn. Never `/clear` yourself. The watch that told you
   recorded your pane, Claude and session, so the first `/clear` of this
   session after your handoff is written moves it to `handoffs/shepherd/prev.md` and adds the
   resume to the new session's context.

   Self-restart is on unless `config.toml` sets
   `[shepherd] self_restart = false`
   (`loopherd config get shepherd.self_restart`); while it is on, first run
   `loopherd restart --self` in this turn. When it arms, tell the human in your last
   message, above the lines above: you restart yourself in this pane in 5
   minutes, and any prompt here stops that. Then end your turn. A detached
   waiter sends `/clear` and `go` once your turn has ended, herdr shows the
   pane idle, its input box is empty and the human sent nothing here for 5
   minutes. When it refuses, give the human its reason with the lines
   above. A self-restart that gives up labels your pane and sends a
   `self_restart` notification; the handoff stays for the manual resume. A
   prompt from the human while it waits cancels it: once you've answered,
   tell them to `/clear` and send `go`, or ask whether to re-arm, and on a
   yes run `restart --self` again. After a self-restart the resume says
   so; say it in your first message.

In a fresh session, given that resume (naming the `prev.md` path) or the
pasted line, run `loopherd --skill`, read the handoff at the path it names
(or `prev.md` beside it, if a resume already moved it there),
`loopherd status` and `loopherd reports --all --tail 40` (for a fuller
history, `--since <time>` or plain `--all`), then re-arm your watch and
carry on from its next steps.

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
kinds = ["question", "blocked", "merge_alert", "self_restart", "no_broker"]   # the default
herdr = true                      # herdr's toast on the watched socket; default true
command = "notify-send {herd}/{member} {text}"
pane_label = true                 # label the relaying pane in herdr; default false
```

`command` is optional: words split on whitespace (no quotes; use an array
of arguments for spaces), each placeholder (`{herd}`, `{member}`, `{kind}`,
`{text}`) filled within its own argument, never through a shell. `{text}`
is the question's first non-blank line, cut to 200 characters with token-like words
and URL passwords `[redacted]` (best-effort: a secret in an unknown shape
gets through), `blocked`, `no broker`, or a merge alert's PR. Each send runs detached
within 30s; a failing one leaves a `note` in the member's mailbox, once until it next succeeds.
`loopherd herds` reports a broken table, which sends nothing, and warns of
an enabled one with `herdr = false`, no `command` and no `pane_label`.
Notifying doesn't replace relaying: still tell the human in your pane. Not
sent: a question you read with `loopherd reports` before a watch saw it, and
a member already blocked when every watch started. A lead relaying a worker's
question as its own sends a second notification.

A `no_broker` is sent once per outage of a lead's broker, by whichever
watch sees it first: the lead's own, or yours, which prints `<lead> no
broker`. Any watch seeing the broker run again re-arms it.

A lead's watch also sends `merge_alert`, with `{text}` the PR's number and
URL, once per PR merged into its repo's default branch, when its `[lead]`
has `merge` off and `merge_guard = "hook"` (the default). The lead runs
with the human's credentials, so a merge the human didn't make shows up.
The lead's first lookup only marks where its window starts, so no earlier
merge is sent; later ones run at most once a minute across its watches,
only while `kinds` holds `merge_alert`, and a re-armed watch catches up on
merges it missed. An attended member's merge sends none: its branch is
skipped, and the watch prints `<name> merged` (D140). A lookup that fails
warns on the watch's stderr. Not covered: pushes straight to the branch,
and merges after the lead's watch stops for good (a drained lead). A `credentials` lead can't merge, so it
gets no alert.

To guard a `merge = false` lead with split credentials instead, set
`[lead] merge_guard = "credentials"` (or answer `init`'s question), then
have the human run `loopherd merge-guard <herd>` with their own gh login.
It makes the deploy key under `<config dir>/credentials/`, adds it to the
repo, creates or updates the rulesets on the default branch and on
force-pushes, writes `Host loopherd-<herd>` into `<config dir>/ssh/config`,
and prints the rest for the human: `Include <config dir>/ssh/config` as the
first line of `~/.ssh/config` (once; loopherd never writes `~/.ssh`), and
the web-UI steps for the token stored at `<herd>.token`. The lead won't
start without its key and token, with `install = true`, or unless its
herd sets `[worker] sandbox = "require"`; `loopherd doctor` checks the
key, the token, the Include, the `Host` block and the rulesets.

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
  A herd with migrations `loopherd herd upgrade` would offer, declined
  ones left out, gets one `info` line naming them, never a failure.
  `<herd>` limits the herd, repo, state and tracker checks to that herd.
  It prints only problems and info lines, each with its fix, then `doctor: <n> ok, <n>
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
  a failed one (D60, D61). A member of an `attended = true` template, one
  the human sits in, reads `(attended)` and `--json` gives it `attended`:
  it holds no slot, and a watch wakes on its `gone` alone, and on its
  `merged` the lead's watch, or the shepherd's for an unled herd (D112).
- `loopherd reports` prints unread reports, questions first, and marks them
  read; `loopherd reports --all` prints every report and stop and marks
  none read. Both take member names or `--herd <herd>`. `--all` also takes
  `--since <date, time or span such as 2h>` and `--tail <n>` (the last n
  entries after the other filters).
- `loopherd stats` shows how each herd's work flowed: per retired worker
  (a released and re-fed name gets a row per run), time to its first `done`, review rounds, merge wait and total, with
  medians per herd, again per template within it, and open workers apart. `in` is the tokens its Claude
  read (input + cache write + cache read) and `out` the tokens it wrote,
  over every session its `stop`s name plus their subagents; retiring
  records them in the mailbox as a `usage` entry, since Claude deletes
  transcripts after `cleanupPeriodDays`. `-` means none are left, its
  mailbox can't be read, or Claude's dir can't be found (`HOME` unset), and
  is left out of the medians. Each herd's `lead overhead` line is its lead's
  tokens within the window, subagents included (D92). The `outcome` column
  is how the run ended (D92): `merged` when feed retired it with its
  template's `merged` check passing, `closed` when feed retired it without
  that (a template with no `merged` check always gives `closed`, so its
  runs are left out of the medians), `released` after `loopherd release`,
  `retired` for `loopherd retire`, with or without `--close`, and `-` for a
  run retired before outcomes were recorded. The medians and maxima cover
  only `merged` runs; the summary counts the other outcomes apart, and each
  template's line does the same for its runs. `--herd`,
  `--since <date or time>` (started on or after), `--until <date or time>`
  (started before; an `--until` at or before `--since` is refused; a date
  means midnight UTC, so `--until` excludes that day),
  `--json` (the per-model split, and `templates`); it marks nothing read.
  Runs without a template are summarized as `-`, `null` in JSON.
  To judge a template or prompt change, run it once per window, before and
  after, and compare the template's lines (D92).
- `loopherd retire <name>` marks a member finished in the ledger only;
  `--close` also closes its tab and removes its worktree and branch if
  they're clean and merged. Feed's `done` check does this for you. Retire
  refuses an entry not yet retired that herdr shows `working` or
  `blocked`, whatever it reported, or shows idle (or done, or unknown)
  with no `done`, `question` or `handoff` report since its last start, so a
  retire meant for an earlier run of the name can't kill a fresh one. The
  error says which; `--force` retires it anyway. A report before a revive
  still counts, a member herdr doesn't list retires without a report, and
  an already-retired entry takes `--close` as before. It also refuses, naming
  `--force`, when herdr doesn't answer within 5s, answers with an error or
  a reply loopherd can't parse, or runs another session than the member's
  (even while this session's herdr is down); otherwise no herdr server at
  all lets it through. A live lead rarely reports
  `done` and is often working, so retiring one usually takes `--force`.
- **Re-feed a retired task** (its member was retired before it finished,
  e.g. a tab closed by accident or a rescoped card) with `loopherd release
  <name>`, then `loopherd feed <herd>`. Feed skips every name in the
  ledger until it's released, so a merged task isn't spawned again while
  its card is still open. Release refuses a member not retired, a lead, or
  one whose tab may be open (`loopherd retire --close <name>` first, with
  `--force` if herdr shows it working, blocked, or idle with no report
  since its start). It
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
  may run it on its own herd's members, without `--drop-branch`; it's
  part of feeding. `status --all` marks the entry
  `(released)`, its mailbox gets a `released` note, and its lead's watch
  announces it claimable again.
- `loopherd prompt <name>` sends a member its stored prompt, e.g. after a
  spawn exited 3 on a dialog. Like `tell`, it refuses and sends nothing
  while the member's pane shows a subagent's view.
- `loopherd tell <name> "<text>"` messages a member (see Talking to
  members).
- `loopherd restart <name>` gives a member that handed off a fresh session
  (see Handoffs); `loopherd restart --self`, from a lead's own pane,
  schedules that for itself once its turn ends.
- `loopherd lead <herd>` starts the herd's lead from its herd file's
  `[lead]` table, in the Claude Code sandbox (see the Lead section). With
  `[lead] sandbox = "require"` (the default) a lead whose sandbox can't
  start isn't started, and `lead`, `restart` and `revive` say why
  (`loopherd doctor` checks it: bubblewrap and socat on Linux and WSL2);
  `"prefer"` starts it guarded by its hook alone, which catches only
  plainly written commands, so its herd files, config and switches are
  protected against plain forms only. It starts with a warning, a note
  in its mailbox, and `(lead, unsandboxed)` in `loopherd view`. `restart`
  refuses, under `require`, a lead started without its sandbox (before it
  existed, or on `prefer`): quit its Claude and run `loopherd revive
  --name <lead>` to resume it sandboxed. The human's own
  `~/.claude/settings.json` can loosen every lead's sandbox
  (`excludedCommands`, `allowWrite` and the like), and the permissions in
  a lead worktree's `.claude` settings can widen its file tools: `lead`,
  `revive` and `restart` refuse such a lead under `require`, naming each
  file and key, and start it with them as warnings under `prefer`, except
  a `merge_guard = "credentials"` lead, refused under both (`merge_guard =
  "hook"` is the other way); doctor reports them too. Only the human
  edits `~/.claude/settings.json`, and a repo's `.claude` settings change
  through a reviewed PR; ask the human. A lead's Claude args, from
  `[lead].args`, else the herd's `args`, then the command line, may be
  only `--model`, `--effort`, `--verbose` and `--permission-mode` with
  `default`, `manual`, `auto` or `plan` (each also as `--flag=value`);
  any other is treated as a loosening is (`--settings`, `--mcp-config` and
  `--strict-mcp-config`, loopherd's own launch flags, are refused under
  both `require` and `prefer`), and `lead --dry-run` checks
  them too. To keep an arg the workers need from the lead, give it its
  own `[lead].args` rather than editing the herd's `args`. A lead has no MCP tools
  unless its herd's `[lead] mcp_servers` names servers from the human's
  user scope (`loopherd herds` shows them); a lead that needs one
  asks the human to add it. A lead has no Unix sockets (Seatbelt blocks
  them on macOS; on Linux and WSL2, Claude Code's seccomp filter, where
  present) unless its herd's
  `[lead] unix_sockets = "all"` (`loopherd herds` shows it), which reopens
  herdr, the ssh agent and WSL2 interop to it; `lead`, `revive` and doctor
  warn of it, and only the human sets it. A `merge_guard = "credentials"`
  lead can't have it: `lead`, `revive` and `restart` refuse the pair,
  `herd set` and `herd unset` refuse an edit that makes it, `loopherd
  herds` calls the herd file invalid, and doctor fails it. Where, on Linux or WSL2, Claude
  Code's seccomp filter can't block them (doctor says so, naming `npm install -g
  @anthropic-ai/sandbox-runtime`), `require` refuses the lead and
  `prefer` starts it as `(lead, sockets open)`; `restart` applies the
  same rule. A `merge_guard = "credentials"` lead is refused under both.
  Under `unix_sockets = "all"` the socket check is skipped: the lead isn't
  refused or flagged for open sockets, though `require` still refuses
  it when the sandbox can't start at all. What still gets past the sandbox:
  workers run unsandboxed unless the herd's `[worker] sandbox` turns
  theirs on, so a lead can have one act for it (a `credentials` lead
  starts only with `[worker] sandbox = "require"`, and its workers are
  refused for loosened settings or args under either mode), and most of
  the state dir stays writable to it. Set `prefer` only
  when the human says so; doctor names those herds and leads.
- **Change a herd file's values** with `loopherd herd set <herd> <key>
  <value>`, `loopherd herd get <herd> <key>` and `loopherd herd unset <herd>
  <key>`, not by hand-editing TOML. Keys are dotted (`queue.max`,
  `lead.merge`, `template.card.args`); the value is TOML, so quote strings
  (`'"x"'`). It keeps the file's comments and layout, creates missing
  tables, and refuses, changing nothing, any edit `loopherd herds` would
  call invalid. `herd unset <herd> lead.<switch>` hands the switch back to
  `[lead_defaults]`, not to off. Turn a lead's switches on only when the
  human says so.
- **Bring a herd up to what newer init writes** with `loopherd herd upgrade
  [<herd>]`, run by the human on a terminal: it offers each missing change
  with a diff and `Apply? [Y/n]`, and a no lands in the herd file's
  `declined`. Without a terminal it changes nothing and lists each herd's
  pending ids. `loopherd init` re-run in the herd's repo asks the herd's
  settings again, Enter keeping each, then makes the same offers.
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
  first (`--force` for one herdr shows working, blocked, or idle with no
  report since its start),
  which also cleans up tabs, worktrees and branches that `herd remove`
  never touches. `--dry-run` lists what would go;
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
- **After a merge that changes the broker**, nothing: once it is
  installed, each lead's broker moves onto the new `loopherd` on its PATH
  by itself, between requests (D124).
  `loopherd broker-restart <lead>` is for when that fails: a note in the
  lead's mailbox saying its broker couldn't move, a lead's
  `<lead>'s broker predates this loopherd` error, its `can't read` reply
  error, or `loopherd doctor`'s warning that a broker doesn't run the
  loopherd on PATH (which `scripts/install` and `loopherd update` show). Run it from your own
  shell, never a lead's or an `after_merge`, as a background task: it can
  wait out a feed. It asks the broker to stop once its request in flight
  is answered, then starts one on the installed binary, which serves what
  is still queued; with none running it starts one. A broker still busy
  after 4 hours is left running and the command fails naming its pid.
  While it drains, every lane stops taking requests, so the lead's `tell`,
  `read` and `status` wait on the feed in flight, and one that waits past
  its own bound (2 minutes for `tell`) is withdrawn and must be asked again.
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
  closes. `scripts/install` offers, default no, to bind it to `prefix+m`
  and `view-beside-shepherd` to `prefix+shift+m` (`--with-herdr-keys`
  asks nothing), skipping keys already held; `loopherd install-herdr-keys
  --remove` takes them out (D129). Its `view-beside-shepherd` action splits the view beside the
  shepherd's pane recorded at its last handoff, leaving focus where it
  was, else beside the focused pane with a note saying why (D87, D94).
  `view.layout` in config.toml sets the side for it and for the view's
  `s`: `side-by-side`, the default, puts the shepherd left and the view
  right; `stacked` puts the view above the shepherd (D138). `v` in the
  view moves the shepherd's pane, when it shares the view's tab, to the
  other layout without restarting either, and saves it as `view.layout`
  (D139).
  Never edit the human's herdr config yourself; only the human's install does.

  The plugin's event hooks run `loopherd herdr-event-hook`, recording a
  member going `blocked` or `gone` in its mailbox for the next watch, and
  following a member's pane moved to another tab or workspace in the
  ledger. The plugin needs the `loopherd` on PATH or in `~/.local/bin` and
  never installs loopherd or its hooks; installing stays as above. It
  needs herdr 0.7.5+.
  Its startup hook runs `loopherd revive --startup` each time herdr
  restores a session or a new server takes over. It touches only
  members recorded on the socket herdr gives the hook
  (`HERDR_SOCKET_PATH`, never one recorded without a socket), so a dev
  server's startup never touches the live session's members, and the
  other way round. It always starts a broker for each alive lead there
  that has none. Only with `[herdr_plugin] revive_on_startup = true`
  does it also resume members and prompt leads; `loopherd setup` asks
  the human for it, and `loopherd config set
  herdr_plugin.revive_on_startup` sets it (only when the human says so).
  Off, it resumes, prompts and notes nothing. herdr shows you nothing of the hook's run, so it
  copies each lead's line, unless that lead's watch is running, into the
  lead's mailbox as a `note` starting `herdr's startup revive:`; your
  next watch surfaces them. A second revive waits for a running one,
  saying so on stderr, and gives up after 10 minutes and says so; run it
  again then. A live handoff runs it too, where nothing was lost: it
  tells one from a restart by the panes' shells outliving the server, and
  then prompts no alive lead and leaves it no note. Where it can't tell
  it treats it as a restart, and notes a `herdr's startup revive:
  warning:` line saying why; a lead between watches then gets the re-arm
  prompt, so tell one that handed off or drained to ignore it.
  Plugins are global to the user, so never link or install one from a test
  or a dev session.

### Talking to members

- `loopherd read <name> [--lines <n>]` prints a member's last lines of
  output (40 by default, up to 1000), not raw `herdr agent read`.
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
  unless you're a lead with `blocked_dialogs` on, and never for an attended
  member, whose pane the human sits in.
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
3. **On wake:** read the watch's lines and the member's reports; for a
   member that left none, `loopherd read <name>` shows its screen, not raw
   `herdr agent read`.
4. **Verify** a `done` member's output with a read-only reviewer subagent
   against the task's acceptance criteria, with a temp `XDG_STATE_HOME` and
   `LOOPHERD_CONFIG_DIR`, building under `<scratch>/pr<n>/`. Tell it every
   wait it runs has a timeout, and it stops every background shell it
   started before it returns. It writes its
   findings to `<scratch>/pr<n>/review-<round>.md` and returns one line:
   `pass <head sha>: <m> minor, <k> nit` when nothing is blocking, else
   `fail <head sha>: <b> blocking, <m> minor, <k> nit`, plus the file's
   path. Read the file only to forward findings, to gather minors and nits,
   or to write a merge question. Gather minors and nits once, when the PR
   passes or you write its merge question: from every round's review file,
   not only the passing one, into one follow-up and one PR comment marked
   non-blocking for the nits. The follow-up goes where Review rounds says:
   one follow-up task per PR, or the member's next PR in the same card's
   stack; with `new_cards` off, a question to the human. Later rounds don't
   repeat earlier rounds' minors and nits, so a failed round's live only in
   its file. Members can't read your scratch dir: forward only a failing
   round's blocking findings, in a PR comment holding just those (`gh pr
   comment <n> --body-file <scratch>/pr<n>/review-<round>-blocking.md`),
   then `loopherd tell` the member to read it. Keep the files until the PR
   merges or closes.
   Before you merge a passing PR or ask to, run `loopherd gate <full head
   sha>` when your herd file sets `[lead] gate`: your broker runs those
   checks outside your sandbox on that head merged with origin's default
   branch, sockets and all. For a PR into another branch (a stacked PR's
   parent, a release branch), add `--base <branch>` from `gh pr view <n>
   --json baseRefName`; the gate otherwise checks it against the default
   branch, not its real base. It prints `gate <sha> merged with origin/<base>
   <base sha>: pass`, or `failed:` with the command and its output's tail
   (exit 1), which goes back to the member. It refuses a sha not on one of
   origin's branches, and without a `gate` ask the human to add one. It can
   take many minutes, so run it as a background task and act on its
   completion; killed mid-wait, it runs on unread.
   When the shepherd runs your herd's gates instead, ask with `loopherd
   report --kind gate "Gate request: #<n> <branch> at <full sha>"`, not a
   note, and end your turn. Like a question, it waits until a prompt
   reaches your pane, and `status` shows `gate waiting <age>` on your row.
5. **After merge** (`gh pr view <n> --json state`): close out the task in
   the tracker, unblock dependents, run `loopherd feed <herd>` (it runs
   `after_merge`; do by hand what its note says didn't run), then stop
   (TaskStop) every background shell or Monitor still running for that PR's
   review, `git worktree remove --force` each verify worktree under
   `<scratch>/pr<n>/`, and delete that directory.
6. **Handoff:** write your handoff with `loopherd handoff <<'EOF'`, which
   puts it at `$XDG_STATE_HOME/loopherd/handoffs/<your name>/handoff.md`;
   your sandbox refuses your file tools, and a `cat >` or `tee`, there. Use
   the outline under Handoffs (`## State`, `## Next steps`, `## Gotchas`),
   listing each `<scratch>/pr<n>/` still in use and naming each PR's
   current review file and whether its minors and nits are gathered.
   Then report `--kind handoff`, stop every background shell and Monitor
   you started (your watch among them), run `loopherd restart --self`,
   and end your turn: you restart in this pane once your stop reads
   finished, with no shepherd needed (see Handoffs).

What differs from the shepherd:

- **No human in the pane.** Nobody reads your screen or answers there. A
  decision that isn't yours goes out as `loopherd report --kind question
  "<the exact question>"`, then end your turn; the shepherd's watch sees it
  and relays it. Don't wait in a loop for an answer.
- **On `<worker> stalled`.** A paused worker has sat idle past
  `stalled_after`. Read its pane with `loopherd read <worker>`; if the
  shell or task it waits on is hung, `loopherd tell` it to stop that and
  carry on. Never kill its shells yourself.
- **Switches.** Your prompt lists seven, each `on` or `off`; off means the
  human does it, so ask instead. Each comes from your herd file's `[lead]`,
  else `[lead_defaults]` in `$LOOPHERD_CONFIG_DIR/config.toml` (set by
  `scripts/install`), else off; `loopherd herds` shows which:

  | Switch | When on, you may |
  |---|---|
  | `merge` | merge a verified PR |
  | `install` | install loopherd and its hooks while feeding |
  | `rebase` | have a member rebase and force-push |
  | `new_cards` | create follow-up tasks for minor or out-of-scope findings |
  | `design_escalation` | settle a design question a member raises |
  | `blocked_dialogs` | answer a member's blocking dialog |
  | `follow_ups_ready` | file a qualifying review follow-up ready to claim once its parent card is Done (D116); needs `new_cards` on |

  When `merge`, `install` or `rebase` is off, the PreToolUse hook also
  blocks the Bash commands that need it (`gh pr merge` or the merge API, a
  `scripts/install`, `loopherd install-hook` or `loopherd update`, a rebase or
  force-push), in
  your pane and in any Claude nested in it, with a line naming the switch;
  ask instead. A herd file or `config.toml` it can't read blocks all three. A `feed` from
  your pane runs none of `after_merge` when the hook would deny any command
  in it, leaving a `note` naming why; that run counts as done, so run the
  commands it held back by hand or ask the human to. The hook reads only
  commands written out plainly, to catch a slip; it is not the boundary,
  so a form it doesn't read (a `bash -c`, a variable, a script) is no more
  yours to run. It denies
  broadly: `gh pr merge --help`, `git rebase --abort` and any `curl` to a
  PR's merge endpoint are denied too, so ask the human to run a `rebase
  --abort`, and check merges with `gh pr view <n> --json state`. With
  `merge` off, your watch also tells the human of every merge to the repo's
  default branch (`[lead] merge_guard = "hook"`, the default), so expect
  that ping for merges the human makes, but not for an attended member's
  (D140). Grilling is always the human's.
- **Attended members** are the human's (D112). Your broker refuses `tell`,
  `prompt`, `restart`, `retire` and `release` on one, and `add` of an
  attended template; your watch shows only its `merged` and `gone`. After
  its merge, move its card to Done and run `feed`, which reaps it.
- **Sandbox.** Your Claude runs in the Claude Code sandbox, from
  settings loopherd writes for you to `<config dir>/sandbox/<you>.json`
  (D89, building D83; D97, D111). Bash can write only to your own
  worktree, your own `handoffs/<you>/` in loopherd's state dir, and temp
  files; writes elsewhere, herd files, `config.toml`, those settings,
  `~/.claude`, `reports/` (every mailbox, yours included), other agents'
  handoffs, `paused/`, `shepherd.json` and the ledger among them, fail
  (`Read-only file system`, or a denied command or Edit). Your file tools,
  and Bash's redirects, `cat` and `tee`, can't write the state dir at all,
  so write your handoff with `loopherd handoff`. A plain `loopherd reports`
  fails too, since it marks reports read in `reports/`; `--all` works. Processes a
  command leaves behind end with it, so keep long waits as background
  tasks. Each Bash call also has its own pid namespace: `pgrep` or `kill`
  sees only processes that same call started, so polling from another call
  reads done at once, and only TaskStop stops another call's shell. Run the
  long command itself as the background task and act on its completion
  notification. A wait inside that one call uses `cmd & pid=$!; …; wait
  $pid`; if it must use `pgrep -f <pattern>`, that matches the waiting
  shell's own command line, which holds the pattern, so bracket a letter
  (`pgrep -f "[t]arget-…"`). `loopherd report`, `tell`, `read`, `status`,
  `restart` (`--self` included), `prompt`, `feed`, `add`, `retire`, `release` and `gate` don't call
  herdr or write your mailbox themselves: they hand the operation to your broker, a process
  outside the sandbox that `loopherd lead` started for you, in any form.
  `feed`, `add`, `restart`, `retire` and `release` run one at a time, so a
  running `feed` holds up the others; `report`, `tell`, `read`, `prompt`
  and `restart --self` run beside it, and `status` and `gate` each in
  a lane of their own.
  Your `status` shows your own herd alone, with no strays, and refuses
  `--strays`. With no broker running they fail at once, naming the
  shepherd's `loopherd revive --name <you>`. Your `report` fails that way
  too, so say so in your last message and end your turn.
  Your `watch` still calls herdr itself, not through your broker. Run
  `loopherd watch` bare, as its own Bash call, to run it outside the
  sandbox (D104), and `scripts/install` or a bare `loopherd update` (no
  flags) too with `install` on: a
  `cd x &&`, a redirect, `$(…)` or a path to the binary keeps it inside,
  where it can't write its files in `reports/`. Your watch refuses `--settings`. `revive` is the shepherd's
  or the human's. `tell`, `read`, `restart` and `prompt` reach only your
  own herd's unretired workers, and `retire` and `release` only your
  herd's workers, retired or not; `retire --close` and `release` also keep
  a worktree or refuse when the ledger puts one anywhere its herd file
  wouldn't, and your broker refuses `retire --force` and
  `release --drop-branch`, which are the human's.
  `loopherd spawn --worktree` is the human's: its worktree sits beside
  yours, where you can't write; use `loopherd add` instead. Every host is
  reachable, and gh and git use the human's credentials as before, except
  any behind a Unix socket (a keyring, an ssh agent, Git Credential Manager
  through WSL2 interop), which the sandbox closes and the first failing call
  shows (D105); ask the human. If your `[lead]` has
  `merge_guard = "credentials"` (D95), gh instead uses a token
  that can open and update pull requests but not merge or write contents,
  git pushes over ssh with the herd's deploy key, and the files holding the
  human's own GitHub credentials and ssh keys are unreadable. Pushes to the default
  branch and force-pushes are refused by GitHub; ask the human. The
  sandbox gives you no Unix sockets (Seatbelt on macOS; on Linux and WSL2,
  Claude Code's seccomp filter, where present), so `herdr`, an ssh agent and WSL2's Windows programs fail
  inside it (`Operation not permitted`), unless your herd sets
  `[lead] unix_sockets = "all"`, which only the human does. A lead started
  as `(lead, sockets open)` has them all. Workers
  run unsandboxed and most of the state dir stays writable to you;
  neither is yours to use against the sandbox. The sandbox
  is a guard rail the human set, not the whole boundary: when it refuses
  something, ask; don't route around it through herdr, a member or
  another tool.
- **Herd files and loopherd's config are the human's.** The hook always
  denies `loopherd herd set`, `unset` and `remove` in your pane, and also `loopherd
  lead-defaults --set`, `loopherd config set` and `unset`, `loopherd
  setup`, and `scripts/install --lead-defaults`, `--context` or
  `--revive-on-startup`, whatever your switches, so you can't turn your
  own switches on, raise your own handoff or review-round limit, or turn
  on the startup revive; `loopherd herd get`, `loopherd config get` and `loopherd
  lead-defaults` are fine. Ask for a change instead.
- **Retiring a lead is the shepherd's or the human's.** `loopherd retire`
  refuses to retire you from your own pane, and the hook also denies
  `loopherd retire` of a `lead-` name there. It always
  denies `loopherd retire --force` there too: retire a member once it is
  idle and has reported, or once herdr no longer lists it.
  Your gates outlast your retirement: a retired lead's pane is still
  gated.
- **Pausing is the human's.** The hook always denies `loopherd pause` and
  `loopherd resume` in your pane, whatever your switches. While your herd
  is paused your watch prints no `claimable` and `feed` spawns nothing;
  `merged`, reports, stops, `blocked` and `gone` wake you as before, so
  keep verifying and running after-merge steps. On resume your watch
  announces the claimable tasks again within 60s.
- **Running low.** On `<herd> running low`, either form (claimable at or
  below your herd's `low_at`, so triage lands before slots empty):
  - If your herd file has a `[template.groom]`, run `loopherd add <herd>
    groom` and end your turn (D116). Do nothing while `groom-<herd>` is
    open. On its `done`, relay its report as one `loopherd report --kind
    question`, batch unchanged, through a quoted heredoc as the Groom
    section sends it, then `loopherd retire --close groom-<herd>`. It holds no slot, so it isn't a worker for your other
    lines.
  - Without the template, have a read-only subagent run your herd's
    `backlog` and sort each task into ready, needs a decision (naming it),
    needs spec (saying what's missing), or stale (saying why). Report the
    result as one `loopherd report --kind question` and end your turn.

  After the `no workers open` form, the question says the herd has no
  work running. Never mark a task ready yourself; the shepherd does it
  once the human approves. The one exception is a qualifying review
  follow-up with `new_cards` and `follow_ups_ready` on (see Review
  rounds).
- **Out of work.** On `<herd> out of work`, send one `loopherd report
  --kind question` asking the human what's next: pause the herd, or file
  and approve work. Then end your turn.
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
  give the reviewer this bar. Every other finding, whatever its severity,
  is handled as a minor or, when trivial, a nit. Only blocking findings
  fail a round, and only they go back to the member while it fails, in a
  PR comment holding just those. Post nits in a separate PR comment marked
  non-blocking; they are not tasks. Minor findings from any round never
  hold the merge: they go to one follow-up task per PR, or, when the
  member has a next PR in the same card's stack, into that PR's scope.
  With `new_cards` off, list the follow-up minors in a question to the
  human rather than creating a task. With `new_cards` and
  `follow_ups_ready` on, file a qualifying follow-up with `--label
  ready-for-agent --label blocked` and the line `blocked-by: <parent key>`
  in its description, in your tracker's key form, so it becomes claimable
  once the parent card is Done. File it before the parent merges, so the
  after-merge unblock frees it; if the parent is already Done, unblock in
  the tracker right after filing. Comment `filed ready: follow-up of PR
  <n>` on it, and send `loopherd report --kind note` naming it, so the
  human can veto it through the shepherd. It qualifies only if it holds
  findings from the parent PR's review files, under the parent card's
  scope. Its criteria may offer the worker alternatives ("fix X, or D<n>
  records why not"). It doesn't qualify if any criterion needs the
  human's decision, changes a decision's direction, or carries a finding
  the reviewer called out of scope or design. File any other follow-up as
  with `follow_ups_ready` off. Gather minors and nits once, when the PR
  passes or you write its merge question: from every round's review file,
  not only the passing one, into one follow-up and one nit comment, each
  routed as above. Later rounds don't repeat earlier rounds' minors and
  nits, so a failed round's live only in its file. Your handoff records
  each PR whose minors and nits are gathered, so a restarted lead doesn't
  refile them. Each
  round after the first checks the previous round's blocking findings and
  only the diff since its head: hand the reviewer `review-<round - 1>.md`
  and `git diff <its head sha>..<new head>`, and give it a narrower bar: is
  each previous blocking finding fixed, and does the fix diff introduce a
  new correctness or safety bug? Its `<b> blocking` counts only those; a new
  minor or nit is reported as non-blocking, handled as above, and never
  fails the round. A first review uses the reviewer as defined; later rounds
  pass `model: sonnet` to the subagent. `<round>` counts per PR across
  handoffs; your handoff's current review file carries it. After your herd's
  review-round limit (in your prompt; 3 when unset) start no other round;
  send `loopherd report --kind question` asking whether to merge as-is,
  narrow the scope, or change approach, with the PR link and the open
  blocking findings, then end your turn.
- **Heartbeat.** Keep `loopherd watch --timeout 3600` as a background task,
  run bare, exactly so. Run from your pane, it's your watch: it reads your
  herd's workers only, never you, and naming any other herd is an error.
  It holds `watch-<your name>.lock`, so it runs beside the shepherd's
  watch, which reads you. Run it from your own pane in your own herdr: one
  your Claude runs from a pane the ledger doesn't give you fails with
  `refused: <you>'s Claude runs this watch, which may only be its own; …`
  naming why. In any other form it runs sandboxed, and from the wrong
  pane prints `warning: pane <pane> holds lead <you> on socket
  <recorded>, but this watch's socket is <this>; running the shepherd's
  watch`, then fails: your sandbox denies the shepherd's watch lock. If
  you're retired, it exits with an error and
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
  handoff like any member's (see Handoffs): stop every background shell
  and Monitor you started (TaskStop each), your watch among them, write it, report `--kind handoff`, run `loopherd restart
  --self`, and end your turn; you restart in this pane once that turn
  stops. Restart your workers when they hand off; never with `--force`.
- **Getting ready for a revive.** Keep your watch armed and keep working
  until the human quits your Claude: a quit Claude ends its watch and
  frees its lock, and `loopherd revive` has no check that a running shell
  fails. Stopping background shells and Monitors applies only before
  `restart --self`, whose stop must read finished. Any handoff you write,
  for any reason, is followed by `loopherd report --kind handoff`, never a
  plain `note`: a plain `loopherd restart` refuses a member that hasn't
  handed off since it started.
- **Your checkout** is detached; refresh it with `git fetch origin && git
  checkout --detach origin/<default branch>` rather than `git pull`, and
  never commit in it.

## Groom

A groomer is member `groom-<herd>`, from the herd file's `[template.groom]`
(D116), started on `running low` by the lead, or by the shepherd for an
unled herd. It holds no slot, sits in a detached worktree at origin's
default branch, and grooms the herd's unready tasks (its `[queue]
backlog`) so the human can approve them in one go.

- **Rights.** You may edit a card's text, comment on it, mark a duplicate
  by a comment naming the card it duplicates, and check whether a card is
  stale against the code.
- **Limits.** Never add `ready-for-agent`, archive a card, move a card
  between lists, or add or remove `need-human` or `blocked`. Those are the
  shepherd's, on the human's word. Commit nothing.
- **`need-human` cards.** For a `need-human` card, change nothing: restate
  its open question in your report.
- **Grilling cards.** On each `wayfinding:grilling` card, post a draft
  grill prompt as a comment: the decision to settle, the questions to put
  to the human in order, and what the grill should file once it settles.
- **Report.** Three parts, in this order:
  1. Each task sorted into ready, needs a decision (naming it), needs spec
     (saying what's missing), or stale (saying why), with the `need-human`
     questions restated.
  2. The edits you made, one line per card.
  3. A numbered batch of proposed actions, one per line, each with the
     exact command that applies it, in your tracker's command form, as
     your prompt or the project's tracker doc gives it: mark ready,
     archive, or comment what's missing on a needs-spec card. Each line is
     `<n>. <command>`, and the command is complete on its own: it needs no
     setup line, variable or earlier command. Give the reason after the
     command as a shell comment, `# <reason>`. Double-quote each text
     argument and escape any `"`, `$`, `` ` `` or `\` inside it with a
     backslash, so an apostrophe or quote in the text can't end it early.

  Send the report as one `loopherd report --kind done "$(cat <<'EOF'`, the
  report, then `EOF` and `)"` on their own lines. The quoted heredoc keeps
  every quote and `$` in the batch as written. End your turn. The lead
  relays it; the shepherd runs only the lines the human approves.
