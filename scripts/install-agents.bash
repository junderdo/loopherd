# Sourced by scripts/install-extras: offers loopherd's agent types without ever
# overwriting a file the human owns or edited unasked (docs/decisions/D26.md).
#
# The ledger holds "<sha256> <path>" for each file loopherd installed, so a
# file whose hash still matches is loopherd's to update.

agent_names=(builder reviewer researcher scout)

agents_sha() {
  if command -v sha256sum >/dev/null; then sha256sum <"$1"; else shasum -a 256 <"$1"; fi | cut -d' ' -f1
}

agents_recorded() {
  [[ -r "$agents_ledger" ]] || return 0
  LEDGER_PATH="$1" awk '{ h = $1; sub(/^[^ ]+ /, ""); if ($0 == ENVIRON["LEDGER_PATH"]) r = h } END { print r }' "$agents_ledger"
}

agents_record() {
  local path=$1 sha=$2 tmp
  mkdir -p "$(dirname "$agents_ledger")"
  tmp=$(mktemp "$agents_ledger.XXXXXX")
  if [[ -r "$agents_ledger" ]]; then
    LEDGER_PATH="$path" awk '{ line = $0; sub(/^[^ ]+ /, ""); if ($0 != ENVIRON["LEDGER_PATH"]) print line }' "$agents_ledger" >"$tmp"
  fi
  printf '%s %s\n' "$sha" "$path" >>"$tmp"
  mv -f "$tmp" "$agents_ledger"
}

# Copies by rename, so a symlink at $2 is replaced rather than written through.
agents_copy() {
  local want=$1 target=$2 tmp
  mkdir -p "$(dirname "$target")"
  tmp=$(mktemp "$(dirname "$target")/.loopherd-agent.XXXXXX")
  cp "$want" "$tmp"
  chmod 644 "$tmp"
  mv -f "$tmp" "$target"
  agents_record "$target" "$(agents_sha "$want")"
}

# Prints each other file under the agents dir whose frontmatter declares
# name: $1, since Claude knows an agent by that name, not by its filename.
agents_declaring() {
  [[ -d "$agents_dir" ]] || return 0
  local file
  while IFS= read -r -d '' file; do
    [[ "$file" != "$2" ]] || continue
    awk -v n="$1" '
      NR == 1 && $0 != "---" { exit }
      NR > 1 && $0 == "---" { exit }
      NR > 1 && sub(/^name:[ \t]*/, "") { sub(/[ \t]+$/, ""); gsub(/^["'\'']|["'\'']$/, ""); found = ($0 == n); exit }
      END { exit !found }' "$file" 2>/dev/null && echo "$file"
  done < <(find "$agents_dir" \( -type f -o -type l \) -name '*.md' -print0)
  return 0
}

# Sets conflict to why $2 needs a question, empty when it is absent, already
# loopherd's text, or unchanged since loopherd installed it. Sets
# conflict_elsewhere when the clash is another file, so overwriting $2 can't
# settle it, and conflict_ours when $2 is then still as loopherd installed it. Anything it can't read or hash counts as the human's.
agents_check() {
  local want=$1 target=$2 agent=$3 sha recorded others
  conflict= conflict_elsewhere= conflict_ours=
  others=$(agents_declaring "$agent" "$target")
  if [[ -n "$others" ]]; then
    conflict="${others//$'\n'/, } already declares name: $agent" conflict_elsewhere=1
    if [[ -f "$target" && -r "$target" && ! -L "$target" ]]; then
      sha=$(agents_sha "$target")
      [[ "$sha" != "$(agents_recorded "$target")" ]] || conflict_ours=1
    fi
    return
  fi
  [[ -e "$target" || -L "$target" ]] || return 0
  if [[ ! -f "$target" || ! -r "$target" ]]; then
    conflict="yours and unreadable"
    return
  fi
  sha=$(agents_sha "$target")
  [[ -n "$sha" && "$sha" != "$(agents_sha "$want")" ]] || return 0
  if [[ -L "$target" ]]; then
    conflict="a symlink you own"
    return
  fi
  recorded=$(agents_recorded "$target")
  [[ -z "$recorded" || "$sha" != "$recorded" ]] || return 0
  if [[ -n "$recorded" ]]; then conflict="edited since loopherd installed it"; else conflict="yours, not loopherd's"; fi
}

# Loopherd's text for $1 installed as loopherd-$1, which Claude knows by that name.
agents_aliased() {
  sed "1,/^---\$/ s/^name: $1\$/name: loopherd-$1/" "$agents_src/$1.md" >"$agents_tmp/loopherd-$1.md"
  echo "$agents_tmp/loopherd-$1.md"
}

