# herdr — what survives a server restart

Tested 2026-10-31 against herdr 0.9.5 (client and server, protocol 22) in
the `loopherd-dev` session via `scripts/herdr-dev`. Server headless, no TUI
client attached. Below, `$S` is a scratch directory; `H` is
`scripts/herdr-dev herdr`.

## Answer

| Handle | Survives restart? | Notes |
|---|---|---|
| Workspace ID | Yes | Persisted. But IDs of closed workspaces above the highest surviving one are **reused** after restart. |
| Tab ID | Yes | Persisted per workspace with its `next` counter; not reused. |
| Pane ID | Yes | Persisted per workspace with its `next` counter; not reused, unless the whole workspace ID is reused (then `wN:p1` comes back). |
| Live agent name | Only if herdr resumes the agent | Restored for `agent start`-managed agents whose native resume succeeds; dropped otherwise. |
| `terminal_id` | No | Fresh every restart. |

Pane processes never survive. Every pane gets a fresh shell in its saved
cwd. herdr then types a resume command into panes whose agent reported one
(`resume_agents_on_restore`, default on).

## Experiment

### Before restart

```sh
H workspace create --label lh10-probe --cwd $S/a --no-focus   # w2, w2:t1, w2:p1
H pane split w2:p1 --direction right --cwd $S/b --no-focus    # w2:p2
H tab create --workspace w2 --cwd $S/b --label probe-tab --no-focus  # w2:t2, w2:p3
H pane rename w2:p2 probe-pane
H pane send-text w2:p1 'export LH10_MARK=before; echo marker-before'; H pane send-keys w2:p1 Enter
H agent start lh10-claude --kind claude --pane w2:p3 -- --model haiku
H pane report-agent w2:p2 --source lh10:probe --agent pi --state idle -- echo fake-resumed
H agent rename w2:p2 lh10-fake
```

```jsonc
// tab list --workspace w2
{"tab_id":"w2:t1","label":"1","pane_count":2}
{"tab_id":"w2:t2","label":"probe-tab","pane_count":1}
// pane list --workspace w2
{"pane_id":"w2:p1","tab_id":"w2:t1","label":null,"cwd":"$S/a","terminal_id":"term_65f2c86b07bcb2"}
{"pane_id":"w2:p2","tab_id":"w2:t1","label":"probe-pane","cwd":"$S/b","terminal_id":"term_65f2c887d45373"}
{"pane_id":"w2:p3","tab_id":"w2:t2","label":null,"cwd":"$S/b","terminal_id":"term_65f2c887d82904"}
// agent list
{"name":"lh10-fake","pane_id":"w2:p2","agent":"pi","agent_session":null}
{"name":"lh10-claude","pane_id":"w2:p3","agent":"claude","agent_session":"b4745a20-…"}
// pane process-info: shell_pid w2:p1=406440, w2:p2=407078, w2:p3=407123
```

`session.json` (w2 only, trimmed). Internal pane numbers map to public
ones through `public_pane_numbers`; the `next_*` counters are persisted:

```json
{"id":"w2","custom_name":"lh10-probe","identity_cwd":"$S/a",
 "public_pane_numbers":{"2":1,"3":2,"4":3},"next_public_pane_number":4,
 "public_tab_numbers":[1,2],"next_public_tab_number":3,
 "tabs":[
  {"custom_name":null,
   "layout":{"Split":{"direction":"Horizontal","ratio":0.5,"first":{"Pane":2},"second":{"Pane":3}}},
   "panes":{"2":{"cwd":"$S/a"},
            "3":{"cwd":"$S/b","label":"probe-pane","agent_name":"lh10-fake",
                 "agent_resume":{"source":"lh10:probe","agent":"pi","argv":["echo","fake-resumed"]}}}},
  {"custom_name":"probe-tab","layout":{"Pane":4},
   "panes":{"4":{"cwd":"$S/b","agent_name":"lh10-claude","managed_agent_kind":"claude",
                 "agent_session":{"source":"herdr:claude","agent":"claude","kind":"id","value":"b4745a20-…"}}}}]}
```

### Restart 1 (claude had no conversation yet)

```sh
H status            # socket: ~/.config/herdr/sessions/loopherd-dev/herdr.sock
H server stop       # all three shell PIDs and the claude process gone
scripts/herdr-dev --start
```

