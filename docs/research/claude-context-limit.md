# Claude Code context limit: facts for a "handoff and fresh session" feature

Researched 2026-11-03 against the live docs (fetched as markdown from
`https://code.claude.com/docs/en/<page>.md`) and loopherd at `main` (54ab76a).
Local Claude Code is 2.1.317 (`claude --version`); D16 probed 2.1.316. Docs
carry no per-page version stamp; version gates are quoted where the docs give
them. All URLs below are under `https://code.claude.com/docs/en/`.

## 1. Learning a session's current context usage

### 1a. Hook stdin: no usage field in any hook except two special cases

Common fields for every hook (hooks, "Common input fields"): `session_id`,
`prompt_id`, `transcript_path`, `cwd`, `scratchpad_dir` (v2.1.257+),
`permission_mode`, `effort`, `hook_event_name`. No token or window field.

Per-event inputs, from the same page:

| Event | Extra input fields | Token/window info? |
|---|---|---|
| Stop | `stop_hook_active`, `last_assistant_message`, `background_tasks`, `session_crons` | none |
| StopFailure | `error`, `error_details`, `last_assistant_message` | none |
| PreToolUse / PostToolUse | `tool_name`, `tool_input`, `tool_use_id` (+ `tool_response`, `duration_ms` on Post) | none |
| UserPromptSubmit | `prompt` | none |
| PreCompact | `trigger`, `custom_instructions` | none |
| PostCompact | `trigger`, `compact_summary` | none |
| Notification | `message`, `title`, `notification_type` | none |
| SessionStart | `source`, optional `model`, `agent_type`, `session_title` | only on `resume`/`fork`: `context_tokens` (v2.1.251+) |
| PreModelSwitch | `from_model`, `to_model`, ... | `context_tokens` (not a loopherd hook) |

The only context-size numbers a hook receives:
- SessionStart, `source` = `resume` or `fork` only: "`context_tokens` | Tokens
  the first request of the resumed session re-sends as its prompt" (hooks.md,
  "SessionStart input"). Never on `startup`, `clear` or `compact`.
- PreModelSwitch: "`context_tokens` ... the input, cache read, cache creation,
  and output tokens of the last response in the main conversation, combined"
  (hooks.md, PreModelSwitch input table).

Neither gives the window size. Conclusion: a hook has to compute usage itself
from `transcript_path` (1c), or read it from a statusline script (1b).

### 1b. Statusline JSON (best source; includes window size)

Source: statusline.md, "Available data" and "Context window fields". The
statusline command gets this on stdin (a `statusLine` setting of
`{"type":"command","command":...}`; optional `refreshInterval` seconds, min 1).

Field names (verbatim table rows):
- `context_window.total_input_tokens`, `context_window.total_output_tokens`:
  "Token counts currently in the context window, from the most recent API
  response. Input includes cache reads and writes"
- `context_window.context_window_size`: "Maximum context window size in tokens.
  200000 by default, or 1000000 for models with extended context."
- `context_window.used_percentage`, `context_window.remaining_percentage`:
  "Pre-calculated percentage of context window used" / "remaining"
- `context_window.current_usage`: `{input_tokens, output_tokens,
  cache_creation_input_tokens, cache_read_input_tokens}` "from the last API call"
- `exceeds_200k_tokens`: "This is a fixed threshold regardless of actual
  context window size."
- Also `session_id`, `transcript_path`, `model.id`, `cost.*`, `workspace.*`.

Formula, quoted: "The `used_percentage` field is calculated from input tokens
only: `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`.
It does not include `output_tokens`." And: "`total_input_tokens` is the sum of
`input_tokens`, `cache_creation_input_tokens`, and `cache_read_input_tokens`".

Caveats, quoted from "Fields that may be null":
- "`context_window.current_usage`: `null` before the first API call in a
  session, and again after `/compact` until the next API call repopulates it"
