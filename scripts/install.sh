#!/bin/sh
# curl -fsSL https://github.com/junderdo/loopherd/releases/latest/download/install.sh | sh
#
# Takes no options. Installs a released loopherd as ~/.local/bin/loopherd, its
# Claude hooks and skill, then, like scripts/install, offers the agent types,
# the mattpocock-skills plugin, the herdr plugin and its key bindings, asking
# on /dev/tty, runs `loopherd setup` and, last, `loopherd doctor`.
# LOOPHERD_NO_SPLASH=1 skips the splash. LOOPHERD_UPDATE=1, set by `loopherd
# update`, also asks none of the offers: it refreshes only the agent types
# loopherd installed and names the skills plugin's update, as `scripts/install
# --update` does. LOOPHERD_VERSION=vX.Y.Z pins a
# release; LOOPHERD_RELEASE_BASE replaces
# https://github.com/junderdo/loopherd/releases. Unless it does, the installed
# tag is recorded in <config dir>/install.toml for `loopherd update`.
set -eu

# A release's copy carries its tag, so it installs that release, not the latest.
release_tag=

supported="x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-apple-darwin aarch64-apple-darwin"

say() { printf 'install: %s\n' "$*"; }
die() {
  printf 'install: %s\n' "$*" >&2
  exit 1
}

target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Linux) os=unknown-linux-musl ;;
    Darwin) os=apple-darwin ;;
    *) die "no loopherd release for $os $arch; releases are built for: $supported" ;;
  esac
  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "no loopherd release for $(uname -s) $arch; releases are built for: $supported" ;;
  esac
  echo "$arch-$os"
}

need_tools() {
  if command -v curl >/dev/null; then
    fetch=curl
  elif command -v wget >/dev/null; then
    fetch=wget
  else
    die "needs curl or wget; install one first"
  fi
  command -v tar >/dev/null || die "needs tar; install it first"
  if command -v sha256sum >/dev/null; then
    sha256="sha256sum"
  elif command -v shasum >/dev/null; then
    sha256="shasum -a 256"
  else
    die "needs sha256sum or shasum to check the download; install one first"
  fi
}

download() {
  case "$fetch" in
    curl) curl -fsSL -o "$2" "$1" ;;
    wget) wget -q -O "$2" "$1" ;;
  esac || die "could not download $1"
}

verify() {
  expected=$(awk -v f="$2" '$2 == f || $2 == "*" f { print $1; exit }' "$1/SHA256SUMS")
  [ -n "$expected" ] || die "SHA256SUMS lists no $2"
  # shellcheck disable=SC2086 # $sha256 is a command and its flags.
  actual=$(cd "$1" && $sha256 "$2" | awk '{ print $1 }')
  [ "$expected" = "$actual" ] ||
    die "checksum mismatch for $2 (expected $expected, got $actual); installed nothing"
}

# Usage: installed_tag <tag or empty for the latest>. Prints the installed
# release's tag, empty when the binary doesn't say.
installed_tag() {
  tag=$1
  [ -n "$tag" ] || tag=$("$bin_dir/loopherd" --version </dev/null | awk '{ print $2 }')
  case "$tag" in
    "" | v*) ;;
    *) tag=v$tag ;;
  esac
  echo "$tag"
}

# Usage: record_install <tag>.
record_install() {
  [ -z "${LOOPHERD_RELEASE_BASE:-}" ] || return 0
  tag=$1
  [ -n "$tag" ] || return 0
  config=${LOOPHERD_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/loopherd}
  mkdir -p "$config"
  printf 'kind = "release"\ntag = "%s"\n' "$tag" >"$config/install.toml.tmp"
  mv -f "$config/install.toml.tmp" "$config/install.toml"
}

warn_missing() {
  for tool in herdr claude; do
    command -v "$tool" >/dev/null || say "warning: $tool isn't on PATH; loopherd needs it to run a herd"
  done
  case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) say "warning: $bin_dir isn't on PATH; add it to run loopherd by name" ;;
  esac
}

