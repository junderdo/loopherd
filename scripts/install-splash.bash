# Sourced by scripts/install: the animated wordmark and pasture it opens with.

splash_wants_to_play() {
  [[ -t 1 && -n "${TERM:-}" && "$TERM" != dumb && "${LOOPHERD_NO_SPLASH:-}" != 1 ]]
}

# One glyph per letter of "loopherd", rows split on '|'.
splash_glyphs=(
  "█    |█    |█    |█    |█████"
  " ███ |█   █|█   █|█   █| ███ "
  " ███ |█   █|█   █|█   █| ███ "
  "████ |█   █|████ |█    |█    "
  "█   █|█   █|█████|█   █|█   █"
  "█████|█    |████ |█    |█████"
  "████ |█   █|████ |█  █ |█   █"
  "████ |█   █|█   █|█   █|████ "
)
splash_letter_colours=(46 47 48 49 50 51 45 39)
splash_sheep_walking=('  .--.   ' ' (@@@@)o ' '  "  "   ')
splash_sheep_grazing=('  .--.   ' ' (@@@@)  ' '  "  "o  ')
splash_tagline='keeps your coding agents working together'
splash_width=54
splash_height=11
splash_drop=3
splash_frames=50

# Frames are built in splash_buf and written whole, so Ctrl+C never lands mid-frame.
splash_buf=''

splash_paint() { # <sgr> <text>
  if [[ -n "${NO_COLOR:-}" ]]; then
    splash_buf+=$2
  else
    splash_buf+=$'\e['"$1m$2"$'\e[0m'
  fi
}

splash_line_start() { splash_buf+=$'\e[2K  '; }

splash_restore() {
  [[ -n "${NO_COLOR:-}" ]] || printf '\e[0m'
  printf '\e[?25h'
}

# The wordmark with letters before <letter> landed and <letter> <rise> rows short of landing.
splash_wordmark() { # <letter> <rise>
  local letter=$1 rise=$2 row i glyph_row
  local -a rows
  for row in 0 1 2 3 4; do
    splash_line_start
    for i in "${!splash_glyphs[@]}"; do
      ((i > 0)) && splash_buf+='  '
      glyph_row=$row
      ((i == letter)) && glyph_row=$((row + rise))
      if ((i > letter || glyph_row > 4)); then
        splash_buf+='     '
      else
        IFS='|' read -ra rows <<<"${splash_glyphs[i]}"
        splash_paint "1;38;5;${splash_letter_colours[i]}" "${rows[glyph_row]}"
      fi
    done
    splash_buf+=$'\n'
  done
}

splash_tagline() { # <shown>
  splash_line_start
  (($1)) && splash_paint 2 "$splash_tagline"
  splash_buf+=$'\n\e[2K\n'
}

# Two sheep that walk a few steps, then graze a while, out of step with each other.
splash_pasture() { # <frame>
  local frame=$1 row s x t sprite_row line
  local -a xs=(2 28)
  printf -v line '%*s' "$splash_width" ''
  local blank=$line
  for row in 0 1 2; do
    line=$blank
    for s in 0 1; do
      t=$((frame + 6 * s))
      x=$((xs[s] + t / 12 * 3))
      if (((t / 6) % 2)); then
        sprite_row=${splash_sheep_grazing[row]}
      else
        sprite_row=${splash_sheep_walking[row]}
      fi
      line="${line:0:x}${sprite_row}${line:x+${#sprite_row}}"
    done
    splash_line_start
    splash_paint '1;97' "${line:0:splash_width}"
    splash_buf+=$'\n'
  done
  line=''
  while ((${#line} < splash_width)); do line+=",.'\`\"',.,"; done
  splash_line_start
  splash_paint 32 "${line:0:splash_width}"
  splash_buf+=$'\n'
}

splash_full() {
  local frame letter rise letters=${#splash_glyphs[@]}
  printf '%*s' "$splash_height" '' | tr ' ' '\n'
  for ((frame = 0; frame < splash_frames; frame++)); do
    letter=$((frame / splash_drop))
    rise=$((splash_drop - 1 - frame % splash_drop))
    ((letter < letters)) || { letter=$letters; rise=0; }
    printf -v splash_buf '\e[%dA' "$splash_height"
    splash_wordmark "$letter" "$rise"
    splash_tagline $((letter == letters))
    splash_pasture "$frame"
    printf '%s' "$splash_buf"
    sleep 0.05
  done
}

# Narrow terminals get the name typed out on one line instead.
splash_title() {
  local word=loopherd i
  printf '  '
  for ((i = 0; i < ${#word}; i++)); do
    splash_buf=''
    splash_paint "1;38;5;${splash_letter_colours[i]}" "${word:i:1}"
    printf '%s' "$splash_buf"
    sleep 0.1
  done
  printf '\n\n'
}

splash_play() {
  local cols
  cols=$(tput cols 2>/dev/null) || cols=${COLUMNS:-80}
  trap 'trap - EXIT; splash_restore; printf "\n"; exit 130' INT
  trap splash_restore EXIT
  printf '\e[?25l'
  if ((cols < splash_width + 4)); then
    splash_title
  else
    splash_full
  fi
  splash_restore
  trap - INT EXIT
}
