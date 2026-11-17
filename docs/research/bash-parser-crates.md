# Bash parser crates for the lead hook

Researched 2026-11-16 for LH-165. The question was whether a Rust bash parser
could replace the lead hook's hand lexer (D17), which must find every command
bash would run in a Bash tool call. Outcome: D48.

## Method

- Sources: crates.io API JSON, each crate's source, README and CHANGELOG in
  `~/.cargo/registry`, and the GitHub API for issues and commits.
- One recursive walker per crate collected the simple commands. It recursed
  into command and process substitutions, backticks, heredoc bodies, `${…}`
  operands and arithmetic.
- 97 inputs were checked against bash 5.2.21 with a stub
  `gh() { echo GH-RAN >&2; }` as ground truth. In 77 of them bash runs `gh`.

## Maintenance, cost, licence

| Crate | Latest | Licence | Deps (normal, transitive) | C toolchain | Release build | Notes |
|---|---|---|---|---|---|---|
| brush-parser | 0.4.0, 2026-05-03 | MIT | 67 (`insta`, `pest`, `peg`, `cached` among them) | no | 9.0 s | Active, 110 open issues. `main` has unreleased breaking changes. |
| shuck-parser + shuck-ast | 0.2.10, 2026-11-16 | MIT | 9 third-party | no | 8.6 s | One maintainer, started 2026-04, pre-1.0 |
| rable | 0.2.2, 2026-10-28 | MIT | 6 | no | 3.0 s | One maintainer. Tested against bash's own parser (Parable corpus, 1604/1604). |
| yash-syntax | 0.25.0, 2026-09-06 | GPL-3.0-or-later | 46 (via `yash-env`) | no | 9.2 s | POSIX, not bash |
| tree-sitter-bash | 0.25.1, 2025-12-02 | MIT | 20 | yes (`cc`) | 6.4 s | No default-branch commits 2026-08-16 to 2026-11-16 |
| conch-parser | 0.1.1, 2019-05-15 | MIT/Apache-2.0 | 1 | no | 1.8 s | Archived 2021, POSIX only |

## Coverage

F means found, E a loud error, M a silent miss, and ~ that the parser returns
raw text the caller must re-parse.

| Input | brush | yash | shuck | rable | tree-sitter |
|---|---|---|---|---|---|
| `function f { gh pr merge 1; }; f` | F | E | F | F | F |
| `coproc gh pr merge 1` | F | **M** | F | F | **M** |
| `((x=((1))<<2)); cat <<E…` hiding `gh` | **M** (fixed on unreleased `main`) | E | F | F | F |
| `echo $((a[$(gh pr merge 1)]))` | ~ | F | F | ~ | F |
| `` echo `echo \`gh pr merge 1\`` `` | F | F | **M** | **M** | **M** |
| `$((gh pr merge 1); echo)` | F | F | **M** | **M** | E |
| `time gh pr merge 1` | F | **M** | F | F | **M** |
| `echo a<NBSP># ; gh pr merge 1` | F | **M** | F | F | F |
| `g\<newline>h pr merge 1` | F | F | F | F | **M** |
| `cat <(gh pr merge 1)` | F | E | F | F | F |
| `[[ -n $(gh pr merge 1) ]]` | F | E | F | F | F |
| `x='a[$(gh pr merge 1)]'; echo $((x))` | M | M | M | M | M |
| `{gh,} pr merge 1` | M | M | M | M | E |

Notes:

- No static parser can see the `$((x))` row: bash evaluates the subscript in
  the variable's value only at run time.
- brush splits words on space and tab only (`src/tokenizer.rs`, `is_blank`).
  yash splits on `char::is_whitespace` (`src/parser/lex/core.rs`, marked
  `TODO locale`).
- brush keeps words, `$(…)` bodies, `${…}` operands and arithmetic as raw
  strings that must be re-parsed. Its `arithmetic::parse` errors on `$(…)`.
- shuck parses all of those recursively, and decodes `$'…'`. It recovers
  from errors, so a caller must require `ParseStatus::Clean`.
- rable leaves `${…}` operands and heredoc bodies as raw strings.
- tree-sitter never returns an error. A caller must check `has_error()`.
  Its words are raw text.

## Word expansion

None of the crates turns a word into its final argv:

- Brace expansion: brush, shuck and rable return its tree, and the caller
  still forms every combination.
- `$'…'` decoding: shuck and rable decode it. brush's `unquote_str` returns
  `$gx68` for `$'g\x68'`.
- Wrappers: only shuck normalises them (`normalize_command`, which covers
  `command`, `exec`, the `sudo` family and `find -exec`).
- `eval`, `bash -c`, sinks and cargo aliases have no support in any crate.

## Fit for a fail-closed hook

None of the crates is safe as the only gate, since each one silently missed
an input bash runs. The safest use would be as a second opinion beside the
lexer: deny on a parse error, an error status, or a command the lexer
didn't report. brush is the most faithful to bash but heavy to build.
shuck has the best API but is young. rable is small and tested against
bash's own parser.

## Sources

- https://crates.io/crates/brush-parser, https://github.com/reubeno/brush
- https://crates.io/crates/shuck-parser, https://github.com/ewhauser/shuck
- https://crates.io/crates/rable, https://github.com/mpecan/rable
- https://crates.io/crates/yash-syntax, https://github.com/magicant/yash-rs
- https://crates.io/crates/tree-sitter-bash, https://github.com/tree-sitter/tree-sitter-bash
- https://crates.io/crates/conch-parser, https://github.com/ipetkov/conch-parser