# Usage: run_extras splash|offers. Reads answers from the terminal: under
# `curl | sh` stdin is this script.
run_extras() {
  updating=
  [ "${LOOPHERD_UPDATE:-}" != 1 ] || updating=1
  case "$1" in
    splash) [ "${LOOPHERD_NO_SPLASH:-}" != 1 ] && [ -z "$updating" ] || return 0 ;;
  esac
  extras="$work/scripts/install-extras"
  if [ ! -f "$extras" ]; then
    [ "$1" = splash ] || say "this release ships no extras to offer; scripts/install from a checkout offers them"
    return 0
  fi
  if ! command -v bash >/dev/null; then
    [ "$1" = splash ] || say "skipped the extras: they need bash"
    return 0
  fi
  tty=/dev/null
  # Opening, not -r: without a controlling terminal /dev/tty exists but won't open.
  if (: </dev/tty) 2>/dev/null; then tty=/dev/tty; fi
  if [ "$1" = splash ]; then
    bash "$extras" splash <"$tty"
  elif [ -n "$updating" ]; then
    bash "$extras" offers update update no no "$bin_dir/loopherd" <"$tty" ||
      say "an extra failed to update (said above); the rest of the install is unaffected"
  else
    LOOPHERD_EXTRAS_NO_TERMINAL="run this installer again in a terminal to be asked" \
      LOOPHERD_EXTRAS_RELEASE=$tag bash "$extras" offers ask ask ask ask "$bin_dir/loopherd" <"$tty" ||
      say "an extra failed to install (said above); the rest of the install is unaffected"
  fi
}

# Called on the last line, so `curl | sh` runs nothing from a cut-off download.
main() {
  [ "$#" -eq 0 ] || die "takes no options; it asks about each extra as it installs"
  need_tools
  target=$(target)
  base=${LOOPHERD_RELEASE_BASE:-https://github.com/junderdo/loopherd/releases}
  version=${LOOPHERD_VERSION:-$release_tag}
  case "$version" in
    "") url="$base/latest/download" ;;
    v*) url="$base/download/$version" ;;
    *) url="$base/download/v$version" ;;
  esac
  tarball="loopherd-$target.tar.gz"
  bin_dir="$HOME/.local/bin"

  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  # dash runs no EXIT trap on a signal, so Ctrl-C at a question would leave $work.
  trap 'exit 130' INT
  trap 'exit 143' TERM
  say "downloading $tarball from $url"
  download "$url/$tarball" "$work/$tarball"
  download "$url/SHA256SUMS" "$work/SHA256SUMS"
  verify "$work" "$tarball"
  tar -xzf "$work/$tarball" -C "$work" || die "could not unpack $tarball"
  [ -f "$work/loopherd" ] || die "$tarball holds no loopherd"
  run_extras splash || exit

  mkdir -p "$bin_dir"
  tmp=$(mktemp "$bin_dir/.loopherd.XXXXXX")
  trap 'rm -rf "$work" "$tmp"' EXIT
  cp "$work/loopherd" "$tmp"
  chmod 755 "$tmp"
  # A rename, so a running loopherd or a hook mid-call never sees a half-copied file.
  mv -f "$tmp" "$bin_dir/loopherd"
  say "installed $bin_dir/loopherd"
  tag=$(installed_tag "$version")
  record_install "$tag"

  # </dev/null: under `curl | sh` stdin is this script. Setup reads /dev/tty.
  "$bin_dir/loopherd" install-hook </dev/null
  run_extras offers
  "$bin_dir/loopherd" setup </dev/null || say "setup failed; run loopherd setup once config.toml is fixed"

  warn_missing
  say "next: loopherd herds"
  # herdr may not be started yet, so a doctor failure is shown but doesn't fail the install.
  "$bin_dir/loopherd" doctor </dev/null || true
}

main "$@"