agents_place() {
  local name=$1 want=$2 target=$3 agent=$4 can_alias=$5 choice backup
  agents_check "$want" "$target" "$agent"
  if [[ -z "$conflict" ]]; then
    if [[ ! -e "$target" && ! -L "$target" ]]; then
      agents_copy "$want" "$target"
      echo "installed $target"
    elif [[ "$(agents_sha "$target")" == "$(agents_sha "$want")" ]]; then
      [[ -L "$target" ]] || agents_record "$target" "$(agents_sha "$want")"
      echo "$target already up to date"
    else
      agents_copy "$want" "$target"
      echo "updated $target"
    fi
    return
  fi
  if [[ -n "$conflict_ours" ]]; then
    echo "left loopherd's $target as is: $conflict"
    return
  fi
  if [[ ! -t 0 ]]; then
    echo "kept $target ($conflict); rerun the installer on a terminal to choose"
    return
  fi
  local can_overwrite=
  [[ -n "$conflict_elsewhere" || ! -f "$target" || ! -r "$target" ]] || can_overwrite=1
  local options="[k]eep yours" keys=K
  [[ -z "$can_overwrite" ]] || options+=", [o]verwrite after a backup" keys+=/o
  [[ -z "$can_alias" ]] || options+=", or install loopherd's as loopherd-$name.md [l]" keys+=/l
  read -r -p "$target: $conflict. $options? [$keys] " choice || choice=
  local action=keep
  case "$choice" in
    o | O) [[ -z "$can_overwrite" ]] || action=overwrite ;;
    l | L) [[ -z "$can_alias" ]] || action=alias ;;
  esac
  case "$action" in
    overwrite)
      backup="$target.$(date -u +%Y%m%dT%H%M%SZ).bak"
      if ! cp -P "$target" "$backup"; then
        echo "kept $target: could not back it up to $backup"
        return
      fi
      agents_copy "$want" "$target"
      echo "overwrote $target; backup $backup"
      ;;
    alias) agents_place "$name" "$(agents_aliased "$name")" "$agents_dir/loopherd-$name.md" "loopherd-$name" "" ;;
    keep) echo "kept $target" ;;
  esac
}

# The ledger keys on paths, so every spelling of the dir must name it the same way.
agents_resolve_dir() {
  [[ ! -d "$agents_dir" ]] || agents_dir=$(cd "$agents_dir" && pwd -P)
}

# Sets pick_want, pick_target, pick_agent and pick_can_alias for agent $1. One installed
# as loopherd-<name> before stays there.
agents_pick() {
  local alias="$agents_dir/loopherd-$1.md"
  if [[ -e "$alias" && -n "$(agents_recorded "$alias")" ]]; then
    pick_want=$(agents_aliased "$1") pick_target=$alias pick_agent="loopherd-$1" pick_can_alias=
  else
    pick_want="$agents_src/$1.md" pick_target="$agents_dir/$1.md" pick_agent=$1 pick_can_alias=1
  fi
}

# Usage: offer_agents <ask|--with-agents|--no-extras|--update> <source dir> <agents dir> <ledger>
# --update refreshes only the agent types still in the agents dir that the ledger says loopherd installed.
offer_agents() {
  local extras=$1 name answer
  agents_src=$2 agents_dir=$3 agents_ledger=$4
  [[ "$extras" != --no-extras ]] || return 0
  agents_resolve_dir
  agents_tmp=$(mktemp -d)
  trap 'rm -rf "$agents_tmp"' EXIT
  if [[ "$extras" == --update ]]; then
    for name in "${agent_names[@]}"; do
      agents_pick "$name"
      [[ -z "$(agents_recorded "$pick_target")" || ! (-e "$pick_target" || -L "$pick_target") ]] || agents_place "$name" "$pick_want" "$pick_target" "$pick_agent" "$pick_can_alias"
    done
    rm -rf "$agents_tmp"
    return 0
  fi
  local overlaps=() installed=1
  for name in "${agent_names[@]}"; do
    agents_pick "$name"
    agents_check "$pick_want" "$pick_target" "$pick_agent"
    [[ -z "$conflict" ]] || overlaps+=("  $pick_target: $conflict")
    [[ -z "$conflict" && -f "$pick_target" && "$(agents_sha "$pick_target")" == "$(agents_sha "$pick_want")" ]] || installed=
  done
  # Unasked, so the ledger is left alone: a copy the human made stays theirs.
  if [[ -n "$installed" && "$extras" == ask ]]; then
    echo "loopherd's agent types are already installed in $agents_dir; left as is"
    rm -rf "$agents_tmp"
    return 0
  fi
  if [[ "$extras" == ask && ! -t 0 ]]; then
    echo "skipped loopherd's agent types (no terminal); ${LOOPHERD_EXTRAS_NO_TERMINAL:-add --with-agents to install them}"
    rm -rf "$agents_tmp"
    return 0
  fi
  if ((${#overlaps[@]})); then
    echo "loopherd's agent types overlap these files in $agents_dir:"
    printf '%s\n' "${overlaps[@]}"
  fi
  if [[ "$extras" == ask ]]; then
    read -r -p "Install loopherd's agent types (${agent_names[*]}) into $agents_dir? [y/N] " answer || answer=
    [[ "$answer" == [yY]* ]] || { echo "skipped loopherd's agent types"; rm -rf "$agents_tmp"; return 0; }
  fi
  mkdir -p "$agents_dir"
  agents_resolve_dir
  for name in "${agent_names[@]}"; do
    agents_pick "$name"
    agents_place "$name" "$pick_want" "$pick_target" "$pick_agent" "$pick_can_alias"
  done
  rm -rf "$agents_tmp"
}
