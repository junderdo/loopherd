#!/bin/sh
# Runs the installed loopherd; the plugin never installs it (docs/decisions/D62.md).
# herdr's server may not have ~/.local/bin, loopherd's install dir, on its PATH.
set -eu

if ! bin=$(command -v loopherd); then
  bin="${HOME:-}/.local/bin/loopherd"
fi
if [ ! -x "$bin" ]; then
  echo "loopherd plugin: loopherd not found on PATH or in ~/.local/bin." \
    "Install it first: https://github.com/junderdo/loopherd#install" >&2
  exit 127
fi

# loopherd calls `herdr`; make that the herdr that ran this command.
if [ -n "${HERDR_BIN_PATH:-}" ]; then
  PATH="${HERDR_BIN_PATH%/*}:$PATH"
  export PATH
fi
exec "$bin" "$@"
