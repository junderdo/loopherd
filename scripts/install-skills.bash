# Sourced by scripts/install: offers the mattpocock-skills plugin, leaving the
# human's own skills as they are (docs/decisions/D26.md).

skills_plugin=mattpocock-skills
skills_marketplace=claude-plugins-official
skills_marketplace_repo=anthropics/claude-plugins-official
skills_id="$skills_plugin@$skills_marketplace"

skills_marketplace_dir() {
  claude plugin marketplace list --json 2>/dev/null |
    jq -r --arg m "$skills_marketplace" '.[] | select(.name == $m) | .installLocation' 2>/dev/null
}

# Prints the plugin's skill names from its manifest, at the commit the added
# marketplace pins, else at the repo's HEAD; fails when it can't read them.
skills_plugin_names() {
  local catalog ref=
  catalog="$(skills_marketplace_dir)/.claude-plugin/marketplace.json"
  if [[ -r "$catalog" ]]; then
    ref=$(jq -r --arg p "$skills_plugin" '.plugins[] | select(.name == $p) | .source.sha // empty' "$catalog" 2>/dev/null) || ref=
  fi
  command -v curl >/dev/null || return 1
  curl -fsSL --max-time 10 "https://raw.githubusercontent.com/mattpocock/skills/${ref:-HEAD}/.claude-plugin/plugin.json" 2>/dev/null |
    jq -er '.skills[] | sub("/+$"; "") | split("/") | last' 2>/dev/null
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

# Usage: offer_skills <ask|yes|no> <claude config dir>
offer_skills() {
  local want=$1 skills_dir=$2/skills answer
  [[ "$want" != no ]] || return 0
  if [[ "$want" == ask && ! -t 0 ]]; then
    echo "skipped the $skills_plugin plugin (no terminal); add --with-skills to install it"
    return 0
  fi
  if ! command -v claude >/dev/null; then
    echo "skipped the $skills_plugin plugin: no claude CLI on PATH"
    return 0
  fi
  if claude plugin list --json 2>/dev/null | jq -e --arg id "$skills_id" 'any(.[]; .id == $id)' >/dev/null 2>&1; then
    echo "the $skills_plugin plugin is already installed; left as is"
    return 0
  fi
  skills_list_overlaps "$skills_dir"
  if [[ "$want" == ask ]]; then
    read -r -p "Install the $skills_plugin plugin ($skills_id)? [y/N] " answer || answer=
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
  echo "installed the $skills_plugin plugin"
}
