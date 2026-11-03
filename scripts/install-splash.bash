# The animated loopherd splash scripts/install opens with. Standalone too:
#   bash -c 'source scripts/install-splash.bash && splash_play'

splash_wants_to_play() {
  [[ -t 1 && -n "${TERM:-}" && "$TERM" != dumb && "${LOOPHERD_NO_SPLASH:-}" != 1 ]]
}

# Rows are built as two parallel ASCII strings: the text, where '#' paints as
# '█' and '%' as '░', and a kind per column that picks its colour.

# One glyph per letter of "loopherd", rows split on '|'.
splash_glyphs=(
  "#    |#    |#    |#    |#####"
  " ### |#   #|#   #|#   #| ### "
  " ### |#   #|#   #|#   #| ### "
  "#### |#   #|#### |#    |#    "
  "#   #|#   #|#####|#   #|#   #"
  "#####|#    |#### |#    |#####"
  "#### |#   #|#### |#  # |#   #"
  "#### |#   #|#   #|#   #|#### "
)
splash_sheep_walking=('  .--.   ' ' (@@@@)o ' '  "  "   ')
splash_sheep_grazing=('  .--.   ' ' (@@@@)  ' '  "  "o  ')
splash_tagline='keeps your coding agents working together'
splash_width=54
splash_drop=3
splash_frames=60
splash_frame_us=50000

# Scene rows, top to bottom.
splash_wordmark_top=2 # sun rows 0..7 sit behind it
splash_horizon=8
splash_ground_top=9 # far, mid, near, then the front sheep's three rows and grass
splash_tagline_row=17
splash_height=18

# Far-to-near sheep: row, kind, sprite, start columns, frames per one-column step.
splash_herd=(
  "9 f @ 8,30,45 30"
  "10 m @o 4,38 20"
  "11 n (@@)o 18,44 15"
)
splash_ground=(
  "a|         .                  .              "
  "b|    .        ,         .          ,       "
  "c|  ,    .   '    ,   .      ,     '   .    "
  "d|      ,                 '            ,    "
  "d|  '                ,              .       "
  "d| ,  '  ,   .  , ' ,  '  .  ,  ' .  , '  , "
  "e|,.'\`\"',.,,'\`.\",',.,.'\`\"',.,,'\`.\",',.,.'\`\"',"
)

splash_buf=''
splash_sgr=()

splash_set_sgr() { # <kind> <24-bit sgr> <256-colour sgr>
  local code
  printf -v code '%d' "'$1"
  if [[ "${COLORTERM:-}" == truecolor || "${COLORTERM:-}" == 24bit ]]; then
    splash_sgr[code]=$2
  else
    splash_sgr[code]=$3
  fi
}

splash_colours() {
  splash_sgr=()
  [[ -n "${NO_COLOR:-}" ]] && return
  local row
  local -a purple_to_orange=(93 134 169 203 208)
  for row in 0 1 2 3 4; do
    splash_set_sgr "$((2 + row))" \
      "1;38;2;$((150 + 105 * row / 4));$((50 + 90 * row / 4));$((230 - 230 * row / 4))" \
      "1;38;5;${purple_to_orange[row]}"
  done
  splash_set_sgr 1 '38;2;255;150;40' '38;5;214'
  splash_set_sgr h '38;2;120;110;140' '38;5;60'
  splash_set_sgr f '38;2;120;125;150' '38;5;103'
  splash_set_sgr m '38;2;160;165;190' '38;5;146'
  splash_set_sgr n '38;2;205;205;220' '38;5;252'
  splash_set_sgr w '1;38;2;255;255;255' '1;38;5;231'
  splash_set_sgr a '38;2;40;80;55' '38;5;22'
  splash_set_sgr b '38;2;50;105;60' '38;5;28'
  splash_set_sgr c '38;2;60;135;65' '38;5;34'
  splash_set_sgr d '38;2;75;165;70' '38;5;70'
  splash_set_sgr e '38;2;95;195;80' '38;5;76'
  splash_set_sgr t '2' '2'
}

