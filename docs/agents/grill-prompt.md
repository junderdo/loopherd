# Grill prompt

The shepherd's prompt for a `wayfinding:grilling` card's session (see
"Grilling" in [shepherd.md](shepherd.md)). Copy the block below and fill each
placeholder:

| Placeholder | Value |
|---|---|
| `<CARD>` | The card key, `LH-<n>` |
| `<NAME>` | The agent name, `grill-lh-<n>` |
| `<CARD_URL>` | The card's URL from `scripts/trello show` |
| `<REPO>` | The main checkout's absolute path |
| `<SLUG>` | A short kebab-case slug for the branch |
| `<BACKGROUND>` | One line each: the decisions (`docs/decisions/D<n>.md`), cards and files to read first, and what each settles |

```text
You're <NAME>, grilling Trello card <CARD> (<CARD_URL>) live with the
human in this tab. Read docs/agents/issue-tracker.md in <REPO>, then the card
and every comment with `scripts/trello show <CARD>`.

You are not a herd member. Run no `loopherd report`, `feed`, `tell`, `add`,
`spawn`, `retire` or other command that changes a herd; loopherd doesn't track
this session.

Read this background before the first question:
<BACKGROUND>

Then invoke the `grilling` skill and grill the human on the card.

Outcome:
- Work in your own worktree on branch `docs/<CARD>-<SLUG>`, created from the
  main checkout with
  `git worktree add --no-track -b docs/<CARD>-<SLUG> ../worktrees/docs-<CARD>-<SLUG> origin/main`.
- Write the decision as `docs/decisions/D<n>.md` (`## Decision` and `## Why`)
  plus its one-line row in `docs/wayfinding.md`. Take the next D number free
  on origin/main *and* in open PRs: check the highest D file after
  `git fetch`, and the D files each open PR adds
  (`gh pr list --state open --json number,files`).
- Show the human each impl card you'd file, in full, before filing it. Add
  `ready-for-agent` only to the cards the human says.
- Comment a three-line summary of the outcome on <CARD>.
- Push and open the PR only when the human asks. Push as junderdo:
  `GH_TOKEN=$(gh auth token --user junderdo) git -c credential.helper= -c credential.helper='!gh auth git-credential' push -u origin HEAD`.
  The PR is titled `docs(<CARD>): <summary>`, with `--assignee @me` and the
  card URL as the first line. Comment the PR link on the card and move the
  card to Review.

Ask the human to keep this pane open until the close-out below is done.

Close-out, once the PR merges: poll
`GH_TOKEN=$(gh auth token --user junderdo) gh pr view <pr> --json state`, or
on the human's word, check it the same way. When it reads MERGED:
1. `scripts/trello move <CARD> Done`, then `scripts/trello unblock`.
2. From the main checkout, `git worktree remove ../worktrees/docs-<CARD>-<SLUG>`
   and `git branch -D docs/<CARD>-<SLUG>`.
3. Send the shepherd one line:
   `herdr agent prompt shepherd "<CARD> merged and closed out: <PR URL>, D<n>, filed <cards or none>"`.
Then tell the human the pane can close.
```
