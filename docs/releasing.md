# Releasing

loopherd follows SemVer from `v0.1.0`. Below 1.0:

- **Minor** (0.1 → 0.2) for a change that breaks an existing setup: a herd
  file, `config.toml`, ledger or mailbox format that needs migrating; a
  renamed or removed command or flag; a hook protocol change that needs
  `loopherd install-hook` run again.
- **Patch** for everything else.

1.0 comes once the one-line install is live, `loopherd init` and
`loopherd setup` are done, and the herd file and `config.toml` formats are
supported with migrations.

## Cutting a release

1. Open a PR that bumps `version` in `Cargo.toml` (and `Cargo.lock` with
   it: `cargo build` updates it). Merge it.
2. Tag the merge commit and push the tag:

   ```bash
   git fetch origin
   git tag v0.2.0 <merge commit>
   git push origin v0.2.0
   ```

3. `.github/workflows/release.yml` checks the tag matches `Cargo.toml`, runs
   the gate (`cargo fmt --check`, `cargo clippy`, `cargo test`, `shellcheck
   scripts/install.sh`), builds `loopherd` for x86_64 and aarch64 Linux
   (static musl) and macOS, and publishes a GitHub Release with:
   - `loopherd-<target>.tar.gz` per target,
   - `SHA256SUMS`,
   - `install.sh`, stamped with the tag so it installs that release.

Release notes are GitHub's generated notes, built from the PR titles; there
is no `CHANGELOG.md`. A tag that fails the workflow publishes nothing; fix
on main, delete the tag, and tag again.

## Versions in builds

`loopherd --version` prints the bare version for a clean build of its
`v<version>` tag, and the version plus the short commit for any other build
from git (`loopherd 0.1.0 (ea71305)`, `-dirty` with uncommitted changes).
A build with no git prints the bare version.
