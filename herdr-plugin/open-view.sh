#!/bin/sh
# The view action: opens the view pane, a popup that closes when loopherd view exits.
set -eu
exec "${HERDR_BIN_PATH:-herdr}" plugin pane open --plugin loopherd --entrypoint view
