# herdr agent state events — research notes

Researched 2026-10-31 against herdr 0.9.5 (protocol 22, schema_version 1).
Answers wayfinding Q4 (card LH-2).

## Answer

**Yes, herdr pushes events.** Its socket API has an `events.subscribe`
method that streams agent state changes. loopherd does not need to poll for
state. It still needs one `session.snapshot` read to bootstrap and to recover.

`herdr api schema` lists `events.subscribe` and `events.wait` among the request
methods, and ships `event` and `subscription_event` envelope schemas
([`herdr api schema --json`](#sources)). The Socket API page that
[llms.txt](https://herdr.dev/llms.txt) links to recommends the raw socket for
"long-lived event subscriptions". The CLI does not wrap `events.subscribe`. Its
`herdr api` subcommands are only `snapshot` and `schema`. `herdr --skill` names
only the one-shot waits (`agent wait`, `agent prompt --wait`,
`pane wait-output`), so loopherd must talk to the socket directly to subscribe.

## Transport

Newline-delimited JSON over a Unix domain socket
([Socket API, "Socket transport"](#sources)):

- Default session: `~/.config/herdr/herdr.sock`.
- Named session: `~/.config/herdr/sessions/<name>/herdr.sock`. For example,
  `herdr --session loopherd-dev status` prints the dev socket path.
- Send one request per line: `{"id": …, "method": …, "params": …}`.
  Responses echo the `id`.
- A subscription keeps its connection open. The first line acknowledges the
  subscription, and every later line is a pushed event.

## Request shape

```json
{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[
  {"type":"pane.agent_status_changed","pane_id":"w1:p1"},
  {"type":"pane.agent_detected"},
  {"type":"pane.created"},
  {"type":"pane.closed"}
]}}
```

The schema defines `params.subscriptions` as an array of `Subscription`.
Agent-relevant subscription types:

| `type` | Scope | Notes |
|---|---|---|
| `pane.agent_status_changed` | **one pane**: `pane_id` required; `agent_status` optional filter | `idle`/`working`/`blocked`/`done`/`unknown` |
| `pane.agent_detected` | all panes | Fires when an agent appears in a pane |
| `pane.created`, `pane.closed`, `pane.exited`, `pane.moved` | all panes | Pane lifecycle; use to (un)subscribe per-pane status |
| `pane.updated` | all panes | Pane record changes (e.g. title); did **not** fire on status changes in the probe below |
| `pane.output_matched` | one pane | Needs `pane_id`, `source`, `match` |

Behaviors observed against `herdr --session loopherd-dev`:

- **No global agent-status subscription.** If `pane_id` is omitted, the request
  fails with
  `{"error":{"code":"invalid_request","message":"invalid request: missing field `pane_id` …"}}`.
- **One unknown pane rejects the whole request.** An unknown pane returns
  `{"error":{"code":"pane_not_found","message":"pane w9:p9 not found"}}`
  and the connection closes. The docs say the same.
- **Each connection's subscription set is fixed.** A second `events.subscribe`
  on a connection that is already subscribed got the connection reset by the
  server. To watch a new pane, open another connection, or reopen one with the
  full list.

## Message format

Every pushed line is `{"event": <kind>, "data": {…}}`. The schema calls this
`SubscriptionEventEnvelope` for agent status and output, and `EventEnvelope`
for lifecycle events. Kind names are inconsistent: agent status uses a dot
(`pane.agent_status_changed`), while lifecycle events use underscores
(`pane_agent_detected`, `pane_created`). Lifecycle `data` also carries a
`type` field. A parser should match on both spellings.

`pane.agent_status_changed` data, per the schema: `pane_id`, `workspace_id`,
and `agent_status` are required. `agent`, `display_agent`, `title`, and
`state_labels` are optional. Events carry no `revision` or sequence number.

### Captured example

This was captured on the `loopherd-dev` session with `socat`, while
`pane.report_agent` drove pane `w1:p1` through `working → blocked → idle`.
That is the same path an agent integration hook uses.

```console
$ SOCK=~/.config/herdr/sessions/loopherd-dev/herdr.sock
$ echo '{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.agent_status_changed","pane_id":"w1:p1"},{"type":"pane.agent_detected"},{"type":"pane.updated"}]}}' \
    | socat - UNIX-CONNECT:$SOCK
```

```json
{"id":"sub_1","result":{"type":"subscription_started"}}
{"data":{"agent":"claude","agent_status":"working","pane_id":"w1:p1","workspace_id":"w1"},"event":"pane.agent_status_changed"}
{"data":{"agent":"claude","pane_id":"w1:p1","type":"pane_agent_detected","workspace_id":"w1"},"event":"pane_agent_detected"}
{"data":{"agent":"claude","agent_status":"blocked","pane_id":"w1:p1","workspace_id":"w1"},"event":"pane.agent_status_changed"}
{"data":{"agent":"claude","agent_status":"idle","pane_id":"w1:p1","workspace_id":"w1"},"event":"pane.agent_status_changed"}
```

Pane lifecycle events from a second run (split, then close):

```json
{"id":"sub_a","result":{"type":"subscription_started"}}
{"data":{"pane":{"agent_status":"unknown","cwd":"/tmp","focused":false,"foreground_cwd":"/tmp","pane_id":"w1:p3","revision":0,"scroll":{"max_offset_from_bottom":0,"offset_from_bottom":0,"viewport_rows":38},"tab_id":"w1:t1","terminal_id":"term_65f28294790933","workspace_id":"w1"},"type":"pane_created"},"event":"pane_created"}
{"data":{"pane_id":"w1:p3","type":"pane_closed","workspace_id":"w1"},"event":"pane_closed"}
```

### `pane.moved`

Probed 2026-11-18 on the `loopherd-dev` session (herdr 0.9.5) with a raw
`events.subscribe` to `pane.moved`, `tab.closed`, `workspace.closed` and the
pane close events, while `herdr pane move` moved a shell pane. No plugin was
linked (D62).

- `data.pane` is the pane record after the move, with `pane_id`, `tab_id`
  and `workspace_id`. `previous_pane_id`, `previous_tab_id` and
  `previous_workspace_id` are where it was.
- A move within a workspace (`--new-tab`, `--tab <id> --split …`) keeps
  the pane id. A move to another workspace (`--new-workspace`,
  `--new-tab --workspace <id>`) gives it a new one: `w2:p2` became `w3:p1`.
  The `terminal_id` stays.
- `created_tab` and `created_workspace` are present when the move made
  them. `closed_tab_id` and `closed_workspace_id` are present when the
  move emptied them, and herdr then emits `tab.closed` and
  `workspace.closed` for those right after `pane.moved`.
- No `pane.closed` or `pane.exited` fires for the old pane id.
- `--tab` needs `--split right|down`; `--workspace` alone is not a move.

Moved to a new tab in the same workspace (`scroll` cut):

```json
{"data":{"created_tab":{"agent_status":"unknown","focused":true,"label":"2","number":2,"pane_count":1,"tab_id":"w2:t2","workspace_id":"w2"},"pane":{"agent_status":"unknown","cwd":"/tmp","focused":true,"foreground_cwd":"/tmp","pane_id":"w2:p2","revision":1,"tab_id":"w2:t2","terminal_id":"term_66091452e852712","terminal_title":"…","terminal_title_stripped":"…","workspace_id":"w2"},"previous_pane_id":"w2:p2","previous_tab_id":"w2:t1","previous_workspace_id":"w2","type":"pane_moved"},"event":"pane_moved"}
```

Moved from workspace `w3`, where it was alone, to a new tab of `w2`:

```json
{"data":{"closed_tab_id":"w3:t1","closed_workspace_id":"w3","created_tab":{"agent_status":"unknown","focused":true,"label":"2","number":3,"pane_count":1,"tab_id":"w2:t3","workspace_id":"w2"},"pane":{"agent_status":"unknown","cwd":"/tmp","focused":true,"foreground_cwd":"/tmp","pane_id":"w2:p3","revision":1,"tab_id":"w2:t3","terminal_id":"term_66091452e852712","terminal_title":"…","terminal_title_stripped":"…","workspace_id":"w2"},"previous_pane_id":"w3:p1","previous_tab_id":"w3:t1","previous_workspace_id":"w3","type":"pane_moved"},"event":"pane_moved"}
{"data":{"tab_id":"w3:t1","type":"tab_closed","workspace_id":"w3"},"event":"tab_closed"}
{"data":{"type":"workspace_closed","workspace_id":"w3"},"event":"workspace_closed"}
```

## Delivery guarantees

These come from the [Socket API, "Event subscriptions"](#sources) section:

- No replay: events start when the subscription is accepted.
- Order is guaranteed only within one subscription entry. It is not guaranteed
  across entries, or between events and a snapshot.
- History is bounded and not durable. A subscriber that falls behind gets an
  error with `error.code: "events_lost"`, and its connection closes.
- The documented recovery: resubscribe, wait for `subscription_started`, then
  call `session.snapshot` on a **second** connection. Replace the cache with
  the snapshot, and treat later events as invalidation signals that trigger
  another read.

## What this means for loopherd

The herd view (slice 2) and the queue (slice 4) can be event-driven:

1. Open a lifecycle connection (`pane.created`, `pane.closed`,
   `pane.agent_detected`) and wait for `subscription_started`.
2. Bootstrap from `session.snapshot` (`herdr api snapshot`) on another
   connection.
3. Open a status connection with one `pane.agent_status_changed` entry per
   member pane. When the member set changes, replace that connection. A pane
   that closed in the meantime fails the whole request, so refresh the pane
   list and retry.
4. On `events_lost` or a disconnect, go back to step 1.

Events carry no sequence numbers, so `revision` and `state_change_seq` on
`agent list` records (from the `AgentInfo` schema) are what make the reconcile
read cheap. If a member's `state_change_seq` hasn't moved, skip it. After the
probe above, `w1:p1` showed `revision: 1, state_change_seq: 3,
completion_seq: 3`, one `state_change_seq` step per reported transition.

Reconcile cost, measured on the live default session (17 agents, read-only
commands, 20 runs each, wall time including process spawn):

| Command | Median | Min–max | Output |
|---|---|---|---|
| `herdr agent list` | 3.7 ms | 3.3–4.9 ms | 9.2 KB |
| `herdr api snapshot` | 6.2 ms | 5.7–6.7 ms | 27.5 KB |

These numbers also make polling a workable fallback. Calling `agent list` once
a second costs under 0.5% of one core. Subscriptions are still the better
default: `blocked` shows up at once, and there is no diffing loop.

## Sources

- `herdr api schema` and `herdr api schema --json` (herdr 0.9.5, protocol 22):
  the `request` methods `events.subscribe`/`events.wait`, the `Subscription`,
  `EventKind`, `SubscriptionEventEnvelope`, and `AgentInfo` definitions.
- https://herdr.dev/llms.txt, which links to the Socket API page:
  https://raw.githubusercontent.com/herdrdev/herdr/v0.9.5/docs/next/website/src/content/docs/socket-api.mdx
  (sections "Socket transport", "Socket paths", "Event subscriptions", and the
  `session.snapshot` notes).
- `herdr --skill`, which documents only one-shot waits (`agent wait`,
  `agent prompt --wait`, `pane wait-output`) and no CLI subscription command.
- The probes above, run against `herdr --session loopherd-dev`.
