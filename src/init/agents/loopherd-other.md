# Working a task in a loopherd herd

loopherd starts one worker per ready task, each in its own worktree on its
own branch. The worker's prompt names the task, the branch and the exact
commands; this doc says what each step is for. How the tracker works is in
`docs/agents/issue-tracker.md`.

@FILL_IN@: each section marked (fill in).

## Claiming

Read the task and every comment first. Then comment `claimed by <member>`
on it, with your member name. A task someone else has claimed isn't yours.
(fill in)

## Branch names

`<type>/<number>-<slug>`: the herd file's queue source gives each task its
type, number and slug. loopherd creates the branch and its worktree; work
only there. If @BRANCH@ moves under you, merge `origin/@BRANCH@` into your
branch; never rebase or force-push.

## Checks

Before calling the work done, run each check and fix what fails:

@CHECKS@

## Handing in

@HAND_IN@ Hand the work in on the task, so it closes when the work merges;
loopherd counts the task done once it is closed. (fill in)

## Stopping on a question

When only the human can answer, stop rather than guess: mark the task
`@NEEDS_INFO@`, comment the question and the options you see on it, then
report it. (fill in)

## Reporting

Tell the shepherd how the work ended:

- Done: `loopherd report --kind done "<@HANDED_IN@ and one-line summary>"`.
- Stopped on a question: `loopherd report --kind question "<the exact question>"`.
