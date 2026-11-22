# Sourced by scripts/install: offers loopherd's herdr plugin, which herdr
# registers without touching the human's herdr config (docs/decisions/D126.md).

herdr_plugin_source=junderdo/loopherd/herdr-plugin

# Usage: offer_herdr_plugin <ask|yes|no> <the checkout's herdr-plugin.toml> <--settings file, or empty>
# Never fails: the plugin is optional, so a failed install is only reported.
offer_herdr_plugin() {
  local want=$1 manifest=$2 settings=$3 ours listed version kind root answer
  [[ "$want" != no ]] || return 0
  if [[ -n "$settings" ]]; then
    # herdr plugins are global to the user, so a test or manual run must not install one.
    echo "skipped loopherd's herdr plugin: a --settings run never installs it"
    return 0
  fi
  if [[ "$want" == ask && ! -t 0 ]]; then
    echo "skipped loopherd's herdr plugin (no terminal); add --with-herdr-plugin to install it"
    return 0
  fi
  ours=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$manifest" 2>/dev/null | head -n 1) || ours=
  if [[ -z "$ours" ]]; then
    echo "skipped loopherd's herdr plugin: no version in $manifest"
    return 0
  fi
  if ! command -v herdr >/dev/null; then
    echo "skipped loopherd's herdr plugin: no herdr on PATH"
    return 0
  fi
  if ! listed=$(list_herdr_plugin); then
    echo "skipped loopherd's herdr plugin: herdr couldn't list its plugins (herdr plugin list --plugin loopherd)"
    return 0
  fi
  # Not tab: read collapses runs of whitespace separators, shifting an empty field.
  IFS=$'\x1f' read -r version kind root <<<"$listed"
  [[ -z "$listed" ]] || herdr_plugin_installed=1
  if [[ "$version" == "$ours" ]]; then
    echo "loopherd's herdr plugin $version is already installed; left as is"
    return 0
  fi
  if [[ "$kind" == local ]]; then
    echo "loopherd's herdr plugin is linked from $root; left as is"
    return 0
  fi
  if [[ -n "$version" && "$(printf '%s\n' "$version" "$ours" | sort -V | tail -n 1)" == "$version" ]]; then
    echo "loopherd's herdr plugin $version is newer than this checkout's $ours; left as is"
    return 0
  fi
  local verb=install question="Install loopherd's herdr plugin ($herdr_plugin_source)?"
  local run=(herdr plugin install "$herdr_plugin_source")
  if [[ -n "$version" ]]; then
    # herdr updates from the ref it installed, which may not be this checkout's version.
    verb=update question="Update loopherd's herdr plugin (installed $version, this checkout $ours)?"
    run=(herdr plugin update loopherd)
  fi
  if [[ "$want" == ask ]]; then
    read -r -p "$question [y/N] " answer || answer=
    [[ "$answer" == [yY]* ]] || { echo "skipped loopherd's herdr plugin"; return 0; }
  else
    # Asked nothing, so herdr mustn't stop to confirm its own preview either.
    run+=(--yes)
  fi
  if "${run[@]}"; then
    herdr_plugin_installed=1
  else
    echo "install: herdr plugin $verb failed; the rest of the install is unaffected" >&2
  fi
}

# Prints loopherd's version, source kind and root, joined by \x1f, or nothing
# when herdr lists no loopherd plugin; fails when herdr can't list.
list_herdr_plugin() {
  local wait=()
  # A wedged herdr server mustn't hang the install; macOS has no timeout(1).
  ! command -v timeout >/dev/null || wait=(timeout 10)
  ${wait[@]+"${wait[@]}"} herdr plugin list --plugin loopherd --json 2>/dev/null |
    jq -er 'first(.result.plugins[] | select(.plugin_id == "loopherd") |
      [.version // "", .source.kind // "", .plugin_root // ""] | join("\u001f")) // ""' 2>/dev/null
}
