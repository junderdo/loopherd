# loopherd token efficiency: findings

Scope: read-only. Window analysed: lead/worker/shepherd/reviewer transcripts under `~/.claude/projects/`, "today" = sessions starting on UTC 2026-11-16 (7 lead sessions, 592 lead API calls; ~09:00-13:00 local), "history" = 2026-11-01..16. Every number is MEASURED unless marked (est.).

## Summary

1. Today's spend, at list price (proxy only; see "How measured"): about $172 = workers $69 + worker subagents $6, lead $26.5, lead's reviewer subagents $55, shepherd $15.8.
2. The two biggest levers are process, not output size: two PRs (#109/LH-114, #115/LH-132) took 11 and 10 review runs; reviewers + their workers on those two PRs cost about $65, roughly 38% of the day. Typical PRs cost $6-10 all-in. Each extra round costs about $3.4 (est.: reviewer $1.5 + worker $1.5 + lead $0.4).
3. Second lever: every Claude Code member starts at 50.8k tokens, 36k more than the 14k a reviewer subagent starts at. 28k+ of it is tool schemas and skill listings the members never use (Artifact tool alone is 37 KB of JSON). That is re-read on every call: about $11/day (est.).
4. Reviewer subagents are 32% of spend; 43% of that is output tokens, half of them thinking. Re-verify rounds run on Opus at default effort.
5. Lead: ~30% of its API calls are watch-protocol overhead (read the task-output file, re-arm). `loopherd reports --all` is 135 KB (899 lines) and the handoff prompt tells the lead to run it verbatim.
6. Raising the lead's `context_limit` to 300k raises its per-call cost about 60% (est., arithmetic below); the measured cost-minimum is around 100-150k.
7. Not problems (measured): cargo output (1.9% of worker carry), prompt-cache misses (about $4/day total), hook messages (loopherd hooks add no context), subagent hand-backs after LH-156 (1-1.6 KB).

## How measured

- Each assistant message in the JSONL carries `usage`; one API response is split over several lines with the same `message.id`, so I dedupe by id. Context per call = `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` (the API docs say `input_tokens` counts only tokens after the last cache breakpoint: https://platform.claude.com/docs/en/build-with-claude/prompt-caching, "Does input_tokens include cached tokens").
- Tool results: matched `tool_use_id` to the tool call, classified Bash commands by regex. Tokens = chars / 2.3, calibrated on 123 single-result call pairs (ctx delta minus the previous call's output tokens vs result chars). Tool-schema JSON calibrates at ~2.85 chars/token. Treat per-category token numbers as +-25%.
- "Carry" = tokens x number of later API calls in that session, i.e. cache-read volume. It is the best proxy for what a big result costs after it lands; $ weights differ (a token is written once at 8/M, read many times at 0.2/M).
- Dollars use the API price page (https://platform.claude.com/docs/en/about-claude/pricing): Opus 5.5 $4 in / $5 5m write / $8 1h write / $0.20 read / $20 out per MTok; Sonnet 5.5 $2 / $2.50 / $4 / $0.10 / $10; Haiku 5.5 (<=100k prompts) $0.10 / $0.125 / $0.20 / $0.01 / $0.50. Transcripts show 1h cache writes on main conversations and 5m on subagents, which matches the docs' default for a Claude subscription within plan usage (https://code.claude.com/docs/en/prompt-caching, "Which TTL each request gets"). So you are probably on a plan, and $ here is a relative proxy, not a bill (UNCONFIRMED: how plan usage weights cache reads vs output).
- Subagent transcripts: `<session>/subagents/agent-*.jsonl` plus `.meta.json` (agentType, description).
- `loopherd stats --json`, `loopherd reports --all`, `loopherd status`, `loopherd --skill` (read-only) for rounds and output sizes.

## 1. Where the tokens go

### Spend by role (list-price proxy)

| Role | Today | History (Nov 1-16) | Cost mix (today) |
|---|---|---|---|
| Lead (7 sessions, 592 calls) | $26.5 | $79 | cache read 50%, 1h cache write 30%, output 20% |
| Lead's subagents (36 reviewers, 2 general-purpose) | $55.0 | $101 | output 43%, 5m write 32%, read 25% |
| Workers (30 sessions incl. live-test sessions) | $68.8 | $312 | read 40%, 1h write 31%, output 29% |
| Worker subagents | $6.2 | $23 | |
| Shepherd | $15.0 (+$0.8 subagents) | $62 (+$9) | read 48%, write 34%, output 18% |

### Lead, 7 sessions today (carry = cache-read volume)

| Category | Calls | Tokens added | % of carry |
|---|---|---|---|
| Baseline at first call (50.8k x 590) | | 355k | 56.1% |
| Assistant output (tool inputs, text, thinking) | | 251k | 18.7% |
| `loopherd status` | 117 | 55k | 5.4% |
| `loopherd reports` | 33 | 31k | 4.7% |
| `loopherd --skill` (3 Bash loads + 4 Skill-tool loads) | 3 | 14.5k Bash; 10.7k per load | 2.6% |
| cat/sed of scratch files | 49 | 27k | 2.4% |
| `loopherd watch` output | 121 | 25k | 2.2% |
| git | 28 | 20k | 1.9% |
| Agent launch acks | 37 | 18k | 1.7% |
| trello, gh, herdr, handoff file, cargo | | ~40k | ~3.5% |

Of the tokens the lead adds after its baseline (about 490k over 7 sessions), assistant output is 51%: Bash command text 43k tokens, Agent prompts 41k (39 prompts, ~3 KB each), Write of fix lists 31k (28 files), `herdr agent prompt` text 16k (est. from chars/2.8). Growth is about 1.2k tokens per call, 50.8k -> 155-161k in ~85 calls (~30 min) = the 150k `default_context_limit` in `~/.config/loopherd/config.toml`. That is what triggered the three back-to-back handoffs. Session 1b03bb2d (new 300k limit) was at 129k after 53 calls.

Handoff re-orientation each time, measured: calls 1-4 take the context from 50.7k to ~66k (+15k): `loopherd --skill` 10.7k, handoff file ~2k, status + `reports --all | tail -40` ~3k. At 85 later calls that is about $0.40 per handoff, $2.8 for today's 7 (est.).

Watch protocol: of 608 lead API calls, 115 are a lone `cat <task>.output` (the background-task notification carries no output; docs: "Output is written to a file and Claude can retrieve it using the Read tool", https://code.claude.com/docs/en/interactive-mode, "How backgrounding works"), 62 are a lone re-arm, 56 are a re-arm plus another command. Together 29% of calls; at ~$0.027 each (small outputs) that is $4.8 of $26.5 (est.).

### Workers, 20 sessions >=20 calls today (1,173 calls)

| Category | % of carry |
|---|---|
| Baseline (first-call ctx 50.9k) | 44.5% |
| Assistant output (1.0M tokens, 35% of it thinking) | 24.3% |
| Reading repo files via Bash cat/sed/grep (362 calls, 569k tokens) | 21.3% |
| git 2.4%, trello 2.1%, cargo 1.9%, loopherd 1.5%, Read tool 0.9% | |

File re-reads: `tool_hook.rs` read 31 times (63k tokens) and `main.rs` 19 times (57k) across today's workers; `tests/init.rs` 17, `src/init.rs` 17. The workers read in ranges (`sed -n a,bp`), so this is re-reading across fix rounds and handoffs, not whole-file dumps. cargo output is not a problem (237 calls, 78k tokens total).

Live-suite waste: `tests/live.rs` spawns real Claude sessions on the default model (Opus 5.5, full 50k baseline). 376 such sessions in history ("Run `sleep 30`...", "Reply with just the word ok") cost $42 (7% of all spend); LH-113 alone $21.85 (172 sessions), LH-74 $6.3. None today. Only one test passes `--model haiku` (tests/live.rs:1166).

### Shepherd, 3 sessions today

Baseline 49%, output 15%, `loopherd --skill` 9.8% (4 loads, 8-12k tokens each), other loopherd commands 6.4%, status 5.1%, reports 3.6%. Startup reads (HANDOFF.md + shepherd.md + skill + handoff) cost 20-30k tokens per session. `gh pr view --json body,files` returned 9-11k tokens twice.

### Reviewer subagents, 36 runs today (1,040 calls)

Start at 14k tokens (4 tools, no skill listing) vs 50.8k for main sessions. Carry: output 25.7%, baseline 21.9%, file reads via Bash 21.5%, git 16.5% (diff output), Read tool 8.3%, cargo 1.7%. Output is 1.2M tokens, 49% of it thinking (591k tokens = $11.8 at list). Per run $0.1-3.8, median about $1.4.

Reading persisted outputs: 8 reviewer `Read` calls of `.../tool-results/*.txt` (14-22k tokens each; the Bash 30,000-char inline ceiling spills larger outputs to a file with a 2,000-char preview, https://code.claude.com/docs/en/tools-reference, "Output limits") undo that truncation. 8 x ~15k tokens, carried ~40 calls: 3.9M carry-tokens, about $1.4/day (est.).

Reviews per PR today: #109 11 runs/$17.8, #115 10/$15.2, #117 4/$4.1, #118 4/$4.1, #113 2/$3.3, #116 1/$2.8, #107 2/$1.9, #108 1/$1.9, #110 1/$1.7. Earlier days: #84 7 runs/$8.6, #104 4/$6.1; typical 1 run/$0.3-1.4.

### Context-size facts about every member session (baseline = 50.8k)

From the first `api-request-shape` and attachments of lead 957f4a35:

| Part | Size | Approx tokens (chars/2.85) |
|---|---|---|
| Tool schemas (16 tools) | 77.9 KB, of which Artifact 37.4, SendFeedback 5.6, Workflow 5.5, AskUserQuestion 5.4, Agent 4.9, ScheduleWakeup 4.8, Bash 3.3, ReportFindings 2.3, Read 2.0, Skill 1.9, ListAgents 1.2 | ~27k |
| System prompt | 7.3 KB | ~2.5k |
| CLAUDE.md + 11 rule files + AGENTS.md | ~16.5 KB | ~5.8k |
| Skill listing (67 skills, mostly `anthropic-skills:*`) | 26.8 KB | ~9.4k |
| Deferred-tool names + agent list + MCP instructions | 12.6 KB | ~4.4k |
| A user-level SessionStart hook (not loopherd's) | ~1.2 KB | ~0.4k |

The shared prefix is cached across sessions in the same directory (first call: ~36.5k read, ~14k written; docs "Cache scope"), so the baseline cost is mostly the cache read paid on every call, not a re-write per session. Workers' first-call context is the same 50.9k (median of 29), shepherd 50.4k.

## 2. Verbose outputs and instructions

| Item | Size | Evidence | Note |
|---|---|---|---|
| `loopherd reports --all` | 135,317 B, 899 lines (500 `stop`, 150 `done`, 89 `start`, 45 `question`, 37 `handoff`) | ran it | grows forever. The restart/revive prompts (`src/restart.rs:228`, `src/revive.rs:15`, LEAD_REARM) tell the lead to run it verbatim. Claude Code spills >30,000 chars to a file plus 2 KB preview, so the lead then re-runs with `| tail -40` (seen in every session: 2.5-3.8k tokens per read, 33 reads). A lead that `Read`s the spilled file would add ~58k tokens. |
| `loopherd --skill` | 24,579 B / ~10.7k tokens | measured load; 400 lines | The Lead section (lines 308-400) is ~2.2k tokens (one lead ran `sed -n '/^## Lead/,$p'`). Every restart reloads all of it (7 lead loads today = $2.1, shepherd 4 loads = $1.4, est.). |
| `loopherd status` | 640 B | ran it | fine; called 117 times because the skill says re-read on each wake. 470 tokens per call x 117 = 55k tokens, 5.4% of carry. |
| `loopherd watch` wake | 471 B result of arming + a separate `cat` of the task output | | the notification has no payload (see above). |
| `herdr agent prompt ... && herdr agent list \| grep` | 15.9 KB once (all workspaces' agents as one-line JSON; `grep` matches the whole line) | lead call 14 of session 1b03bb2d | one-off: ~5.6k tokens carried ~60 calls = ~$0.14. Use `loopherd status \| grep`. |
| Lead prompt file `loopherd-cards-lead.md` | 1.7 KB | | already lean. It says "Read `loopherd --skill` in full", which is the 10.7k-token load. |
| Reviewer final reports before LH-156 | 74 KB in session 46bb8e76 (3 hand-backs of 21-29 KB, ~26k tokens) | queued_command attachments | after LH-156 the other 6 sessions have none >1.6 KB. Already handled; keep. |
| Hook messages (`loopherd stop-hook`, `session-hook`) | none added to context | grep `additionalContext` in src: no matches; transcripts show only a user-level SessionStart hook | the Stop-hook handoff request is ~700 chars once per session. |
| Prompt templates | card prompt ~2.5 KB, wayfinding ~1.2 KB | herd file | lean. |

Review protocol text: the reviewer agent definition (`agents/reviewer.md`) has no effort, no output cap, no "tail gate output" rule, and no scope limit beyond "Hunt for bugs the tests miss". The lead's per-round reviewer prompts run ~3 KB (39 today) and re-list probe tooling paths each time.

## 3. Process-level waste

**Rounds that do not converge (largest single item).** `loopherd stats --json`: 54 retired members with a done; rounds distribution {1: 8, 2: 31, 3: 9, 4: 3, 5: 2, 9: 1}, median 2, mean 2.37. Rounds beyond 3 total 13 across history, plus today's two open PRs (8+ rounds each, ~10 beyond 3). Today's two outliers:

- #109 / LH-114 (notification redaction heuristics): 8 fix rounds. Fix lists show each round's fix opening a new leak of the same class: round 5 passphrase clause rule, round 7 curly quotes, round 8 "curly-quote change caused two leaks that round 6 didn't have" (fixes8.md, item 1: apostrophe ends a quote). Cost: reviewers $17.8 + worker $19.8.
- #115 / LH-132 (lead PreToolUse hook parsing Bash text): 8 rounds. fixes8.md: "The human chose to fail closed instead of chasing exact bash parsing... Every round has opened new gaps, so stop extending the lexer and add a fail-closed rule." Cost: reviewers $15.2 + worker $12.4.

Both are open-ended-surface problems (redaction heuristics, shell lexing) where a reviewer told to "hunt for bugs the tests miss" always finds more, and each fix introduces regressions. Typical PRs cost $6-10 all-in; these two cost ~$65 (38% of today). The human's intervention at round 8 is the fix that should have happened at round 3-4.

**Re-running reviews after handoffs.** Four reviewer runs were cut short by handoffs today (calls 2, 4, 9, 6; $0.08-0.25 each) and relaunched (3 confirmed duplicate descriptions: #115 round 5, #109 round 7, #118). The waste is about $0.5 plus the lead's re-orientation ($0.4 each); the main harm is latency and lead confusion, not dollars. Whether `/clear` kills background subagents is observed here but not stated in the docs; the docs do say compaction keeps them ("Background commands and background subagents: Keep running", https://code.claude.com/docs/en/context-window, "What survives compaction").

**Reviewer re-verifies re-run everything.** The round-6 reviewer prompt asks for p1..p13 probe reruns, mutation runs, and the full gate (fmt/clippy/test) on a 50-line delta; round 7 does it again. A re-verify of a 50-line delta cost $1.2-2.1 (28 re-verifies today, ~$40).

**Workers re-reading.** See section 1; modest (tool_hook.rs 63k tokens over 31 reads is ~$0.9).

**Cargo output in context.** Not a problem; workers cap it already (tail).

**Subagent replies.** Fixed by LH-156 (observed 1-1.6 KB since). The remaining reply-adjacent cost is the lead retyping findings into `fixesN.md` (31k output tokens, ~$0.9 + carry) which LH-156's PR-comment route removes.

**Lead context limit.** Per-call cost model (est.; growth 1.2k tokens/call, baseline+orientation 65k, 1h write $8/M, read $0.20/M, ~$0.13 re-orientation):

| limit L | calls/session | avg context | read $/call | re-orient $/call | total $/call |
|---|---|---|---|---|---|
| 100k | 29 | 82k | 0.0165 | 0.0045 | 0.021 |
| 150k | 71 | 107k | 0.0215 | 0.0018 | 0.023 |
| 300k | 196 | 182k | 0.0365 | 0.0007 | 0.037 |

At 618 calls/day that is +$8/day for 300k vs 150k (about +30% of lead cost) if the lead runs to the limit. It buys fewer handoffs, which only pays if handoffs are what hurts (they cost ~$0.4 plus killed work). Compaction is not the alternative here: with a native 1M window Claude Code compacts at about 967k by default (https://code.claude.com/docs/en/model-config, "Default auto-compact thresholds"), so loopherd's Stop-hook limit is the only thing bounding context.

**Cache.** Miss-like events (cache_creation >50% of a >30k context after call 1): lead 1, shepherd 1, workers 0, reviewers 6 (~$2.2). Not worth tuning; setting `subagentPromptCacheTtl` to 1h would raise reviewer write cost 60% (5m write $5 -> 1h $8) to save ~$2.

## 4. Recommendations, ranked by saving per effort

Savings are per day at today's load unless noted, list-price proxy, (est.) unless measured.

### A. Prompts / skill / process (no code)

**A1. Round cap and a stop-and-ask rule for non-converging PRs.** Where: lead prompt file (`~/.config/loopherd/herds/loopherd-cards-lead.md`, `docs/examples/loopherd-cards-lead.md`) and skill.md "Verify" step; reviewer prompt/agent. Text: blocking = acceptance criterion unmet, or a regression against main with a concrete failing command; anything else becomes a follow-up card. After round 3, or when a round's findings are regressions of the previous fix in the same class, send a `--kind question` with options (fail closed / narrow scope / merge with follow-up card) instead of another round. Saving: #109 and #115 rounds 4-8 = 10 rounds x $3.4 = ~$34 today (est.); history average 23 excess rounds / 62 cards x $3.4 = ~$1.3 per card (est.). Risk: ships with known gaps; mitigated because merge is the human's and the gap becomes a card. Effort: low (an hour of prompt text). Rank 1.

**A2. Re-verify rounds check the delta only.** Reviewer prompt for round >=2: read fixesN.md and `git diff <prev>..<head>`, run the gate once, no full probe reruns unless the delta touches them, write the findings file, return one line. Today's re-verifies avg $1.4; assume 30% off = ~$12 (est.). Risk: misses an interaction outside the delta; the first full review and one final full pass remain. Effort: low.

**A3. Re-arm the watch in the same message as the first action after a wake**, and never as a solo call. Where: lead prompt step 3 / skill Lead "Heartbeat". Saves up to 62 solo calls x $0.025 = ~$1.6 (est.). Risk: a feed or after-merge step races the freshly armed watch; the watch only marks events read when printed, so low. Effort: trivial.

**A4. Lead orientation after restart reads only what it needs.** Change the restart prompt (`src/restart.rs` LEAD_REARM, `src/revive.rs`) to: read the handoff, `loopherd status`, `loopherd reports --all <open members> | tail -30`, and `loopherd --skill | sed -n '/^## Lead/,$p'`. Saves ~8k tokens per restart x ($8/M + 85 x $0.2/M = $0.025/k) = ~$0.20 per restart, $1.4/day (est.). Better as code (D1/D2 below). Effort: trivial as text; risk: Lead section refers to the Shepherd section's task life, so keep a 10-line "task life" digest in the Lead section.

**A5. Reviewer agent definition (`agents/reviewer.md`).** Add: pipe gate output (`cargo test 2>&1 | tail -30`), never `Read` a `tool-results/` file, use `git diff --stat` first, return one line (already LH-156). Saves ~$1.4/day (est.); effort trivial.

**A6. Briefing file per PR.** Lead writes `pr<n>/brief.md` once (card, AC, probe tooling paths, prior findings index); round prompts shrink from ~3 KB to ~600 chars. 39 prompts x ~700 tokens = 27k output+carry tokens, ~$1.3/day (est.). Low value, do after A1-A3.

### B. Claude Code settings / launch args (no loopherd code; herd-file `args`)

**B1. Strip unused tools and skills from member sessions.** In `~/.config/loopherd/herds/loopherd-cards.toml`, top-level `args`:
`["--permission-mode","auto","--disallowedTools","Artifact,SendFeedback,Workflow,AskUserQuestion,ReportFindings,ListAgents,ScheduleWakeup","--settings","{\"skillOverrides\":{...all but tdd, diagnosing-bugs, research, grilling, domain-modeling, prototype, loopherd: \"off\"}}"]`.
Docs: `--disallowedTools` "A bare tool name removes the matching tools from Claude's context" (https://code.claude.com/docs/en/cli-reference); `skillOverrides` "off" hides the skill from the listing, `--settings` "Keys you omit keep their file-based values" so the user-level loopherd hooks stay (https://code.claude.com/docs/en/skills, https://code.claude.com/docs/en/cli-reference). `--setting-sources` and `--bare` would drop the hooks loopherd needs, so do not use them. Also `--disallowedTools "mcp__*"` removes the MCP tool names and instructions (~2k tokens, est.).
Saving: tools ~57 KB/2.85 = ~20k tokens + skill listing ~24 KB/2.85 = ~8.4k tokens = ~28k per call. Lead 618 calls x 28k x $0.2/M = $3.5; workers 1,252 x 28k x $0.2/M = $7.0; plus session-start writes ~$1 => ~$11.5/day (est.). Check: reviewers already start at 14k, so ~14-15k is the achievable floor for tool-limited sessions. Risk: a member needing a removed tool (AskUserQuestion has no human in the pane anyway; ScheduleWakeup/Workflow unused by the lead per transcripts: tools-used lists Bash, Agent, Read, ToolSearch, TaskStop, Write, Skill, SendMessage). Not verified live: run `claude -p ... --output-format json --disallowedTools ...` from a temp dir and compare `usage` to confirm the drop (I did not, because the user-level hooks would also fire). Effort: low (herd-file edit, restart members).

**B2. Reviewer subagent model/effort.** In `agents/reviewer.md` frontmatter: `model: sonnet` for re-verify rounds (the lead picks per call: first review Opus, re-verifies Sonnet; the Agent tool's per-invocation `model` parameter takes precedence, https://code.claude.com/docs/en/sub-agents "Model selection") and `effort: low` (the subagent `effort` field exists, same page). Sonnet 5.5 is exactly half Opus 5.5 on every price (above). 28 re-verifies x ~$1.4 = ~$40 -> ~$20 saved (est.); effort low on thinking (591k tokens = $11.8) maybe -30..50% = $3.5-6 (est., UNCONFIRMED effect size). Risk: weaker adversarial probing on exactly the PRs that need it (#115). Mitigate: keep Opus for round 1 and the final confirm. Effort: low; measure with `loopherd stats` rounds.

**B3. Live tests on Haiku.** In `tests/live.rs` `spawn_hooked_with` and the other `spawn(...)` calls add `--model haiku` (as line 1166 already does) and `--disallowedTools` the big tools. History: 376 sessions, $42; Haiku 5.5 input is 40x cheaper ($0.10 vs $4 /MTok) => ~$41 saved over history, ~$0.8 per full suite run (est.). Risk: a live test depends on Opus-specific behaviour (background-task reporting, Stop hook payload); test 1166 already runs on Haiku. Effort: trivial. Zero today, but LH-113 burned $22 in one card.

**B4. Per-role model.** Lead on Sonnet 5.5 via `[lead] args = ["--model","sonnet"]` (the lead merges nothing, the human merges): $26.5 -> $13.3 (est., exactly 0.5x), -$13/day. Risk: judgement on reviewer findings and merge-question wording; try for a day and compare rounds in `loopherd stats`. Workers: `--model sonnet` per template for `impl:chore`/`docs`/`refactor` (wayfinding already does). Per docs "Sonnet handles most coding tasks well and costs less than Opus. Reserve Opus for complex architectural decisions" (https://code.claude.com/docs/en/costs). Size depends on the card mix (unmeasured). Effort: low, but it is an experiment.

**B5. Do not change.** `subagentPromptCacheTtl`, effort on the lead (thinking is 9% of its output), auto-compact (default 967k, loopherd's limit governs).

### C. loopherd code

**C1. Bound `loopherd reports --all`.** Add `--since <time>` / `--tail <n>` / default to non-retired members, update the LEAD_REARM and revive prompts, `src/skill.md` ("Commands", "On each wake"). Removes a 135 KB (~58k token) footgun; saves ~$1-1.5/day today (the lead already tails; the measured 31k tokens of reports = 4.7% of lead carry). Risk: shepherd muscle memory; keep `--all` meaning everything with an explicit flag. Effort: low-medium.

**C2. Defer the handoff request while subagents are in flight.** `src/context.rs::ask` gets the Stop input (`background_tasks` already parsed at `src/stop_hook.rs:177`): if any `type == "subagent"` is running and tokens < 1.5 x limit, return without writing the `handoff-asked` marker; the next Stop (after the reviewer's hand-back wakes the lead) asks. Saves ~$0.5/day wasted partial reviews plus ~$0.4/handoff re-orientation avoided when the review result lands first; the real gain is no killed reviews and no duplicate relaunches. Effort: low-medium; test in `context.rs` unit tests. With this, set lead `context_limit` back to 150-200k (saves up to ~$8/day vs 300k, est.).

**C3. Role-sized skill output.** `loopherd --skill lead` / `--skill shepherd` (or sections) printing only the role's text plus a short common part; Lead + the parts the lead uses (task's life, handoffs, talking to members) is ~4-5k tokens vs 10.7k. Saves ~5.5k x $0.025/k = $0.14 per load; 7 lead + 4 shepherd loads/day = ~$1.5/day (est.). Also shortens what the restart prompt asks for (A4). Effort: medium (restructure skill.md, tests/skill.rs pin text). Risk: agents miss a rule that lived in the other role's section.

**C4. Streaming wake instead of exit-and-rearm (higher effort, uncertain).** `loopherd watch --follow` that never exits and prints one line per event, run via the Monitor tool (https://code.claude.com/docs/en/tools-reference "Monitor tool": "feeds each output line back to Claude"; limit "5 minutes by default, at most 30 minutes", re-arm once per deadline). Removes the `cat` call (115/608 calls) and the re-arm (62+56): up to ~$4.8/day (est., 18% of lead cost). Risks: Monitor runs "outside" the lead's PreToolUse Bash hook semantics (the docs say it uses Bash's permission rules; confirm `src/tool_hook.rs` matcher covers it), the lead's `paused`/`finished` stop detection, `watch.lock`, 30 min deadline. Effort: high. Do only after A/B/C2.

**C5. Lead `claimable`/`merged` auto-feed.** Watch could run `feed` itself in lead mode instead of waking the model for it (feed + after_merge are already deterministic). Would remove a share of the ~121 wakes/day, but a design change (D28/D36 territory); mention only.

### Ranking (saving per effort)

| # | Change | Kind | Est. saving/day | Effort |
|---|---|---|---|---|
| 1 | A1 round cap + fail-closed escalation | prompt | $10 typical, $34 on a day like today | low |
| 2 | B1 strip tools/skills in member args | settings | ~$11.5 | low |
| 3 | B2 reviewer Sonnet for re-verifies + effort low | settings/agent file | $20 + $4-6 | low |
| 4 | A2 delta-only re-verify | prompt | ~$12 | low |
| 5 | B3 live tests on Haiku | test code | $41 history (0 today) | trivial |
| 6 | B4 lead on Sonnet | settings | ~$13 | low, experiment |
| 7 | C2 defer handoff with subagents in flight, then lower lead limit | code | ~$1 + up to $8 | low-med |
| 8 | A3/A4/A5 small prompt edits | prompt | ~$1.6 + $1.4 + $1.4 | trivial |
| 9 | C1 bound `reports --all` | code | $1-1.5 + removes footgun | low-med |
| 10 | C3 role-sized skill | code | ~$1.5 | medium |
| 11 | C4 Monitor-based watch | code | up to $4.8 | high |

Savings do not add (B2/A2/A1 overlap on reviewer cost). Total plausible: ~25-35% of today's $172 with items 1-4 and 6, about half of that on an ordinary day without a runaway PR.

## 5. Claude Code behavior cited

- Cache read/write multipliers, 1h write 2x, Opus 5.5/Sonnet 5.5 read 0.05x: https://platform.claude.com/docs/en/build-with-claude/prompt-caching (pricing multipliers). Per-model prices: https://platform.claude.com/docs/en/about-claude/pricing (Opus 5.5 $4/$20, read $0.20).
- Whole conversation re-sent every request and re-read at cached rate; long sessions cost more: https://code.claude.com/docs/en/costs ("Why usage climbs in a long session").
- Subagents get their own context, system prompt and fresh cache, 5-minute TTL; the parent's prefix is unaffected: https://code.claude.com/docs/en/prompt-caching ("Subagents and the cache"), https://code.claude.com/docs/en/sub-agents ("What a subagent starts with"). Subagent `model` and `effort` fields and per-invocation model precedence: sub-agents page.
- Compaction keeps background commands and subagents running; `/compact` itself is a request over the whole context: https://code.claude.com/docs/en/context-window, https://code.claude.com/docs/en/prompt-caching ("Compacting the conversation").
- Bash output over ~30,000 chars becomes a file path plus a 2,000-char preview: https://code.claude.com/docs/en/tools-reference ("Output limits"); setting `bashOutputMaxChars` exists.
- Skill listings load every turn; `skillOverrides` `off`/`name-only`; `disable-model-invocation`: https://code.claude.com/docs/en/skills.
- `--disallowedTools`, `--settings`, `--bare`, `--setting-sources`, `--disable-slash-commands`: https://code.claude.com/docs/en/cli-reference.
- Auto-compact window setting, 1M-native models compact at ~967k by default: https://code.claude.com/docs/en/model-config.
- Monitor tool: https://code.claude.com/docs/en/tools-reference.

## Open questions / unconfirmed

- B1's real baseline drop: not run live (the user-level Stop/SessionStart hooks would fire in a test session). Estimate uses first-call shape sizes and a 2.85 chars/token calibration. Verify with one `claude -p --output-format json` from a temp dir.
- Plan-usage weighting: on a subscription the $ proxy may misrepresent cache reads vs output. Relative rankings by token volume still hold.
- Whether `loopherd restart` (`/clear`) kills background subagents: inferred from the 4 cut-short reviewer runs and the user's report; docs only say compaction preserves them.
- Effect size of `effort: low` on reviewer thinking, and Sonnet's catch rate on adversarial review: needs an A/B; compare `loopherd stats` rounds and a hand count of late-round findings.
- Whether the 300k lead limit is net positive: needs a day of data; compare cost per call and handoff count with the table above.
- Whether Monitor delivers events while the lead is mid-turn and how `tool_hook.rs` treats it.
- Reviewers sometimes cut short for non-handoff reasons (e.g. #113 first run, 2 calls): not investigated.
- Date window: "today" in UTC; earlier days had a mix of limits (the lead at 250k-366k on Nov 2-3) so the "history" lead numbers are not comparable run-to-run.
