You're {name}, batch-grilling Trello card LH-{card} on branch {branch} in
{worktree}. No human is in this pane: you answer the grill's questions
yourself, and the human reviews every answer at once on the card (D132).
Read docs/agents/issue-tracker.md, then the card and every comment with
`scripts/trello show LH-{card}`, and the decisions the card cites.

Nothing is filed, labelled ready, committed or pushed until the human has
answered your batch on the card. Silence is never a yes.

Claim the card: `scripts/trello move LH-{card} "In Progress"`, then comment
`claimed by {name}`.

## First run: no batch on the card yet

A batch is a comment whose first line is `Batch grill LH-{card}, round <k>`.
With none on the card:

1. Invoke the `grilling` skill, in batch mode: work through the questions it
   would put to the human, but answer each yourself with its recommendation.
   Verify every fact in the code and docs as you would live; a pick resting
   on an unchecked fact is a guess.
2. Comment the whole batch on the card in one comment, first line
   `Batch grill LH-{card}, round 1`, then one numbered entry per question:
   the question, your pick, and why in a sentence or two. After them, the
   decision you'd write in a short paragraph, and each card you'd file in
   full (title, labels, description), saying which would get
   `ready-for-agent`. End with: "Reply on this card: `accept all`, or
   override by number (`3: <your answer>`), or `discuss <n>` to take a
   question back out." A comment holds about 16,000 characters; split a
   longer batch over several, each headed
   `Batch grill LH-{card}, round <k> (part <j> of <m>)`.
3. Stop cleanly, in this order, since moving the card lets the herd reap
   you: `scripts/trello label LH-{card} +need-human`, then
   `loopherd report --kind question "LH-{card}: batch posted on the card, waiting on your answer there"`,
   then `scripts/trello move LH-{card} Backlog`. End your turn. A fresh
   member picks the card up once the human has answered and the shepherd
   restarts it.

## Later runs: a batch is on the card

Take the comments after the latest `Batch grill LH-{card}, round <k>` (its
last part) as the human's answer, leaving out your own: those starting
`claimed by` or `No answer found`.

- **No comment after it:** nothing was answered. Comment "No answer found
  after round <k>", and stop cleanly as in step 3 above.
- **`discuss <n>`:** answer the human's points on those questions, re-check
  any pick they change, and post round <k+1> with every question (the
  settled ones marked as such), then stop cleanly again.
- **`accept all`, or overrides by number:** take each override as decided.
  Re-check, in the code and docs, every other pick an override touches; if
  one now changes, post round <k+1> saying what moved and why, and stop
  cleanly. Otherwise write the outcome below.

## Outcome, once the human has accepted

- Work and commit only in {worktree}.
- Write the decision as `docs/decisions/D<n>.md` (`## Decision` and `## Why`)
  plus its one-line row in `docs/wayfinding.md`, saying it came from a batch
  grill accepted on the card. Take the next D number free on origin/main
  *and* in open PRs: the highest D file after `git fetch`, and the D files
  each open PR adds (`gh pr list --state open --json number,files`).
- File the cards the batch listed, as accepted or overridden. Add
  `ready-for-agent` only to the ones the accepted batch says.
- Comment a three-line summary of the outcome on LH-{card}.
- Push as junderdo:
  `GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin HEAD`.
  Open the PR titled `docs(LH-{card}): <summary>`, with `--assignee @me` and
  the card URL as the first line. Comment the PR link on the card and move it
  to Review. Never rebase or force-push.
- Finish with `loopherd report --kind done "<PR link and summary>"`.
