# Claude Code — how workspace trust is decided

Checked 2026-10-31 against Claude Code 2.1.316 and the docs at
code.claude.com. Nothing was changed: `~/.claude.json` was only read.

## Answer

Trust is keyed **per git repository root, and a worktree uses its main
checkout's root**. So every worktree of a repo shares one trust record. Trust
in that record covers every existing and future worktree, wherever it lives
on disk.

| Where Claude starts | Key it reads and writes in `~/.claude.json` `projects` |
|---|---|
| Inside a repo (any subdirectory) | The repo root |
| Inside a linked worktree | The **main checkout's** root |
| Outside any repo | The start directory. Trust covers its subdirectories, except nested git repos |
| `$HOME` itself | Nothing. Trust lasts for that session only |

Trusting a parent directory does **not** cover a git repo nested under it.
The `projects/` or `<project>/worktrees/` container directories therefore
don't help.

## Sources

- [Permissions → Project allow rules and workspace trust][perm]:
  "In a repository, Claude Code keys the trust on the git repository root, so
  the trust covers the whole repository apart from any git repository nested
  inside it … In a worktree, it uses the main checkout's root." Outside a
  repo, trust covers subdirectories "apart from a git repository nested inside
  it". The same section gives the manual form:
  `projects["<path>"].hasTrustDialogAccepted: true` in `~/.claude.json`, with
  `<path>` the repository root.
- [Security → Additional safeguards][sec]: the dialog appears only in
  interactive sessions. A `-p` session never shows it. Trust accepted in the
  home directory isn't written to disk.
- [Worktrees][wt]: "Interactive runs require workspace trust … run `claude`
  once there to accept the trust dialog."
- The 2.1.316 binary agrees. The trust check (`hasTrustDialogAccepted`) looks
  up the project key, then walks up from the cwd but stops at the repository
  root.

[perm]: https://code.claude.com/docs/en/permissions#project-allow-rules-and-workspace-trust
[sec]: https://code.claude.com/docs/en/security#additional-safeguards
[wt]: https://code.claude.com/docs/en/worktrees#start-claude-in-a-worktree

## The LH-15 data point, explained

`~/.claude.json` has **no** key for any `loopherd/worktrees/*` path. It does
have `…/loopherd/loopherd: true`, the main checkout, though no Claude session
ever ran there (`~/.claude/projects` has no transcript for it). The trust
clicked in the worktrees landed on the main checkout's key.

The transcripts give the order:

| First message (UTC) | Worktree |
|---|---|
| 01:09:42.8 | `docs-LH-8-ledger-storage-adr` |
| 01:09:43.1 | `feat-LH-12-repo-default` |
| 01:09:43.5 | `feat-LH-13-spawn-output` |
| 01:40:43.5 | `feat-LH-17-retire` |

LH-8, LH-12 and LH-13 were spawned within one second. All three checked trust
before anyone answered a dialog, so all three blocked. One answer then
trusted the main checkout, and with it LH-17's worktree 30 minutes later. No
per-directory memory or sibling inheritance is involved.

## Consequence for loopherd

The one-time setup is: trust each repo's **main checkout** once. Every
worktree spawn into that repo then skips the dialog. Either:

- run `claude` in the main checkout (or in any one of its worktrees) and
  accept the dialog; or
- set `projects["<main checkout>"].hasTrustDialogAccepted` to `true` in
  `~/.claude.json` while no Claude session is writing it.

`git rev-parse --path-format=absolute --git-common-dir` prints
`<main checkout>/.git`, which gives the key.

The first spawns into a never-trusted repo can still block, and parallel
spawns block together. `loopherd prompt <name>` covers that case once the
human has answered.
