# Issue tracking

Work is tracked on the Trello board https://trello.com/b/MtiTIrp1/loopherd.
Reach it only through `scripts/trello` (`scripts/trello help` for commands).
Cards are keyed `LH-<number>`, and that key is the issue key in branch
names, PR titles, and commits.

## Board model

**Lists are the stage**, ordered left to right:

| List | Meaning |
|---|---|
| Backlog | Not started. Top of the list is highest priority. |
| Doing | Claimed. Someone is actively on it. |
| Review | Output is waiting for the human: an open PR, or a doc to read. |
| Done | Merged or accepted. |

**Labels are the kind of work and who acts next.**

Kind: a card carries exactly one of these.

| Label | Resolve it with |
|---|---|
| `wayfinding:research` | `research` skill: primary-source investigation written to `docs/research/` |
| `wayfinding:grilling` | `grilling` skill, live with the human. Always a human session |
| `wayfinding:prototype` | `prototype` skill: throwaway code that answers one design question |
| `wayfinding:domain` | `domain-modeling` skill: a decision recorded as an ADR or glossary entry |
| `impl:feat` | A vertical slice of behavior, built with the `tdd` skill |
| `impl:fix` | A bug. Start with the `diagnosing-bugs` skill |
| `impl:refactor` | A behavior-preserving restructure |
| `impl:chore` | Tooling, deps, CI, and docs that aren't wayfinding output |

Routing: add these as needed.

| Label | Meaning |
|---|---|
| `ready-for-agent` | Fully specified. Any agent may claim it unattended. |
| `need-human` | Stopped on a question, decision, or credential only the human can supply. The latest comment states exactly what is needed. |
| `blocked` | Waiting on another card. A comment names its `LH-` key. |

A card is **claimable** when it sits in Backlog, has `ready-for-agent`, and has neither `need-human` nor `blocked`.

## Working a card

1. **Pick.** Run `scripts/trello cards --list Backlog --label ready-for-agent`
   and take the first claimable card. Then run `scripts/trello show LH-<n>`
   and read the description and every comment.
2. **Claim.** Run `scripts/trello move LH-<n> Doing`, then comment
   `claimed by <agent name or HERDR_PANE_ID>`. A card already in Doing belongs
   to someone else, so pick another.
3. **Work.** Use the skill its kind label names. Implementation goes on a
   branch named `<type>/LH-<n>-<slug>`, where `<type>` comes from the `impl:`
   label (`feat`, `fix`, `refactor`, or `chore`). Wayfinding output goes on
   `docs/LH-<n>-<slug>`.
4. **Finish.** The card is finished when every acceptance criterion is met and
   the checks pass (`cargo fmt --check`, `cargo clippy -- -D warnings`,
   `cargo test`).
   - Open a PR titled `<type>(LH-<n>): <summary>` with the card URL as the
     first line of the description.
   - Comment the PR link on the card and move the card to Review.
   - Wayfinding cards also comment a three-line summary of the outcome.
5. **Stop cleanly** when you hit a question only the human can answer:
   - Run `scripts/trello label LH-<n> +need-human -ready-for-agent`.
   - Comment the specific question and the options you see.
   - Move the card back to Backlog.

   Stopping like this beats guessing at a decision.

Work you discover along the way becomes a new card in Backlog. Mention its
key in a comment on the card you're working; your own branch stays scoped to
the card you claimed.

## Writing a card

Use this description template:

```markdown
## Goal
One sentence: the observable outcome.

## Context
Why now, links to docs/wayfinding.md decisions or other LH- cards.

## Acceptance criteria
- [ ] Checkable statements, each verifiable by a test, command, or file.

## Out of scope
- What a reasonable agent might wander into but shouldn't.
```

Add `ready-for-agent` only when every acceptance criterion is checkable
without asking the human. Fuzzy criteria mean the card needs wayfinding
first, usually `wayfinding:grilling`.

```bash
scripts/trello create Backlog "Spawn a herd member from a template" \
  --label impl:feat --label ready-for-agent --desc-file card.md
```

## Setup

`scripts/trello` reads `TRELLO_API_KEY` and `TRELLO_TOKEN` from the
environment or from `~/.config/loopherd/trello.env`. `scripts/trello setup-labels`
creates or recolors the labels above and is safe to re-run.
