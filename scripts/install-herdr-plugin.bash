# Sourced by scripts/install-extras: offers loopherd's herdr plugin, which herdr
# registers without touching the human's herdr config (docs/decisions/D126.md).

herdr_plugin_source=junderdo/loopherd/herdr-plugin
# Long enough for herdr to clone the plugin from GitHub.
herdr_plugin_timeout=300

# Usage: offer_herdr_plugin <ask|yes|no> <the checkout's herdr-plugin.toml> <--settings file, or empty>
# Never fails: the plugin is optional, so a failed install is only reported.
# LOOPHERD_EXTRAS_RELEASE names the release tag being installed, whose plugin it installs.
offer_herdr_plugin() {
  local want=$1 manifest=$2 settings=$3 ours listed version kind root answer
  local release=${LOOPHERD_EXTRAS_RELEASE:-} from="this checkout"
  [[ "$want" != no ]] || return 0
  [[ -z "$release" ]] || from="this release"
  herdr_plugin_reinstall="herdr plugin uninstall loopherd && herdr plugin install $herdr_plugin_source${release:+ --ref $release}"
  if [[ -n "$settings" ]]; then
    # herdr plugins are global to the user, so a test or manual run must not install one.
    echo "skipped loopherd's herdr plugin: a --settings run never installs it"
    return 0
  fi
  # Without a terminal, whatever stops the offer early, the skip says how to add it.
  herdr_plugin_no_terminal=
  [[ "$want" != ask || -t 0 ]] || herdr_plugin_no_terminal=1
  ours=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$manifest" 2>/dev/null | head -n 1) || ours=
  if [[ -z "$ours" ]]; then
    herdr_plugin_skip "no version in $manifest"
    return 0
  fi
  if ! command -v herdr >/dev/null; then
    herdr_plugin_skip "no herdr on PATH"
    return 0
  fi
  if ! listed=$(list_herdr_plugin); then
    herdr_plugin_skip "herdr couldn't list its plugins (herdr plugin list --plugin loopherd)"
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
    echo "loopherd's herdr plugin $version is newer than $from's $ours; left as is"
    return 0
  fi
  if [[ -n "$version" ]] && ! herdr_has_plugin_update; then
    local herdr_says
    herdr_says=$(herdr --version 2>/dev/null | head -n 1) || herdr_says=
    echo "loopherd's herdr plugin $version is older than $from's $ours, and ${herdr_says:-this herdr} has no plugin update (0.9.4 or later); reinstall it by hand: $herdr_plugin_reinstall"
    return 0
  fi
  if [[ -n "$herdr_plugin_no_terminal" ]]; then
    if [[ -n "$version" ]]; then
      echo "skipped loopherd's herdr plugin $version (no terminal); ${LOOPHERD_EXTRAS_NO_TERMINAL:-add --with-herdr-plugin to update it}"
    else
      herdr_plugin_skip
    fi
    return 0
  fi
  local verb=install question="Install loopherd's herdr plugin ($herdr_plugin_source)?"
  local run=(herdr plugin install "$herdr_plugin_source")
  if [[ -n "$version" ]]; then
    # herdr updates from the ref it installed, which may not be this checkout's version.
    verb=update question="Update loopherd's herdr plugin (installed $version, $from $ours)?"
    run=(herdr plugin update loopherd)
  elif [[ -n "$release" ]]; then
    local install_help
    install_help=$(herdr plugin install --help 2>/dev/null) || install_help=
    if [[ "$install_help" == *--ref* ]]; then
      run+=(--ref "$release")
    else
      local herdr_says
      herdr_says=$(herdr --version 2>/dev/null | head -n 1) || herdr_says=
      echo "${herdr_says:-this herdr} can't install a plugin at a ref, so loopherd's herdr plugin comes from GitHub's default branch, not $release"
    fi
  fi
  if [[ "$want" == ask ]]; then
    read -r -p "$question [y/N] " answer || answer=
    [[ "$answer" == [yY]* ]] || { echo "skipped loopherd's herdr plugin"; return 0; }
  else
    # Asked nothing, so herdr mustn't stop to confirm its own preview either.
    run+=(--yes)
  fi
  local timed=
  # Without a terminal a hung clone or wedged server mustn't hang the install;
  # on one, timeout(1) would swallow Ctrl-C.
  if [[ ! -t 0 ]] && command -v timeout >/dev/null; then
    timed=1
    run=(timeout "$herdr_plugin_timeout" "${run[@]}")
  fi
  local status=0
  "${run[@]}" || status=$?
  if [[ -n "$timed" ]] && ((status == 124)); then
    echo "install: herdr plugin $verb timed out after ${herdr_plugin_timeout}s; the rest of the install is unaffected" >&2
  elif ((status != 0)); then
    echo "install: herdr plugin $verb failed; the rest of the install is unaffected" >&2
  else
    herdr_plugin_installed=1
    [[ "$verb" != update ]] || say_updated_herdr_plugin "$version"
  fi
}

# Usage: say_updated_herdr_plugin <the version before the update>
say_updated_herdr_plugin() {
  local listed now
  listed=$(list_herdr_plugin) || return 0
  IFS=$'\x1f' read -r now _ <<<"$listed"
  if [[ "$now" == "$1" ]]; then
    echo "loopherd's herdr plugin is still $1 after the update; herdr updates from the ref it installed, so reinstall it by hand: $herdr_plugin_reinstall"
  elif [[ -n "$now" ]]; then
    echo "loopherd's herdr plugin is now $now"
  fi
}

# True when herdr is 0.9.4 or later, the first with `herdr plugin update`.
herdr_has_plugin_update() {
  local have
  have=$(herdr --version 2>/dev/null | sed -n 's/^herdr \([0-9][0-9.]*\).*/\1/p' | head -n 1)
  [[ -n "$have" && "$(printf '%s\n' 0.9.4 "$have" | sort -V | head -n 1)" == 0.9.4 ]]
}

# Usage: herdr_plugin_skip [<why>]
herdr_plugin_skip() {
  if [[ -n "$herdr_plugin_no_terminal" ]]; then
    echo "skipped loopherd's herdr plugin (no terminal); ${LOOPHERD_EXTRAS_NO_TERMINAL:-add --with-herdr-plugin to install it}"
  else
    echo "skipped loopherd's herdr plugin: $1"
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
