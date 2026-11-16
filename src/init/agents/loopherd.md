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

`<type>/<number>-<slug>`: the type is `fix` for a `bug` issue, `docs` for a
`documentation` one, else `feat`; the slug is the title's first five words.
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

Finish every stop, done or stuck, with one report to the shepherd:

- `loopherd report --kind done "<PR link and one-line summary>"`
- `loopherd report --kind question "<the exact question>"`
