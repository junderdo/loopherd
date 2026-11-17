# Issue tracker: GitHub

Issues and specs for this repo live in its GitHub Issues. Use the `gh` CLI for every
operation.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`, with a heredoc for a multi-line body.
- **Read an issue**: `gh issue view <number> --json number,title,body,labels,comments`.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'`, narrowed with `--label` and `--state`.
- **Make an issue a sub-issue of a parent**: create it with `gh issue create --parent <parent> ...`, or attach it later with `gh issue edit <parent> --add-sub-issue <child>` (needs `gh` 2.94 or newer). With an older `gh`, call `gh api --method POST repos/<owner>/<repo>/issues/<parent>/sub_issues -F sub_issue_id=<child-db-id>`, passing the child's database id as described under **Blocking**. A repo without sub-issues marks the child with a `Part of #<parent>` first line in its body.
- **Comment on an issue**: `gh issue comment <number> --body "..."`.
- **Apply / remove labels**: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`.
- **Close**: `gh issue close <number> --comment "..."`.

Inside a clone, `gh` finds the repo from `git remote -v` on its own.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Change to `yes` if external PRs count as feature requests here; `/triage` reads this flag.)_

With the flag at `yes`, PRs take the same labels and states as issues, through `gh pr`:

- **Read a PR**: `gh pr view <number> --comments`, plus `gh pr diff <number>` for the change itself.
- **List external PRs for triage**: `gh api --paginate 'repos/{owner}/{repo}/pulls?state=open' --jq '.[] | select(.author_association | IN("OWNER","MEMBER","COLLABORATOR") | not) | {number, title, author: .user.login, author_association, labels: [.labels[].name]}'`.
- **Comment / label / close**: `gh pr comment`, `gh pr edit --add-label` / `--remove-label`, `gh pr close`.

Issues and PRs draw from one sequence of numbers, so `#42` alone could be either: try `gh pr view 42` first, then `gh issue view 42`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Read it as in **Read an issue** above.

## Wayfinding operations

`/wayfinder` works through these. Its **map** is one issue, and each ticket is a **child** issue of it.

- **Map**: one issue labelled `wayfinder:map`, whose body holds the Notes, Decisions-so-far and Fog sections. Create it with `gh issue create --label wayfinder:map`.
- **Child ticket**: an issue attached to the map as a sub-issue (see **Make an issue a sub-issue of a parent**). In a repo without sub-issues, list the child in a task list in the map's body and start the child's body with `Part of #<map>`. Label it `wayfinder:<type>`, one of `research`, `prototype`, `grilling` or `task`. A claimed ticket is assigned to the developer driving it.
- **Blocking**: use GitHub's native issue dependencies, which the UI shows. Record that `<blocker>` blocks `<child>` with `gh api --method POST repos/<owner>/<repo>/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`. The id is the blocker's numeric database id from `gh api repos/<owner>/<repo>/issues/<n> --jq .id`; neither the `#number` nor the `node_id` works. `issue_dependencies_summary.blocked_by` counts only blockers still open, so it is the live gate. Without dependencies, start the child's body with `Blocked by: #<n>, #<n>`. A ticket unblocks once all its blockers are closed.
- **Frontier query**: take the map's open children (`gh issue list --state open`, limited to the map's sub-issues or task list), drop any that has an assignee or an open blocker (`issue_dependencies_summary.blocked_by > 0`, or an open issue on its `Blocked by` line), and pick the first left in map order.
- **Claim**: `gh issue edit <n> --add-assignee @me`, before any other write in the session.
- **Resolve**: comment the answer with `gh issue comment <n> --body "<answer>"`, close it with `gh issue close <n>`, then add a pointer (a one-line gist and a link) to the map's Decisions-so-far.
