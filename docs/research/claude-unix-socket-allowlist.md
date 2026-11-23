# Does Claude Code's Linux sandbox enforce `allowUnixSockets` with a path list?

Probed 2026-11-21 (LH-363) on WSL2, Linux 6.18, Claude Code **2.1.330** (native binary and npm install).
Source accuracy fixes 2026-11-23 (LH-395); no new probes.

## Bottom line

**No.** Where Claude Code's seccomp filter is present (D105's filter check
finds where it is), a sandboxed Bash command on Linux can create no `AF_UNIX`
socket at all, so a path list never gets the chance to admit one. Where the
filter is missing, every socket is open and the list is still ignored.
Recommend **no new `unix_sockets` value** (not `"herdr"`): `"all"` stays the
only way to open sockets, and D114's refusal of it for `credentials` herds
stands.

## Sources

Claude Code settings reference, https://code.claude.com/docs/en/settings-reference
(fetched as `.md` 2026-11-23):

- `sandbox.network.allowUnixSockets`: "List the Unix socket paths sandboxed
  commands can connect to on macOS. Claude Code ignores this list on Linux and
  WSL2, where the seccomp filter can't inspect socket paths; use
  `allowAllUnixSockets` there instead."
- `sandbox.network.allowAllUnixSockets`: "On Linux and WSL2, the sandbox's
  seccomp filter blocks `socket(AF_UNIX, ...)` calls, so this is the only way
  to permit Unix sockets there. When the filter is missing, which `/sandbox`
  reports on its Dependencies tab, the sandbox doesn't block Unix-socket calls."

D89 already records that Claude Code ignores `allowUnixSockets` on Linux and
WSL2; the probe below confirms it on 2.1.330.

## Method

A throwaway server (python) listened on two sockets in a short dir, `listed.sock`
and `unlisted.sock`, and appended a line to `<name>.hits` on each accepted
connection. The ssh agent (`$SSH_AUTH_SOCK`) was the third target.
A probe script ran `socket.socket(AF_UNIX)` then `.connect(path)` for each
target in turn, run via `claude -p` (haiku, `--allowedTools Bash`) with
`--settings`:

```json
{"sandbox":{"enabled":true,"failIfUnavailable":true,"allowUnsandboxedCommands":false,
 "network":{"allowAllUnixSockets":false,"allowUnixSockets":["/tmp/lh363/listed.sock"]}}}
```

The control used the same file with `allowAllUnixSockets: true`.

Results were read from two places: `claude -p`'s final reply, saved to a file
per run, which relayed the probe's output (its per-target lines, or the
traceback with `Exit code 1`); and the server's `.hits` files, which gained a
line for each socket only at the control run's time.

| Run | listed | unlisted | ssh agent |
|---|---|---|---|
| Unsandboxed baseline | connected | connected | connected |
| Native 2.1.330, path list | `socket()` → `EPERM` | not reached | not reached |
| npm 2.1.330, path list | `socket()` → `EPERM` | not reached | not reached |
| Native 2.1.330, `allowAllUnixSockets: true` (control) | connected | connected | connected |

Every denied run failed at its first `socket.socket(AF_UNIX)` (`PermissionError:
[Errno 1] Operation not permitted`), before any `connect`, and the probe exited
there. `socket()` takes no path, so the denial happens before any target is
named: the unlisted and ssh-agent sockets would fail the same way, though the
probe didn't try them separately.

## Notes

- The npm run invoked `node_modules/.bin/claude`, which links to
  `@anthropic-ai/claude-code/bin/claude.exe`. Postinstall had replaced that
  stub with the `@anthropic-ai/claude-code-linux-x64` package's native binary
  (the same inode), so the npm run executed the native binary and the two
  installs are one test. The 2.1.330 package has no `vendor/seccomp/`
  directory; the native binary carries its own filter.
- **D105's npm sentence is outdated as of 2.1.330.** D105 says an npm-installed
  Claude Code looks for `vendor/seccomp/<arch>/apply-seccomp` and runs without
  blocking when it's missing. That doesn't hold for 2.1.330, whose npm package
  ships the native binary, which blocks sockets without it.
- Why a list can't work here: the filter denies `socket(AF_UNIX, ...)`, and a
  seccomp-bpf filter can't read the path in `connect`'s `sockaddr` (a
  user-memory pointer). The settings reference says the same ("the seccomp
  filter can't inspect socket paths").
- Limits: only WSL2 was probed, not bare-metal Linux (the docs treat them
  alike). The `allowAllUnixSockets: true` control ran on the native install
  only. macOS, where the list does apply, is out of scope. Not tested:
  `@anthropic-ai/sandbox-runtime` standalone, and Claude Code versions other
  than 2.1.330.

## Consequence

A sandboxed member needing the dev herdr socket (D107, `loopherd-cards` live
tests) must take `unix_sockets = "all"`, and so cannot be in a `credentials`
herd. Keeping its live tests out of the sandbox instead has no existing path:
D98's broker is one per lead, and D107's workers have none. A worker-side
broker, or another unsandboxed route such as `excludedCommands`, would be a
new design.