- `workspace list`, `tab list`, `pane list`: same `w2`, `w2:t1`, `w2:t2`,
  `w2:p1..p3`, same labels (`lh10-probe`, `probe-tab`, `probe-pane`), same
  cwds, same split. New `terminal_id`s and shell PIDs (409128, 409757,
  409803).
- `w2:p1`: `echo $LH10_MARK` → `unset`. Fresh shell. (bash history is
  still there because bash writes `HISTFILE`, not because of herdr.)
- `w2:p2`: herdr typed `echo fake-resumed` (the reported resume argv) at
  the prompt. Name `lh10-fake` **not** restored.
- `w2:p3`: herdr typed `claude --resume b4745a20-…`; Claude answered
  `No conversation found` (no turn had been taken) and exited to the
  shell. herdr then cleared the name and `agent_session` from the pane.
- `agent list` → empty.

### Restart 2 (claude with one completed turn)

```sh
H agent start lh10-claude --kind claude --pane w2:p3 -- --model haiku
H agent prompt lh10-claude "Reply with only the word OK. Do not use tools." --wait
H pane split w2:p1 --direction down --no-focus   # w2:p4
H pane close w2:p4
H server stop; scripts/herdr-dev --start
```

```jsonc
// agent list (~4 s after start, still no client attached)
{"name":"lh10-claude","pane_id":"w2:p3","agent":"claude","agent_status":"idle",
 "agent_session":"812eaa6c-…"}
// ps on the pane's foreground pid
claude --resume 812eaa6c-dba6-40b5-bbea-a00243ff82d9
```

The pane showed the prior `OK` exchange. Name, pane ID, and
`agent_session` all came back. The original `--model haiku` argument was
not replayed. herdr's built-in resume argv is only `claude --resume <id>`.

New IDs afterwards: `pane split w2:p1` → `w2:p5` (closed `p4` not reused),
`tab create` → `w2:t3`.

### Restart 3 (workspace ID reuse)

```sh
H workspace create --label lh10-tmp-a ...   # w3 / w3:p1
H workspace close w3
H workspace create --label lh10-tmp-b ...   # w4 / w4:p1 (no reuse within a run)
H workspace close w4
H server stop; scripts/herdr-dev --start
H workspace create --label lh10-tmp-c ...   # w3 / w3:p1  <- reused
H pane split w2:p1 ...                      # w2:p7 (no reuse in a surviving workspace)
```

`lh10-claude` resumed again on `w2:p3` with the same session.

## Why (source)

