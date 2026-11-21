# Does Claude Code's Linux sandbox enforce `allowUnixSockets` with a path list?

Probed 2026-11-21 (LH-363) on WSL2, Linux 6.18, Claude Code **2.1.330** (native binary and npm install).

## Bottom line

**No.** On Linux a sandboxed Bash command can create no `AF_UNIX` socket at all,
so a path list never gets the chance to admit one. Recommend **no new
`unix_sockets` value** (not `"herdr"`): `"all"` stays the only way to open
sockets, and D114's refusal of it for `credentials` herds stands.

## Method

A throwaway server (python) listened on two sockets in a short dir, `listed.sock`
and `unlisted.sock`. The ssh agent (`$SSH_AUTH_SOCK`) was the third target.
A probe script called `socket.socket(AF_UNIX).connect(path)` on each, run via
`claude -p` (haiku, `--allowedTools Bash`) with `--settings`:

```json
{"sandbox":{"enabled":true,"failIfUnavailable":true,"allowUnsandboxedCommands":false,
 "network":{"allowAllUnixSockets":false,"allowUnixSockets":["/tmp/lh363/listed.sock"]}}}
```

| Run | listed | unlisted | ssh agent |
|---|---|---|---|
| Unsandboxed baseline | connected | connected | connected |
| Native 2.1.330, path list | `socket()` → `EPERM` | not reached | not reached |
| npm 2.1.330, path list | `socket()` → `EPERM` | not reached | not reached |
| Native 2.1.330, `allowAllUnixSockets: true` (control) | connected | connected | connected |

Every denied run failed at `socket.socket(AF_UNIX)` (`PermissionError: [Errno 1]
Operation not permitted`), before any `connect`. The server saw no hits.

## Notes

- The npm package `@anthropic-ai/claude-code@2.1.330` resolved to the same
  native binary (`claude-code-linux-x64`), so the two installs are one test. Its
  `vendor/seccomp/` directory was absent (postinstall not run); the sandbox still
  blocked sockets, as D105 found for the native binary.
- Why a list can't work here: the seccomp filter denies `socket(AF_UNIX, ...)`.
  A seccomp-bpf filter can't read the path in `connect`'s `sockaddr` (it's a
  user-memory pointer), so a per-path allow can't be done in the filter. This
  is my inference, not a quoted doc. Claude Code's own settings check
  (`src/sandbox.rs`) already treats `allowUnixSockets` as a macOS-only reopen.
- Not tested: `@anthropic-ai/sandbox-runtime` standalone, and Claude Code
  versions other than 2.1.330.

## Consequence

A sandboxed member needing the dev herdr socket (D107, `loopherd-cards` live
tests) must either take `unix_sockets = "all"` (and so cannot be a `credentials`
herd) or keep live tests out of the sandbox, e.g. through a broker (D98).
