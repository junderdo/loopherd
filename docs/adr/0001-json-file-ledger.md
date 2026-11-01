# The ledger is a JSON file, not SQLite

The ledger maps each agent loopherd spawned to its herdr pane, Claude session
ID, repo, and worktree. It lives in one JSON file,
`$XDG_STATE_HOME/loopherd/ledger.json`, behind the `Ledger` trait. We chose a
file over SQLite because the expected concurrency is low and a plain file is
easy to read, edit, and repair by hand.

## Expected concurrency

A few loopherd processes on one machine, for one user (D6: no multi-machine in
v1). Writes happen when an agent is spawned, so they are rare and seldom
overlap.

## Consequences

- **Torn writes are prevented.** Each write goes to a per-process temp file,
  is synced, then renamed over the ledger. A reader sees the old file or the
  new one, never a partial one.
- **Lost updates are a known gap.** Every write reads the whole file, changes
  it, and writes it back. Two processes doing that at once means the later
  rename drops the earlier one's entry. Close the gap with an advisory `flock`
  held across read-modify-write once it shows up in practice.
- **Revisit SQLite** only if the queue slice (slice 5) needs many agents
  writing claims concurrently. The `Ledger` trait keeps that swap local.

## Considered options

- **SQLite.** Handles concurrent writers and queries. Rejected for now: it
  adds a dependency and makes the ledger opaque, for concurrency we don't have.
- **TOML.** Equally simple, but JSON is what serde writes without extra
  crates, and the ledger is machine-written state, not hand-written config.