- **Workspace IDs** come from a process-global counter. On restore it is
  bumped to `max(surviving workspace number) + 1`, so the IDs of closed
  workspaces above that max are handed out again. IDs use the base-32
  alphabet `123456789ABCDEFGHJKMNPQRSTVWXYZ0` (`w9` is followed by `wA`).
  Don't parse them as decimal.
  [`src/workspace.rs` L104–170](https://github.com/herdrdev/herdr/blob/v0.9.5/src/workspace.rs#L104-L170),
  [`src/persist/restore.rs` L311](https://github.com/herdrdev/herdr/blob/v0.9.5/src/persist/restore.rs#L311).
- **Tab/pane IDs** are `{workspace}:t{n}` / `{workspace}:p{n}` with
  per-workspace `next_public_*_number` saved in `session.json`.
  [`src/workspace.rs` L143–150](https://github.com/herdrdev/herdr/blob/v0.9.5/src/workspace.rs#L143-L150).
  The skill says closed tab and pane IDs are never reused. It says nothing
  of the kind about workspace IDs (`herdr --skill`, "Use IDs and caller
  context").
- **Persisted per pane**: `cwd`, `label`, `agent_name`,
  `managed_agent_kind`, `agent_session`, `agent_resume`, `launch_argv`.
  [`src/persist/snapshot.rs` L100](https://github.com/herdrdev/herdr/blob/v0.9.5/src/persist/snapshot.rs#L100).
  Per tab: `custom_name`, layout tree with ratios, zoom, focus.
- **Name restore**: on cold restore with a pending resume plan, the name is
  restored only when `managed_agent_kind` is set (i.e. the agent came from
  `agent start`). A name on an unmanaged/reported agent is dropped.
  [`src/persist/restore.rs` L536–610](https://github.com/herdrdev/herdr/blob/v0.9.5/src/persist/restore.rs#L536-L610).
  The pane env (`HERDR_PANE_ID` etc.) is rebuilt with the same public IDs.
- **Resume** argv per agent kind (`claude --resume <id>`, `codex resume <id>`, …):
  [`src/agent_resume.rs` L199](https://github.com/herdrdev/herdr/blob/v0.9.5/src/agent_resume.rs#L199).
  Resume waits for a client's terminal theme, then launches anyway after a
  timeout. That explains why it worked headless.
  [`src/app/agent_resume.rs` L226](https://github.com/herdrdev/herdr/blob/v0.9.5/src/app/agent_resume.rs#L226).
- **Docs** ("Session state and restore"): restart restores "workspaces,
  tabs, panes, cwd, layout, and focus". Processes don't survive. Panes come
  back as new shells unless native agent session restore applies. That
  needs an official integration (Claude ≥ v6; ours is v11) or an
  agent-reported resume command. Missing, invalid, duplicated, or stale
  refs restore as plain shells. The docs say resume happens "after a client
  attaches". We saw it happen with no client (see the theme timeout above).
- `session-snapshots/` holds up to 48 layout snapshots, at most one per
  15 min, plus the first. They are never auto-restored. Pane screen history
  is off by default (`[experimental] pane_history`).

## Corrections to herdr.md

- "Closed IDs are never reused" holds for **tab and pane** IDs only.
  Workspace IDs can come back after a restart, and the pane IDs inside them
  (`w3:p1`) come back with them.
- "After server stop panes return as shells; agents not restarted" is
  **wrong** in 0.9.5 with integrations installed. Agents with an official
  integration session ref, or a self-reported resume argv, are resumed in
  their pane by default. Others return as shells.

## Recommendation for revive

1. **Primary key: `agent_session` (Claude session ID).** It is the only
   handle that identifies the conversation itself. It survives the restart
   unchanged. After the restart, find the pane with `agent list` by
   matching `agent_session.value`.
2. **Then the loopherd name.** Agents spawned via `agent start` (as
   `loopherd spawn` does) get their name back when herdr's resume succeeds.
   A resolved name plus a matching `agent_session` confirms the pane.
3. **If no live agent matches** (resume disabled, failed, or stale ref):
   locate the pane by workspace **label** + pane cwd (+ tab label). If it is
   at a shell prompt, run `agent start <name> --kind claude --pane <id> --
   --resume <session> <original args>` ourselves. Re-pass args such as
   `--model`, because herdr's resume drops them.
4. **Never trust a recorded pane/workspace ID alone.** It is a fast-path
   hint. Use it only after checking that the pane's workspace label and cwd
   still match, because `wN` (and `wN:p1`) can be reused after a restart.
5. **Avoid double-resume.** Before starting anything, check whether herdr
   already resumed the session. Resume runs a few seconds after server
   start, so wait for it, e.g. `agent list` until there are no
   `launch_pending` panes, or a short delay. Otherwise two Claudes could
   end up on one session.

## Not verified

- Codex/other kinds, `pane_history = true`, `--handoff`, and an attached TUI
  client during restore.
- Duplicate-session dedupe (two panes on the same session): docs and source
  say only one resumes; not exercised.

## Sources

- https://herdr.dev/llms.txt (doc index, 0.9.5)
- [Session state and restore](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/session-state.mdx)
- [Add Herdr support to your agent](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/add-herdr-support.mdx)
- [Socket API](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/socket-api.mdx) (`resume_argv`)
- [CLI reference](https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/cli-reference.mdx)
- Source at tag v0.9.5: `src/workspace.rs`, `src/persist/restore.rs`,
  `src/persist/snapshot.rs`, `src/agent_resume.rs`,
  `src/app/agent_resume.rs` (https://github.com/herdrdev/herdr, AGPL-3.0)
- `herdr --skill`, `herdr --default-config` (`[session]
  resume_agents_on_restore`), `herdr pane report-agent --help` (0.9.5)
- `~/.config/herdr/sessions/loopherd-dev/{session.json,herdr-server.log}`
