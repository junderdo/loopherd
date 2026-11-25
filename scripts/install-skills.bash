# Sourced by scripts/install-extras: offers the mattpocock-skills plugin, leaving the
# human's own skills as they are (docs/decisions/D26.md).

skills_plugin=mattpocock-skills
skills_marketplace=claude-plugins-official
skills_marketplace_repo=anthropics/claude-plugins-official
skills_id="$skills_plugin@$skills_marketplace"

# Runs a read-only claude call. stdin is /dev/null: under `curl | sh` it is
# the terminal, which a claude would read from or stop on. A stalled claude
# mustn't hang the install, and a stopped one ignores TERM until KILLed.
# No --foreground: BusyBox's timeout lacks it (LH-613).
claude_read() {
  local wait=()
  ! command -v timeout >/dev/null || wait=(timeout -k 5 10)
  ${wait[@]+"${wait[@]}"} claude "$@" </dev/null 2>/dev/null
}

skills_marketplace_dir() {
  claude_read plugin marketplace list --json |
    jq -r --arg m "$skills_marketplace" '.[] | select(.name == $m) | .installLocation' 2>/dev/null
}

# Prints the plugin's skill names from its manifest: in the repo and at the
# commit the added marketplace's catalog names, else at mattpocock/skills HEAD.
# Fails when it can't read them.
skills_plugin_names() {
  local catalog repo=mattpocock/skills ref=HEAD source
  catalog="$(skills_marketplace_dir)/.claude-plugin/marketplace.json"
  if [[ -r "$catalog" ]]; then
    source=$(jq -r --arg p "$skills_plugin" '
      first(.plugins[] | select(.name == $p) | .source) |
      ((.repo // (.url // "" | capture("github\\.com[/:](?<r>[^/]+/[^/]+?)(\\.git)?/*$").r)) + " " + (.sha // "HEAD"))
    ' "$catalog" 2>/dev/null) || return 1
    read -r repo ref <<<"$source"
    [[ -n "$repo" && -n "$ref" ]] || return 1
  fi
  command -v curl >/dev/null || return 1
  # A manifest's skills is a path or a list of them; without one Claude scans
  # skills/, which a single fetch can't list, so that counts as unreadable.
  curl -fsSL --max-time 10 "https://raw.githubusercontent.com/$repo/$ref/.claude-plugin/plugin.json" 2>/dev/null |
    jq -er '
      .skills | if type == "string" then [.] elif type == "array" then . else error("no skills") end |
      .[] | sub("/+$"; "") | split("/") | last | select(. != "" and . != "." and . != "..")
    ' 2>/dev/null
}

skills_list_overlaps() {
  local skills_dir=$1 names name overlaps=()
  if ! names=$(skills_plugin_names); then
    echo "could not read the $skills_plugin plugin's skill names, so overlaps with $skills_dir are not listed"
    return 0
  fi
  while IFS= read -r name; do
    if [[ -n "$name" && (-e "$skills_dir/$name" || -L "$skills_dir/$name") ]]; then
      overlaps+=("  $skills_dir/$name")
    fi
  done <<<"$names"
  ((${#overlaps[@]})) || return 0
  echo "the $skills_plugin plugin has skills named like these of yours; both will load, the plugin's as $skills_plugin:<name>:"
  printf '%s\n' "${overlaps[@]}"
}

skills_installed() {
  command -v claude >/dev/null && claude_read plugin list --json | jq -e --arg id "$skills_id" \
    'any(.[]; .id == $id and (.scope == "user" or .scope == "managed"))' >/dev/null 2>&1
}

# Usage: offer_skills <ask|yes|no|update> <claude config dir> [--name-dir]
# --name-dir names the config dir in what it prints, for a dir that isn't the human's default.
# update installs nothing: Claude updates its own plugins, so it only prints how.
offer_skills() {
  local want=$1 skills_dir=$2/skills answer where= env=
  [[ "${3:-}" != --name-dir ]] || where=" in $2" env="CLAUDE_CONFIG_DIR=$2 "
  [[ "$want" != no ]] || return 0
  if [[ "$want" == update ]]; then
    skills_installed || return 0
    echo "the $skills_plugin plugin$where updates through Claude: ${env}claude plugin update $skills_id"
    return 0
  fi
  if skills_installed; then
    echo "the $skills_plugin plugin is already installed$where; left as is"
    return 0
  fi
  if [[ "$want" == ask && ! -t 0 ]]; then
    echo "skipped the $skills_plugin plugin (no terminal); ${LOOPHERD_EXTRAS_NO_TERMINAL:-add --with-skills to install it}"
    return 0
  fi
  if ! command -v claude >/dev/null; then
    echo "skipped the $skills_plugin plugin: no claude CLI on PATH"
    return 0
  fi
  skills_list_overlaps "$skills_dir"
  if [[ "$want" == ask ]]; then
    read -r -p "Install the $skills_plugin plugin ($skills_id)$where? [y/N] " answer || answer=
    [[ "$answer" == [yY]* ]] || { echo "skipped the $skills_plugin plugin"; return 0; }
  fi
  if [[ -z "$(skills_marketplace_dir)" ]]; then
    claude plugin marketplace add "$skills_marketplace_repo" || {
      echo "install: could not add the $skills_marketplace_repo marketplace" >&2
      return 1
    }
  fi
  claude plugin install "$skills_id" || {
    echo "install: claude plugin install $skills_id failed" >&2
    return 1
  }
  echo "installed the $skills_plugin plugin$where"
}
