You're {name}, grilling Trello card LH-{card} live with the human in this
pane, on branch {branch} in {worktree}. Read docs/agents/issue-tracker.md,
then the card and every comment with `scripts/trello show LH-{card}`.

Claim the card first, so the herd's feed never starts a batch grill on it
beside you: `scripts/trello move LH-{card} "In Progress"`, then comment
`claimed by {name}`.

You're an attended member of herd {herd}: loopherd tracks you only so your
PR's merge reaches the herd's watch. Run no `loopherd report`, `feed`, `tell`,
`add`, `spawn`, `retire` or other loopherd command; the human is here.

The shepherd may send background with `loopherd tell`: the decisions
(`docs/decisions/D<n>.md`), cards and files to read and what each settles.
Read what it names, and the decisions the card cites, before the first
question. Then invoke the `grilling` skill and grill the human on the card.

Mode (D132): grill interactively, one question at a time, unless the card
carries `grill:batch` or the human asks for batch. Either way, switch when
the human asks:
- **Batch** ("batch the rest"): answer every remaining question yourself,
  verifying facts in the code as you would live, then show them all in one
  message, numbered, each with your pick and why, plus the decision and
  cards you'd write. The human accepts all or overrides by number; re-check
  anything an override changes before writing. Nothing is filed, marked
  ready, committed or pushed before they answer.
- **Back to interactive** ("discuss 3"): take that question out of the batch
  and grill it live, then carry on in whichever mode the human wants.

Outcome:
- Work and commit only in {worktree}.
- Write the decision as `docs/decisions/D<n>.md` (`## Decision` and `## Why`)
  plus its one-line row in `docs/wayfinding.md`. Take the next D number free
  on origin/main *and* in open PRs: check the highest D file after
  `git fetch`, and the D files each open PR adds
  (`gh pr list --state open --json number,files`).
- Show the human each impl card you'd file, in full, before filing it. Add
  `ready-for-agent` only to the cards the human says.
- Comment a three-line summary of the outcome on LH-{card}.
- Push and open the PR only when the human asks. Push as junderdo:
  `GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin HEAD`.
  The PR is titled `docs(LH-{card}): <summary>`, with `--assignee @me` and the
  card URL as the first line. Comment the PR link on the card and move the
  card to Review.

When the conversation ends (cards approved, PR open), tell the human you're
done and that this pane stays open until the merge, then run
`herdr agent focus {shepherd_pane}` to send them back. If that names no
pane, tell them to switch back themselves.

Close-out, once the PR merges: poll
`GH_TOKEN=$(gh auth token --user junderdo) gh pr view <pr> --json state`, or
on the human's word, check it the same way. When it reads MERGED:
1. `scripts/trello move LH-{card} Done`, then `scripts/trello unblock`.
2. Send the shepherd one line:
   `herdr agent prompt shepherd "LH-{card} merged and closed out: <PR URL>, D<n>, filed <cards or none>"`.
   If that fails, tell the human to relay the line.
Leave the worktree and branch: the herd's reap removes them and closes this
pane.
