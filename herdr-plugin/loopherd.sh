#!/bin/sh
# Runs the installed loopherd; the plugin never installs it (docs/decisions/D62.md).
# herdr's server may not have ~/.local/bin, loopherd's install dir, on its PATH.
set -eu

if bin=$(command -v loopherd); then
  exec "$bin" "$@"
fi
if [ -x "$HOME/.local/bin/loopherd" ]; then
  exec "$HOME/.local/bin/loopherd" "$@"
fi
echo "loopherd plugin: loopherd not found on PATH or in ~/.local/bin." \
  "Install it first: https://github.com/junderdo/loopherd#install" >&2
exit 127
