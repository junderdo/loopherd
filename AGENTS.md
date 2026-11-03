# loopherd

A Rust TUI that groups coding agents into herds and keeps them working
together across sessions, built as a layer over herdr's CLI/socket API.

- **Issues**: claiming, working, or creating a card on the Trello board →
  read `docs/agents/issue-tracking.md` first.
- **Shepherding**: running the herd live with the human (feeding, watching,
  verifying and cleaning up members) → read `loopherd --skill` first,
  then `docs/agents/shepherd.md`, this repo's runbook.
- **Direction**: decisions, the herd model, and open questions →
  `docs/wayfinding.md`, which indexes one `docs/decisions/D<n>.md` per
  decision. herdr's capabilities → `docs/research/herdr.md`.
- **herdr in tests** goes through the fake behind the herdr trait, and manual
  runs use `herdr --session loopherd-dev`, keeping the human's live session
  untouched. Wrap manual commands in `scripts/herdr-dev` (`--start` boots the
  dev server); live tests run with
  `LOOPHERD_LIVE=1 scripts/herdr-dev cargo test --test live`.
- **Installing** the `loopherd` on PATH and its Claude Stop hook is
  `scripts/install`, run by the shepherd, or by a herd's lead whose `[lead]`
  sets `install = true` (D17). Other members never run it; tests and manual
  runs of `install-hook` pass `--settings <temp file>`.
- **Checks** before calling work done: `cargo fmt --check`,
  `cargo clippy -- -D warnings`, `cargo test`.