# Appends <text> at screen row <row> of the scene, one colour per run of <kinds>.
splash_paint_row() { # <row> <text> <kinds>
  local text=$2 kinds=$3 kind rest run code sgr
  splash_buf+=$'\e['"$((splash_top + $1));${splash_left}H"
  while [[ -n "$kinds" ]]; do
    kind=${kinds:0:1}
    rest=${kinds##+("$kind")}
    run=${text:0:${#kinds}-${#rest}}
    text=${text:${#kinds}-${#rest}}
    kinds=$rest
    run=${run//#/█}
    run=${run//%/░}
    printf -v code '%d' "'$kind"
    sgr=${splash_sgr[code]:-}
    if [[ -n "$sgr" && "$kind" != 0 ]]; then
      splash_buf+=$'\e['"${sgr}m${run}"$'\e[0m'
    else
      splash_buf+=$run
    fi
  done
}

splash_blank() { printf -v "$1" '%*s' "$splash_width" ''; }

splash_fill() { # <var> <char>
  splash_blank "$1"
  printf -v "$1" '%s' "${!1// /$2}"
}

# A sun setting into the horizon, centred behind the wordmark.
splash_draw_sun() {
  local row col dx dy line
  splash_sun_rows=()
  for ((row = 0; row < splash_horizon; row++)); do
    line=''
    for ((col = 0; col < splash_width; col++)); do
      dx=$((col - splash_width / 2)) dy=$((row - 5))
      if ((dx * dx + 4 * dy * dy <= 110)); then line+='%'; else line+=' '; fi
    done
    splash_sun_rows[row]=$line
  done
}

splash_sun_kinds() { # <text>
  local kinds=${1//%/1}
  printf '%s' "${kinds// /0}"
}

# Wordmark row <row> with letters before <letter> landed, <letter> <rise> rows short.
splash_wordmark_row() { # <row> <letter> <rise>
  local row=$1 letter=$2 rise=$3 i glyph_row mask='' col sun text='' kinds=''
  local -a rows
  for i in "${!splash_glyphs[@]}"; do
    ((i > 0)) && mask+='  '
    glyph_row=$row
    ((i == letter)) && glyph_row=$((row + rise))
    if ((i > letter || glyph_row > 4)); then
      mask+='     '
    else
      IFS='|' read -ra rows <<<"${splash_glyphs[i]}"
      mask+=${rows[glyph_row]}
    fi
  done
  sun=${splash_sun_rows[splash_wordmark_top + row]}
  for ((col = 0; col < splash_width; col++)); do
    if [[ "${mask:col:1}" == '#' ]]; then
      text+='#' kinds+=$((2 + row))
    elif [[ "${sun:col:1}" == '%' ]]; then
      text+='%' kinds+=1
    else
      text+=' ' kinds+=0
    fi
  done
  splash_paint_row "$((splash_wordmark_top + row))" "$text" "$kinds"
}

splash_wordmark() { # <letter> <rise>
  local row
  for row in 0 1 2 3 4; do splash_wordmark_row "$row" "$1" "$2"; done
}

splash_sky() {
  local row text horizon
  for row in 0 1 7; do
    text=${splash_sun_rows[row]}
    splash_paint_row "$row" "$text" "$(splash_sun_kinds "$text")"
  done
  splash_fill horizon '-'
  splash_paint_row "$splash_horizon" "$horizon" "${horizon//-/h}"
}

# Lays <sprite> over the row in progress at column <x>, coloured <kind>.
splash_overlay() { # <x> <sprite> <kind>
  local x=$1 sprite=$2 kind=$3 kinds
  ((x + ${#sprite} <= splash_width)) || return 0
  splash_fill kinds "$kind"
  splash_text="${splash_text:0:x}${sprite}${splash_text:x+${#sprite}}"
  splash_kinds="${splash_kinds:0:x}${kinds:0:${#sprite}}${splash_kinds:x+${#sprite}}"
}

splash_ground_row() { # <index>
  local entry=${splash_ground[$1]} texture kind
  kind=${entry%%|*}
  texture=${entry#*|}
  while ((${#texture} < splash_width)); do texture+=$texture; done
  splash_text=${texture:0:splash_width}
  splash_kinds=${splash_text//[! ]/$kind}
  splash_kinds=${splash_kinds// /0}
}

# Further back is smaller, higher, slower and dimmer; the front pair walk and graze.
splash_pasture() { # <frame>
  local frame=$1 entry row kind sprite starts period x s t r
  for entry in "${splash_herd[@]}"; do
    read -r row kind sprite starts period <<<"$entry"
    splash_ground_row $((row - splash_ground_top))
    for x in ${starts//,/ }; do
      splash_overlay $((x + frame / period)) "$sprite" "$kind"
    done
    splash_paint_row "$row" "$splash_text" "$splash_kinds"
  done
  for r in 0 1 2; do
    splash_ground_row $((3 + r))
    for s in 0 1; do
      t=$((frame + 6 * s))
      x=$((2 + 26 * s + t / 15 * 3))
      if (((t / 6) % 2)); then
        sprite=${splash_sheep_grazing[r]}
      else
        sprite=${splash_sheep_walking[r]}
      fi
      splash_overlay "$x" "$sprite" w
    done
    splash_paint_row $((splash_ground_top + 3 + r)) "$splash_text" "$splash_kinds"
  done
  splash_ground_row 6
  splash_paint_row $((splash_ground_top + 6)) "$splash_text" "$splash_kinds"
}

splash_show_tagline() {
  local pad=$(((splash_width - ${#splash_tagline}) / 2)) text kinds
  printf -v text '%*s%s' "$pad" '' "$splash_tagline"
  splash_blank kinds
  text="${text}${kinds:${#text}}"
  kinds=${text//[! ]/t}
  splash_paint_row "$splash_tagline_row" "$text" "${kinds// /0}"
}

splash_now_us() {
  local now=${EPOCHREALTIME:-}
  printf '%s' "${now//[!0-9]/}"
}

# Sleeps until <frame> is due, or a plain frame's length where the clock is missing.
splash_wait() { # <start_us> <frame>
  local now wait
  now=${EPOCHREALTIME:-}
  now=${now//[!0-9]/}
  if [[ -z "$1" || -z "$now" ]]; then
    sleep 0.05
    return
  fi
  wait=$(($1 + ($2 + 1) * splash_frame_us - now))
  ((wait > 0)) || return 0
  printf -v wait '%d.%06d' $((wait / 1000000)) $((wait % 1000000))
  sleep "$wait"
}

splash_full() {
  local frame letter rise letters=${#splash_glyphs[@]} start
  splash_draw_sun
  start=$(splash_now_us)
  printf '\e[?1049h\e[2J'
  splash_alt_screen=1
  for ((frame = 0; frame < splash_frames; frame++)); do
    letter=$((frame / splash_drop))
    rise=$((splash_drop - 1 - frame % splash_drop))
    ((letter < letters)) || { letter=$letters; rise=0; }
    splash_buf=''
    if ((frame == 0)); then splash_sky; fi
    if ((frame <= letters * splash_drop)); then splash_wordmark "$letter" "$rise"; fi
    if ((frame == letters * splash_drop)); then splash_show_tagline; fi
    splash_pasture "$frame"
    printf '%s' "$splash_buf"
    splash_wait "$start" "$frame"
  done
}

# Narrow or short terminals get the name typed out on one line instead.
splash_title() {
  local word=loopherd i code sgr
  printf '  '
  for ((i = 0; i < ${#word}; i++)); do
    printf -v code '%d' "'$((2 + i * 4 / 7))"
    sgr=${splash_sgr[code]:-}
    if [[ -n "$sgr" ]]; then
      printf '\e[%sm%s\e[0m' "$sgr" "${word:i:1}"
    else
      printf '%s' "${word:i:1}"
    fi
    sleep 0.1
  done
  printf '\n\n'
}

splash_restore() {
  [[ -n "${NO_COLOR:-}" ]] || printf '\e[0m'
  printf '\e[?25h'
  if [[ -n "${splash_alt_screen:-}" ]]; then printf '\e[?1049l'; fi
  splash_alt_screen=
}

splash_play() {
  local size rows= cols= extglob
  # stty reads stdin, which may not be the terminal; fd 3 is stdout from outside the $(...).
  { size=$(stty size <&3 2>/dev/null); } 3>&1 || return 0
  read -r rows cols <<<"$size"
  [[ "$rows" =~ ^[1-9][0-9]*$ && "$cols" =~ ^[1-9][0-9]*$ ]] || return 0
  extglob=$(shopt -p extglob) || true
  shopt -s extglob
  splash_colours
  splash_alt_screen=
  trap 'trap - EXIT; splash_restore; exit 130' INT
  trap splash_restore EXIT
  printf '\e[?25l'
  if ((cols < splash_width + 4 || rows < splash_height + 2)); then
    splash_title
  else
    splash_top=$(((rows - splash_height) / 2 + 1))
    splash_left=$(((cols - splash_width) / 2 + 1))
    splash_full
  fi
  splash_restore
  trap - INT EXIT
  eval "$extglob"
}
