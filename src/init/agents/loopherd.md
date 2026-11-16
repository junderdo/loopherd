# Working an issue in a loopherd herd

loopherd starts one worker per ready issue, each in its own worktree on its
own branch. The worker's prompt names the issue, the branch and the exact
commands; this doc says what each step is for. How the tracker works is in
`docs/agents/issue-tracker.md`.

## Claiming

Read the issue and every comment first. Then comment `claimed by <member>`
on it, with your member name, and assign it to @ACCOUNT@, the account every
gh command runs as. An issue someone else has claimed isn't yours.

## Branch names

`<type>/<number>-<slug>`: the type comes from the issue's labels through the
herd file's type map (by default `fix` for `bug`, `docs` for
`documentation`, else `feat`); the slug comes from the title.
loopherd creates the branch and its worktree; work only there. If @BRANCH@
moves under you, merge `origin/@BRANCH@` into your branch; never rebase or
force-push.

## Checks

Before calling the work done, run each check and fix what fails:

@CHECKS@

## The pull request

Push the branch and open a PR with `--base @BRANCH@`. End its body with the
line `Closes #<number>`, so merging the PR closes the issue; loopherd counts
the issue done once it is closed.

## Stopping on a question

When only the human can answer, stop rather than guess: add the
`@NEEDS_INFO@` label to the issue, comment the question and the options you
see, then report it.

## Reporting

Tell the shepherd how the work ended:

- Done: `loopherd report --kind done "<PR link and one-line summary>"`.
- Stopped on a question: `loopherd report --kind question "<the exact question>"`.
