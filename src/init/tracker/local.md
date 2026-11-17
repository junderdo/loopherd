# Issue tracker: Local Markdown

Issues and specs for this repo live as markdown files under `.scratch/`.

## Conventions

- One feature per directory: `.scratch/<feature-slug>/`.
- The feature's spec is `.scratch/<feature-slug>/spec.md`.
- One file per implementation issue: `.scratch/<feature-slug>/issues/<NN>-<slug>.md`, numbered from `01`, never in one combined tickets file.
- Triage state is a `Status:` line near the top of each issue file, using the strings in `triage-labels.md`.
- Comments and the rest of the conversation append to the bottom of the file under a `## Comments` heading.

## When a skill says "publish to the issue tracker"

Create a new file under `.scratch/<feature-slug>/`, creating the directory if needed.

## When a skill says "fetch the relevant ticket"

Read the file at the referenced path. Usually the user hands over the path or the issue number.

## Wayfinding operations

`/wayfinder` works through these. Its **map** is one file, and each ticket is a **child** file beside it.

- **Map**: `.scratch/<effort>/map.md`, holding the Notes, Decisions-so-far and Fog sections.
- **Child ticket**: `.scratch/<effort>/issues/NN-<slug>.md`, numbered from `01`, its body stating the question. A `Type:` line names the ticket type (`research`, `prototype`, `grilling` or `task`), and a `Status:` line reads `claimed` or `resolved`.
- **Blocking**: a `Blocked by: NN, NN` line near the top. The ticket unblocks once every file it names is `resolved`.
- **Frontier**: look through `.scratch/<effort>/issues/` for files that are open, unblocked and unclaimed, and take the lowest number.
- **Claim**: set `Status: claimed` and save the file before starting.
- **Resolve**: write the answer under an `## Answer` heading, set `Status: resolved`, then add a pointer (a one-line gist and a link) to Decisions-so-far in `map.md`.
