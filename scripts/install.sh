#!/bin/sh
# curl -fsSL https://github.com/junderdo/loopherd/releases/latest/download/install.sh | sh
#
# Installs a released loopherd as ~/.local/bin/loopherd, its Claude hooks and
# skill, then runs `loopherd setup`. LOOPHERD_VERSION=vX.Y.Z pins a release;
# LOOPHERD_RELEASE_BASE replaces https://github.com/junderdo/loopherd/releases.
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

warn_missing() {
  for tool in herdr claude; do
    command -v "$tool" >/dev/null || say "warning: $tool isn't on PATH; loopherd needs it to run a herd"
  done
  case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) say "warning: $bin_dir isn't on PATH; add it to run loopherd by name" ;;
  esac
}

# Called on the last line, so `curl | sh` runs nothing from a cut-off download.
main() {
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
  say "downloading $tarball from $url"
  download "$url/$tarball" "$work/$tarball"
  download "$url/SHA256SUMS" "$work/SHA256SUMS"
  verify "$work" "$tarball"
  tar -xzf "$work/$tarball" -C "$work" loopherd || die "could not unpack loopherd from $tarball"

  mkdir -p "$bin_dir"
  tmp=$(mktemp "$bin_dir/.loopherd.XXXXXX")
  trap 'rm -rf "$work" "$tmp"' EXIT
  cp "$work/loopherd" "$tmp"
  chmod 755 "$tmp"
  # A rename, so a running loopherd or a hook mid-call never sees a half-copied file.
  mv -f "$tmp" "$bin_dir/loopherd"
  say "installed $bin_dir/loopherd"

  # </dev/null: under `curl | sh` stdin is this script. Setup reads /dev/tty.
  "$bin_dir/loopherd" install-hook </dev/null
  "$bin_dir/loopherd" setup </dev/null || say "setup failed; run loopherd setup once config.toml is fixed"

  warn_missing
  say "next: loopherd herds"
}

main
