# loopherd

A Rust TUI that groups coding agents into herds and keeps them working
together across sessions, built as a layer over herdr's CLI/socket API.

- **Issues**: claiming, working, or creating a card on the Trello board →
  read `docs/agents/issue-tracking.md` first.
- **Direction**: decisions, the herd model, and open questions →
  `docs/wayfinding.md`. herdr's capabilities → `docs/research/herdr.md`.
- **herdr in tests** goes through the fake behind the herdr trait, and manual
  runs use `herdr --session loopherd-dev`, keeping the human's live session
  untouched.
- **Checks** before calling work done: `cargo fmt --check`,
  `cargo clippy -- -D warnings`, `cargo test`.
