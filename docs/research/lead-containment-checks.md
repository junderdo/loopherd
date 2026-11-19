# Lead containment checks (LH-279)

Run 2026-11-19 against D83 (LH-239): throwaway public repo `junderdo/lh279-containment-test` for GitHub, this WSL2 machine (kernel 6.18.33.2, Claude Code 2.1.328, gh 2.99.0, bubblewrap 0.9.0, herdr 0.9.5) for the sandbox. Everything is empirical, from commands run by an agent.

## Result

Every check passed or passed with a note. Nothing needs D83 to drop `credentials` mode. Four notes change how LH-280 and LH-281 should build it (see Consequences).

## GitHub

Token: fine-grained PAT, Pull requests RW, Contents R, Actions R, Commit statuses R, one repo. Rulesets: `main` (Restrict updates + Restrict deletions, bypass Repository admin "for pull requests only") and all branches (`non_fast_forward` only, no bypass).

| Check | Result |
|---|---|
| `gh pr merge` with PAT, auto-merge off | Fails: `GraphQL: Resource not accessible by personal access token (mergePullRequest)` |
| `gh pr merge --auto` with PAT, auto-merge off | Fails with the same `mergePullRequest` error |
| Both again with "Allow auto-merge" on | Same failures. The `--auto` run still reports `mergePullRequest`, not `enablePullRequestAutoMerge`: with no required checks, gh merges directly instead of queuing |
| Raw GraphQL `enablePullRequestAutoMerge` and `mergePullRequest`, REST `PUT pulls/N/merge`, auto-merge off and on | All refused: `FORBIDDEN` / HTTP 403 `Resource not accessible by personal access token` |
| `gh pr create`, `gh pr view` (plain and `--json state,mergeable,statusCheckRollup`), `gh pr comment`, `gh pr list`, `gh pr checks` | All work. `gh pr checks` on a PR with no checks says `no checks reported` and exits non-zero |
| `gh api` reads: `pulls/N`, `pulls/N/comments`, `commits/<ref>/check-runs`, `commits/<ref>/status`, `actions/runs` | All work |
| Deploy-key push to `main` | Refused by ruleset: `Cannot update this protected ref` |
| Deploy-key `git push --delete main` | Refused, but by git's own rule against deleting the default branch (`refusing to delete the current branch`), not the ruleset. The deletion rule was checked on a non-default branch (`del-test`, temporary ruleset with Restrict deletions): refused, `Cannot delete this branch` |
| Deploy-key push and `git push --delete` of a feature branch | Both succeed |
| Force-push ruleset, no bypass | Deploy-key `--force` to a feature branch refused: `Cannot force-push to this branch` |
| Same ruleset with a `DeployKey` bypass actor (`actor_id` null, `bypass_mode` always) | Force-push succeeds (`Bypassed rule violations`) |
| Owner merges a PR under the `main` ruleset | Works only with the bypass: plain `gh pr merge` says `the base branch policy prohibits the merge`; `gh pr merge --admin` merged. Run through gh (API), not clicked in the web UI |

Private-repo caveat: `gh pr view`/`gh pr checks` read `statusCheckRollup`, which needs `checks:read`, unobtainable for a fine-grained user PAT; per the gh maintainers it fails only on private repos. This repo is public, so it was not reproduced. A private herd repo needs the `--json` / Actions REST fallback (LH-281).

## Sandbox

Settings passed with `--settings`: `sandbox.enabled`, `failIfUnavailable`, `allowUnsandboxedCommands: false`, `network.allowAllUnixSockets`, `network.strictAllowlist` (allowlist: `api.github.com`), `permissions.denyInsteadOfPrompting`, filesystem `allowWrite` = the worktree and `~/.local/state/loopherd`, `denyWrite` = `~/.config/loopherd` and `~/.claude`, `denyRead` = `~/.config/gh`, and `Read(~/.config/gh/hosts.yml)` denied. Each run was `claude -p` driving a probe script through Bash.

| Check | Result |
|---|---|
| `failIfUnavailable` without bubblewrap | Refuses to start: `sandbox required but unavailable ... bubblewrap (bwrap) not installed` |
| herdr socket from Bash | Reachable (`HERDR_SOCKET_PATH=<loopherd-dev socket> herdr status` works) |
| Writes | Worktree and `~/.local/state/loopherd` succeed; `~/.config/loopherd/herds` and `~/.claude` fail with `Read-only file system` |
| Read deny | Read tool: `File is in a directory that is denied by your permission settings`. Sandboxed Bash `cat ~/.config/gh/hosts.yml`: `No such file or directory` (both the `Read(...)` rule and the `denyRead` entry were set together; each was not tried alone) |
| Seccomp filter | Present. Without `allowAllUnixSockets` the process gets 3 seccomp filters instead of 1, and the herdr socket is refused (`Operation not permitted`). `@anthropic-ai/sandbox-runtime` is not installed globally and `/sandbox` was not opened, so where the filter comes from is unconfirmed. `allowAllUnixSockets` drops the Unix-socket filter, which the herdr socket needs |
| `denyInsteadOfPrompting` and network | Covers network prompts: with `strictAllowlist` off, an unlisted host is refused unattended (`deny network-outbound example.com:443 (user denied)`) |
| Unlisted host, `strictAllowlist` on | Proxy answers `CONNECT tunnel failed, response 403`; violation `host is not on the allow list` |
| Listed host | `api.github.com` returns 200 |

## Consequences for D83

- No check failed; `credentials` mode stands and `hook` stays the default.
- Allowing all Unix sockets (needed for herdr) removes the seccomp socket filter, so a lead's Bash can likely reach other sockets the user can, such as an ssh agent (not tested). LH-280 should check whether `sandbox.network.allowUnixSockets` can name just the herdr socket, and test the agent socket.
- LH-280 should set both the `Read(...)` deny and the `denyRead` filesystem entry: D83 says file tools run outside the sandbox, and each was only tested together.
- The owner's merge under the `main` ruleset needs the bypass option (`gh pr merge --admin`, or "Merge without waiting for requirements" in the UI). Document it for `init`; the plain merge button reads as blocked.
- Deleting `main` is refused by git itself; only a non-default protected branch exercises the deletion rule.
- With no required checks, `gh pr merge --auto` merges directly, so the PAT's refusal is the only guard.

## How the setup ran

The agent that ran these checks created the write deploy key and rulesets itself, under a scoped allow rule the human added to its `.claude/settings.local.json` (`gh api repos/junderdo/lh279-containment-test/...`) after the auto-mode classifier denied the first attempt. The real D83 design must not rely on an allow rule like that: a lead must not be able to set up or widen its own containment.

## Cleanup

Done: the deploy key, all three rulesets and "Allow auto-merge" were removed. Remaining for the human: delete the repo (`gh repo delete junderdo/lh279-containment-test` needs their `delete_repo` scope) and revoke the `lh279-pr-only` token (it also expires 7 days after 2026-11-19).