- "`used_percentage`, `remaining_percentage`: may be `null` early in the session"
- model-config.md: with `CLAUDE_CODE_AUTO_COMPACT_WINDOW` set, "The status
  line's `used_percentage` measures against the model's full context window, so
  while the variable is set, that percentage doesn't indicate when compaction
  will run." So compare token counts to your own threshold, not `used_percentage`.
- Update cadence: event driven, "debounces updates at 300ms"; cancels an
  in-flight script when a new update fires. A statusline script can only report
  while a TUI renders it; it is not a hook, but can write
  `context_window` to a file keyed by `session_id` for loopherd to read.

Not documented: a way to query this from outside the process without a
statusline script (no CLI/socket command for context usage). `/context` shows
a grid in the TUI only (commands.md); no machine-readable output documented.
OpenTelemetry token metrics exist (monitoring-usage.md, not read in detail).

### 1c. Transcript JSONL

- Location: sessions.md, "Where transcripts are stored": "stores transcripts as
  JSONL at `~/.claude/projects/<project>/<session-id>.jsonl`". Hooks and
  statusline both receive `transcript_path`.
- Lag warning (hooks.md common fields): "The transcript file is written
  asynchronously and may lag the in-memory conversation". Stop section: "the
  transcript file isn't guaranteed to include the final message at Stop time
  on all versions."
- Record schema for the JSONL: NOT DOCUMENTED on code.claude.com. I did not read
  any local transcript (told not to touch ~/.claude). The closest official
  statement is for the Agent SDK (agent-sdk/cost-tracking.md): assistant
  messages carry `message.usage` with `input_tokens`, `output_tokens`,
  `cache_creation_input_tokens`, `cache_read_input_tokens`; "all messages in
  that turn share the same ID, so deduplicate by ID"; "Per-step `output_tokens`
  is a placeholder"; "The deduplicated per-step values are accurate for input
  and cache tokens." That is the SDK message stream, not a documented transcript
  contract; treat on-disk shape as UNCONFIRMED until a probe.
- Computation consistent with the docs: take the LAST assistant record's usage,
  context = `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`
  (statusline's input-only formula; adding `output_tokens` gives the PreModelSwitch
  "next request re-sends" number). Subagent/sidechain records should be excluded
  (UNCONFIRMED how they are marked).

### 1d. Context window size (200k vs 1M)

- Statusline `context_window.context_window_size` is the only direct source (1b).
- model-config.md "Default auto-compact thresholds": native-1M models "compact
  before the window fills, at about 967K tokens by default"; "Sonnet 4.6 and
  Opus 4.6 without extended context compact at the 200K boundary";
  `CLAUDE_CODE_DISABLE_1M_CONTEXT=1` forces 200K; behind an unrecognized model ID
  Claude Code assumes 200K (`CLAUDE_CODE_MAX_CONTEXT_TOKENS` overrides, env-vars.md).
- Hooks do not report it; deriving it from model name in `SessionStart.model`
  is guesswork.

## 2. PreCompact, auto-compact threshold, disabling

PreCompact (hooks.md, "PreCompact"):
- Fires: "Runs before Claude Code is about to run a compact operation."
  Matchers: `manual` = `/compact`; `auto` = "Auto-compact when the conversation
  reaches the auto-compact window, and idle compaction".
- Input (verbatim example): `{"hook_event_name":"PreCompact","trigger":"manual",
  "custom_instructions":null, ...common fields}`. "For `manual`,
  `custom_instructions` contains what the user passes into `/compact` and is
  `null` when they pass nothing. For `auto`, `custom_instructions` is `null`."
- Can block: "Exit with code 2 to block compaction. ... You can also block by
  returning JSON with `"decision": "block"`." Effect: "If compaction was
  triggered proactively before the context limit, Claude Code skips it and the
  conversation continues uncompacted. If compaction was triggered to recover
  from a context-limit error already returned by the API, the underlying error
  surfaces and the current request fails."
- Cannot inject instructions: "Claude Code discards a PreCompact hook's
  `systemMessage` and `continue` fields." PreCompact is absent from the
  `additionalContext` delivery list (hooks.md, "Add context for Claude"), and
  no `custom_instructions` output field is documented. Delay: not documented
  (only block).
