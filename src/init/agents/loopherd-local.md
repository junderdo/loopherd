# Working an issue in a loopherd herd

Issues are markdown files under `.scratch/<feature>/issues/`. An issue's
triage state is the `Status:` line near its top, and comments go under a
`## Comments` heading at its bottom.

loopherd starts one worker per ready issue, each in its own worktree on its
own branch. The worker's prompt names the issue, the branch and the exact
commands; this doc says what each step is for. How the tracker works is in
`docs/agents/issue-tracker.md`.

## Claiming

Read the issue file in full first. Then set its `Status:` line to `claimed`,
append `claimed by <member>` under `## Comments`, with your member name, and
commit on your branch. Every gh command runs as @ACCOUNT@.

## Branch names

`<type>/<id>-<slug>`: the id is the feature and the issue's number; the type
comes from the words of its `Status:` line through the herd file's type map
(by default `fix` for `bug`, `docs` for `documentation`, else `feat`); the
slug comes from the file name.
loopherd creates the branch and its worktree; work only there. If @BRANCH@
moves under you, merge `origin/@BRANCH@` into your branch; never rebase or
force-push.

## Checks

Before calling the work done, run each check and fix what fails:

@CHECKS@

## The pull request

Before pushing, set the issue's `Status:` line to `resolved` and append a
one-line summary under `## Comments`, so merging the PR closes the issue;
loopherd counts the issue done once @BRANCH@ has it resolved. Push the branch
and open a PR with `--base @BRANCH@`.

## Stopping on a question

When only the human can answer, stop rather than guess: set the issue's
`Status:` line to `@NEEDS_INFO@`, append the question and the options you see
under `## Comments`, commit and push, then report it.

## Reporting

Tell the shepherd how the work ended:

- Done: `loopherd report --kind done "<PR link and one-line summary>"`.
- Stopped on a question: `loopherd report --kind question "<the exact question>"`.
