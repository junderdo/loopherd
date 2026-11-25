# Sourced by scripts/install-extras after install-herdr-plugin.bash: offers herdr
# key bindings for the plugin's actions (docs/decisions/D129.md).

# Usage: offer_herdr_keys <ask|yes|no> <installed loopherd> <--settings file, or empty>
# Never fails: the bindings are optional, so a failure is only reported.
offer_herdr_keys() {
  local want=$1 loopherd=$2 settings=$3 answer listed
  [[ "$want" != no ]] || return 0
  if [[ -n "$settings" ]]; then
    echo "skipped herdr key bindings: a --settings run never writes herdr's config"
    return 0
  fi
  if [[ "$want" == ask && ! -t 0 ]]; then
    echo "skipped herdr key bindings (no terminal); ${LOOPHERD_EXTRAS_NO_TERMINAL:-add --with-herdr-keys to add them}"
    return 0
  fi
  if ! command -v herdr >/dev/null; then
    echo "skipped herdr key bindings: no herdr on PATH"
    return 0
  fi
  if [[ -z "${herdr_plugin_installed:-}" ]]; then
    if ! listed=$(list_herdr_plugin); then
      echo "skipped herdr key bindings: herdr couldn't list its plugins (herdr plugin list --plugin loopherd)"
      return 0
    fi
    if [[ -z "$listed" ]]; then
      echo "skipped herdr key bindings: loopherd's herdr plugin isn't installed"
      return 0
    fi
  fi
  if [[ "$want" == ask ]]; then
    read -r -p "Add herdr key bindings for loopherd's plugin (prefix+m: herd view, prefix+shift+m: view beside shepherd)? [y/N] " answer || answer=
    [[ "$answer" == [yY]* ]] || { echo "skipped herdr key bindings"; return 0; }
  fi
  "$loopherd" install-herdr-keys ||
    echo "install: herdr key bindings failed; the rest of the install is unaffected" >&2
}