- Design note: blocking auto-compact once the window is near full is the
  failure path above, so blocking is only safe well below the limit.
- PostCompact (`trigger`, `compact_summary`) cannot affect the result.
  SessionStart with `source: "compact"` fires after compaction.

Threshold / disable (exact names, env-vars.md and settings-reference.md):
- `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`: "Set the percentage (1-100) of the
  auto-compact window at which auto-compaction triggers. Use lower values like
  `50` to compact earlier; the variable can't raise the threshold, so values
  above the default percentage are ignored."
- `CLAUDE_CODE_AUTO_COMPACT_WINDOW`: "Set the auto-compact window ... from
  `100000` to `1000000` tokens. Suffixed values such as `200k` require
  v2.1.304 or later." No suffix = tokens (`200` means 200 tokens). Precedence
  (model-config.md): env var > `--autocompact` flag > `/autocompact`
  (`modelSettings`) > `autoCompactWindow` setting. Capped at the model's window.
- `autoCompactWindow` setting (tokens 100000-1000000 or `"auto"`);
  `--autocompact <auto|tokens>` flag (v2.1.221+); `/autocompact` command.
- Disable: `DISABLE_AUTO_COMPACT=1` ("disable automatic compaction ... The
  manual `/compact` command remains available"; also turns off idle compaction);
  `autoCompactEnabled: false` setting; `DISABLE_COMPACT=1` disables auto AND
  manual. With auto-compact off, sessions "stop at the 200K boundary with the
  context-limit error" on a 200K window (model-config.md).
- Handy for loopherd: since the window can be set per launch via env, loopherd
  could start members with `CLAUDE_CODE_AUTO_COMPACT_WINDOW` or `--autocompact`
  (a `claude_args` entry) so its own handoff threshold is deterministic. Not
  tested.

## 3. Making Claude continue with an instruction from a hook

All from hooks.md.

Stop (decision control):
- `{"decision":"block","reason":"..."}`: "`"block"` prevents Claude from
  stopping ... `reason`: Required when `decision` is `"block"`. Tells Claude why
  it should continue." Exit 2 with stderr does the same ("Claude receives the
  stderr message as the explanation").
- Non-error variant: `{"hookSpecificOutput":{"hookEventName":"Stop",
  "additionalContext":"Please run the test suite before finishing"}}` -
  "The conversation continues so Claude can act on it, but unlike
  `decision: "block"` it is shown in the transcript as hook feedback rather than
  a hook error".
- Guards: input `stop_hook_active` is `true` "when Claude Code is already
  continuing as a result of a stop hook". "after stop hooks have continued the
  turn eight times in a row, Claude Code overrides the next block and ends the
  turn" (reset when Claude calls a tool; raise with `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP`).
- Note loopherd's installed command ends `|| true` (install_hook.rs:73-84), so a
  stop-hook exit 2 is deliberately swallowed today; JSON on stdout with exit 0
  would still be honored. The Stop hook also does not run on user interrupt;
  API errors fire StopFailure (no decision control).

Other `additionalContext` injection points (hooks.md "Add context for Claude"):
`{"hookSpecificOutput":{"hookEventName":"<Event>","additionalContext":"..."}}`,
wrapped in a system reminder, delivered:
- SessionStart: "at the start of the conversation, before the first prompt"
  (plain stdout also becomes context).
- UserPromptSubmit: "alongside the submitted prompt". (`decision:"block"` here
  blocks the prompt; `reason` is "Not added to context".)
- PreToolUse / PostToolUse / PostToolUseFailure / PostToolBatch: "next to the
  tool result". PostToolUse `decision:"block"` "adds the `reason` next to the
  tool result". These fire mid-turn, so they warn the agent without waiting for
  Stop, but only work while it is calling tools.
- Stop / SubagentStop: "at the end of the turn. The conversation continues".

So a hook can say "write your handoff now" via Stop `decision:"block"`+`reason`
(or Stop `additionalContext`), or PostToolUse/UserPromptSubmit `additionalContext`.
The Stop route needs the usage reading from 1b/1c in the hook, plus a
marker (file or ledger flag) so it fires once and the 8-cap/`stop_hook_active`
loop guard isn't tripped.

## 4. Fresh session

### 4a. Typing `/clear` into the input

- commands.md: "A command is only recognized at the start of your message. Text
  that follows the command name becomes its arguments." `/clear [name]`: "Start
  a new conversation with empty context ... Aliases: `/reset`, `/new`".
  "If you send a command while Claude is responding, Claude Code queues it and
  runs it after the current turn finishes."
- herdr: `agent prompt` "sends text followed by encoded Enter as one ordered
  submission" via bracketed paste (`herdr --skill` output, read-only). Whether a
  bracketed-paste `/clear` + Enter is executed as a slash command (rather than
  pasted text) is NOT DOCUMENTED by either side; needs a probe on a scratch
  agent. loopherd's own call is `agent prompt <name> <text>` (src/herdr.rs:438-440).
  `herdr agent send-keys` exists as a fallback for keystrokes.
- `agent prompt` refuses a blocked agent (`agent_blocked`); `--wait` needs
  observed `working`/`blocked` within 5 s or returns `agent_prompt_stalled`.
  A `/clear` may not make the agent "work", so use without `--wait`
  (loopherd's `prompt()` already does not wait, src/herdr.rs:123).
- After `/clear`: SessionStart fires with `source: "clear"` and a new
  `session_id` (hooks.md matcher table: "`clear` | `/clear`"). `model` "can be
  omitted, for example after `/clear`". SessionEnd also fires (reason `clear`,
  default 1.5 s timeout). SessionStart hooks "run in the background" on `/clear`;
  "nothing they return applies" if `/clear` is run again while still running.
  SessionStart `additionalContext` is therefore the natural channel for
  injecting the handoff note into the fresh session.
- statusline: `cost.total_cost_usd` "Resets to $0 when `/clear` starts a new
  session".
- Previous conversation stays resumable (`/resume`), commands.md.

### 4b. How loopherd already follows /clear

- `src/session_hook.rs` (whole file; header lines 1-3): SessionStart hook reads
  `source` and `session_id` (`parse`, lines 84-88), finds the live member by
  `$HERDR_PANE_ID` + socket, and `ledger.update(...)` sets `record.session_id`
  (lines 72-77). Adoption rule: with the member's Claude process known, any
  source from that process is adopted; otherwise only `clear` and `resume`
  (lines 44-70, `from_clear_or_resume`). A session owned by another live member
  is refused (lines 41-43).
- `src/stop_hook.rs:93-116` `follow_session` also updates `session_id` from any
  Stop from the member's process.
- Installed for SessionStart with no matcher, so all sources
  (`src/install_hook.rs:39-45`), timeout 10 s.
- docs/decisions/D16.md (~lines 55-75) states: session-hook adopts a session
  "whatever the source (`clear`, `resume`, `fork`, `startup`)" once the process
  is recorded; "An unclaimed member that runs `/clear` before its first stop is
  not followed". Probe facts for 2.1.316: `/branch` is "the same process with
  SessionStart `fork`, a new session id".
- So `/clear` on a member already keeps the ledger on the new session; the
  member keeps its name, pane, worktree, and process.

### 4c. Retire-and-respawn a lead instead

- `src/lead.rs`: `lead()` -> `resolve()` builds a `SpawnRequest` with
  `worktree: Some(Checkout::Detached("lead-<herd>"))`, `name = lead-<herd>`,
  prompt from `lead_config`, `role: Lead`. It refuses ("already has a lead ...
  retire it first", lead.rs:~84-90) while an unretired lead exists, and refuses
  if the name is held by an unretired member.
- `src/spawn.rs`: `spawn()` (line 157) creates the tab/worktree
  (`Checkout::ensure`, line 56), `start_until_ready`, sends the rendered prompt
  and records the ledger entry. A new `claude` process = new session with a
  fresh context.
- `src/retire.rs`: `retire()` (line 16) marks the ledger entry retired only;
  `retire_and_close()` (line 36) also closes the tab and removes a clean merged
  worktree. For a lead, the worktree is detached `lead-<herd>`; `close` "removes
  a clean detached worktree even when unmerged" (test at retire.rs:~502), but
  keeps one with uncommitted/untracked changes or mid-rebase. Plain `retire`
  leaves tab, agent and worktree alone (test ~345), so retire-then-`lead`
  would reuse the worktree: `ensure_detached_worktree` (src/worktree.rs:341-347)
  "Returns a worktree at `slug`'s path detached at origin's default branch,
  reusing a worktree already there." So respawn in the same worktree works, but
  the fresh lead lands on whatever is checked out there (not reset to origin's
  default), with any uncommitted changes intact.
- Alternative, `revive` (`src/revive.rs:326-352`) starts `claude --resume <session>`,
  which resumes the SAME context, so not suitable for a fresh start.
- Trade-off: `/clear` keeps pane, process and ledger entry (cheap, no new
  lock/name dance); respawn gives a guaranteed clean process but must
  re-run spawn and the hand-off must go into the new `config.prompt`.

## 5. What loopherd reads today

`grep -rniE "transcript|usage|token" src/`: no production code reads token
usage, `transcript_path`, or `context_window`. Only hits are the CLI usage
string (`src/herd.rs:475`), tests, and `gh auth token` in tool_hook tests.
`stop_hook.rs::parse` (lines 124-157) reads only `background_tasks`,
`session_crons`, `hook_event_name`, `error`, `session_id`; `session_hook.rs`
reads only `source` and `session_id`. PreToolUse (`tool_hook.rs`) reads the Bash
command. The hooks installed are exactly Stop, StopFailure, SessionStart,
PreToolUse (Bash matcher) (`src/install_hook.rs:22-52`); there is no
PreCompact, PostToolUse, UserPromptSubmit or statusLine installation.

## Unconfirmed (what was checked)

1. Transcript JSONL record schema and subagent/sidechain markers: official docs
   only name the file; checked sessions.md, hooks.md, cost-tracking.md.
   Not read: a real transcript.
2. Whether `agent prompt "/clear"` executes the slash command (herdr bracketed
   paste): checked herdr skill text and `agent prompt --help`; needs a probe.
3. Whether the statusline JSON is available to a process other than the
   rendering script (no documented IPC); a script writing a per-session file is
   the only workable path found.
4. Exact point at which Claude Code sends the SessionStart `context_tokens` for
   `clear` (documented only for `resume`/`fork`).
5. OpenTelemetry token metrics as an alternative feed (monitoring-usage.md not read).

## Sources

- hooks.md: https://code.claude.com/docs/en/hooks.md (Common input fields;
  SessionStart; Stop; StopFailure; PreCompact; PostCompact; Add context for
  Claude; exit code table)
- statusline.md: https://code.claude.com/docs/en/statusline.md (Available data;
  Context window fields)
- model-config.md: https://code.claude.com/docs/en/model-config.md ("Context
  window and auto-compaction")
- env-vars.md: https://code.claude.com/docs/en/env-vars.md
- settings-reference.md: https://code.claude.com/docs/en/settings-reference.md
  (`autoCompactEnabled`, `autoCompactWindow`)
- commands.md: https://code.claude.com/docs/en/commands.md
- sessions.md: https://code.claude.com/docs/en/sessions.md
- cli-reference.md: https://code.claude.com/docs/en/cli-reference.md (`--autocompact`)
- agent-sdk/cost-tracking.md: https://code.claude.com/docs/en/agent-sdk/cost-tracking.md
- herdr: local `herdr --skill`, `herdr agent prompt --help`; docs/research/herdr.md
- loopherd: src/session_hook.rs, src/stop_hook.rs, src/install_hook.rs,
  src/lead.rs, src/spawn.rs, src/retire.rs, src/revive.rs,
  src/herdr.rs, docs/decisions/D16.md
