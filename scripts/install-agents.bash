# Sourced by scripts/install: offers loopherd's agent types without ever
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

# Prints nothing when $target needs no question: absent, already loopherd's
# content, or unchanged since loopherd installed it.
agents_conflict() {
  local want=$1 target=$2 sha recorded
  [[ -e "$target" ]] || return 0
  sha=$(agents_sha "$target")
  [[ "$sha" != "$(agents_sha "$want")" ]] || return 0
  recorded=$(agents_recorded "$target")
  [[ "$sha" != "$recorded" ]] || return 0
  if [[ -n "$recorded" ]]; then echo "edited since loopherd installed it"; else echo "yours, not loopherd's"; fi
}

# Loopherd's text for $1 installed as loopherd-$1, which Claude knows by that name.
agents_aliased() {
  sed "1,/^---\$/ s/^name: $1\$/name: loopherd-$1/" "$agents_src/$1.md" >"$agents_tmp/loopherd-$1.md"
  echo "$agents_tmp/loopherd-$1.md"
}

agents_place() {
  local name=$1 want=$2 target=$3 can_alias=$4 conflict choice backup
  if [[ ! -e "$target" ]]; then
    agents_copy "$want" "$target"
    echo "installed $target"
    return
  fi
  if [[ "$(agents_sha "$target")" == "$(agents_sha "$want")" ]]; then
    agents_record "$target" "$(agents_sha "$want")"
    echo "$target already up to date"
    return
  fi
  conflict=$(agents_conflict "$want" "$target")
  if [[ -z "$conflict" ]]; then
    agents_copy "$want" "$target"
    echo "updated $target"
    return
  fi
  if [[ ! -t 0 ]]; then
    echo "kept $target ($conflict); run scripts/install on a terminal to choose"
    return
  fi
  local options="[k]eep yours, [o]verwrite after a backup"
  [[ -z "$can_alias" ]] || options+=", or install loopherd's as loopherd-$name.md [l]"
  read -r -p "$target is $conflict. $options? [K/o${can_alias:+/l}] " choice || choice=
  case "$choice" in
    o | O)
      backup="$target.$(date -u +%Y%m%dT%H%M%SZ).bak"
      cp "$target" "$backup"
      agents_copy "$want" "$target"
      echo "overwrote $target; backup $backup"
      ;;
    l | L)
      if [[ -n "$can_alias" ]]; then
        agents_place "$name" "$(agents_aliased "$name")" "$agents_dir/loopherd-$name.md" ""
      else
        echo "kept $target"
      fi
      ;;
    *) echo "kept $target" ;;
  esac
}

# Sets pick_want, pick_target and pick_can_alias for agent $1. One installed
# as loopherd-<name> before stays there.
agents_pick() {
  local alias="$agents_dir/loopherd-$1.md"
  if [[ -e "$alias" && -n "$(agents_recorded "$alias")" ]]; then
    pick_want=$(agents_aliased "$1") pick_target=$alias pick_can_alias=
  else
    pick_want="$agents_src/$1.md" pick_target="$agents_dir/$1.md" pick_can_alias=1
  fi
}

# Usage: offer_agents <ask|--with-agents|--no-extras> <source dir> <agents dir> <ledger>
offer_agents() {
  local extras=$1 name conflict answer
  agents_src=$2 agents_dir=$3 agents_ledger=$4
  [[ "$extras" != --no-extras ]] || return 0
  if [[ "$extras" == ask && ! -t 0 ]]; then
    echo "skipped loopherd's agent types (no terminal); add --with-agents to install them"
    return 0
  fi
  agents_tmp=$(mktemp -d)
  local overlaps=()
  for name in "${agent_names[@]}"; do
    agents_pick "$name"
    conflict=$(agents_conflict "$pick_want" "$pick_target")
    [[ -z "$conflict" ]] || overlaps+=("  $pick_target: $conflict")
  done
  if ((${#overlaps[@]})); then
    echo "loopherd's agent types overlap these files in $agents_dir:"
    printf '%s\n' "${overlaps[@]}"
  fi
  if [[ "$extras" == ask ]]; then
    read -r -p "Install loopherd's agent types (${agent_names[*]}) into $agents_dir? [y/N] " answer || answer=
    [[ "$answer" == [yY]* ]] || { echo "skipped loopherd's agent types"; rm -rf "$agents_tmp"; return 0; }
  fi
  for name in "${agent_names[@]}"; do
    agents_pick "$name"
    agents_place "$name" "$pick_want" "$pick_target" "$pick_can_alias"
  done
  rm -rf "$agents_tmp"
}
