# The animated loopherd splash scripts/install opens with. Standalone too:
#   bash -c 'source scripts/install-splash.bash && splash_play'

splash_wants_to_play() {
  [[ -t 1 && -n "${TERM:-}" && "$TERM" != dumb && "${LOOPHERD_NO_SPLASH:-}" != 1 ]]
}

# Rows are built as two parallel ASCII strings: the text, where '#' paints as
# '█', '%' as the sun and '&' as cloud, and a kind per column that picks its colour.
# Cloud is kind c, or x over the sun's core and y over its rim, on the sun's colour.

# One glyph per letter of "loopherd", rows split on '|'.
splash_glyphs=(
  "##    |##    |##    |##    |######"
  " #### |##  ##|##  ##|##  ##| #### "
  " #### |##  ##|##  ##|##  ##| #### "
  "##### |##  ##|##### |##    |##    "
  "##  ##|##  ##|######|##  ##|##  ##"
  "######|##    |##### |##    |######"
  "##### |##  ##|##### |## ## |##  ##"
  "##### |##  ##|##  ##|##  ##|##### "
)
splash_tagline='keeps your coding agents working together'
splash_drop=4
splash_frames=100
splash_frame_us=50000

# Three layouts, by terminal size (splash_layout):
#   title   under 66 columns or 23 rows: the name typed out on one line.
#   medium  the 62x21 scene, centred.
#   large   140 columns and 40 rows or more: full-bleed. Sky, horizon and pasture
#           span the width, the pasture runs to the row above the tagline on the
#           last row, and the herd grows with the area.
splash_large_cols=140
splash_large_rows=40
# Laying a sheep costs time in proportion to the width, so the herd is capped at
# this many sheep-columns, which keeps a frame under 50ms at 400x100.
splash_large_herd_budget=16000

# Scene rows, top to bottom; the ground and tagline rows are set by the layout.
splash_wordmark_top=2 # sun rows 0..7 sit behind it
splash_horizon=8
splash_ground_top=9
splash_ground_kinds=ABCDEFGHIJ

# The herd, far to near: row, colour kind, sprite size, start column, motion
# (r or l walks that way; gr or gl grazes in place facing that way, from size 2
# up), and frames per one-column step, or per head bob when grazing.
splash_medium_herd=(
  "9 f 1 14 r 40" "9 f 1 47 l 45"
  "10 f 1 30 l 35"
  "11 m 2 6 r 25" "11 m 2 40 gl 11"
  "12 m 2 54 l 22"
  "13 n 5 44 l 16" "13 n 5 12 gr 10"
  "14 n 5 22 r 14"
  "15 w 9 2 r 8" "15 w 9 26 gl 9" "15 w 9 46 l 10"
)
# Clouds, drawn over the sun and under the letters: top row, start column,
# columns per step (+1 or -1), frames per step, then the shape's rows.
splash_medium_clouds=(
  "0|18|1|14|   &&&&&     |&&&&&&&&&&&&&"
  "1|48|-1|20|&&&&&&&"
  "3|1|1|25| &&&&& |&&&&&&&"
  "6|36|-1|16|  &&&&&&    |&&&&&&&&&&&&"
)

splash_buf=''
splash_sgr=()

splash_layout() { # <rows> <cols>
  local rows=$1 cols=$2
  splash_width=62 splash_height=21 splash_ground_rows=10 splash_tagline_row=20
  splash_herd=("${splash_medium_herd[@]}") splash_clouds=("${splash_medium_clouds[@]}")
  if ((cols < splash_width + 4 || rows < splash_height + 2)); then
    splash_layout=title
  elif ((cols < splash_large_cols || rows < splash_large_rows)); then
    splash_layout=medium
    splash_top=$(((rows - splash_height) / 2 + 1))
    splash_left=$(((cols - splash_width) / 2 + 1))
  else
    splash_layout=large
    splash_width=$cols splash_height=$rows splash_top=1 splash_left=1
    splash_tagline_row=$((rows - 1))
    splash_ground_rows=$((rows - 1 - splash_ground_top))
  fi
  splash_wordmark_x=$(((splash_width - 62) / 2))
  # The shepherd stands front-on at the right of the foreground, leaning on a
  # crook, feet on the front row.
  splash_shepherd_row=$((splash_ground_top + splash_ground_rows - 6))
  splash_shepherd_x=$((splash_width - 8))
  if [[ "$splash_layout" == large ]]; then
    splash_large_herd
    splash_large_clouds
  fi
}

# A well-mixed hash of two numbers, 0 to 996.
splash_mix() { # <a> <b>
  splash_hash=$(((($1 * 7919 + $2 * 104729) * 2654435761 >> 16) % 997))
}

# A row of sheep every other ground row (every third for the 3-row front sheep),
# about one per 25 columns within splash_large_herd_budget, banded by
# depth like the medium herd. Like the medium herd, none ever reaches the shepherd.
splash_large_herd() {
  local d k step size kind base period n j x motion max=$((splash_large_herd_budget / splash_width))
  local -a moves=(r l gr gl) rows=()
  splash_herd=()
  for ((d = 0; d <= splash_ground_rows - 3; d += step)); do
    rows+=("$d")
    step=2
    (((d * 9 / (splash_ground_rows - 1)) >= 6)) && step=3
  done
  n=$((splash_width / 25))
  ((n * ${#rows[@]} <= max)) || n=$((max / ${#rows[@]}))
  ((n > 0)) || n=1
  for d in "${rows[@]}"; do
    k=$((d * 9 / (splash_ground_rows - 1)))
    if ((k <= 1)); then
      size=1 kind=f base=40
    elif ((k <= 3)); then
      size=2 kind=m base=22
    elif ((k <= 5)); then
      size=5 kind=n base=14
    else
      size=9 kind=w base=9
    fi
    for ((j = 0; j < n; j++)); do
      splash_mix "$j" "$d"
      x=$((j * splash_width / n + splash_hash % (splash_width / n)))
      motion=${moves[splash_hash % (size == 1 ? 2 : 4)]}
      period=$((base + splash_hash % 6))
      splash_clear_of_shepherd "$d" "$size" "$x" "$motion" $(((splash_frames - 1) / period)) || continue
      splash_herd+=("$((splash_ground_top + d)) $kind $size $x $motion $period")
    done
  done
}

# Whether a sheep never shares a row with the shepherd, or stays left of his
# columns for the whole splash.
splash_clear_of_shepherd() { # <ground row> <size> <x> <motion> <travel>
  local bottom=$1 right=$(($3 + $2 - 1))
  if (($2 == 9)); then bottom=$(($1 + 2)); fi
  if [[ "$4" == r ]]; then right=$((right + $5)); fi
  ((bottom < splash_shepherd_row - splash_ground_top || right < splash_shepherd_x))
}

# The medium clouds once per 62 columns, up to four copies, each shifted down
# and the odd ones drifting the other way.
splash_large_clouds() {
  local copies=$((splash_width / 62 < 4 ? splash_width / 62 : 4)) c entry IFS='|'
  local -a cloud
  splash_clouds=()
  for ((c = 0; c < copies; c++)); do
    for entry in "${splash_medium_clouds[@]}"; do
      read -ra cloud <<<"$entry"
      cloud[0]=$(((cloud[0] + 2 * c) % 7))
      cloud[1]=$((cloud[1] + c * splash_width / copies))
      ((c % 2)) && cloud[2]=$((-cloud[2]))
      splash_clouds+=("${cloud[*]}")
    done
  done
}

splash_set_sgr() { # <kind> <24-bit sgr> <256-colour sgr>
  local code
  printf -v code '%d' "'$1"
  if [[ "${COLORTERM:-}" == truecolor || "${COLORTERM:-}" == 24bit ]]; then
    splash_sgr[code]=$2
  else
    splash_sgr[code]=$3
  fi
}

# The colour band, 0 to 9, of ground row <d>.
splash_ground_depth() { # <d>
  splash_depth=$(($1 * 9 / (splash_ground_rows - 1)))
}

splash_colours() {
  splash_sgr=()
  # Without colour, letters over a solid sun would vanish.
  splash_sun_glyph='░'
  [[ -n "${NO_COLOR:-}" ]] && return
  splash_sun_glyph='█'
  local row d
  local -a purple_to_orange=(93 134 169 203 202) greens=(22 22 28 28 34 34 70 70 76 82)
  for row in 0 1 2 3 4; do
    splash_set_sgr "$((2 + row))" \
      "1;38;2;$((150 + 105 * row / 4));$((50 + 30 * row / 4));$((230 - 230 * row / 4))" \
      "1;38;5;${purple_to_orange[row]}"
  done
  for ((d = 0; d < 10; d++)); do
    splash_set_sgr "${splash_ground_kinds:d:1}" \
      "38;2;$((35 + 45 * d / 9));$((70 + 150 * d / 9));$((45 + 40 * d / 9))" \
      "38;5;${greens[d]}"
  done
  splash_set_sgr 1 '38;2;255;154;31' '38;5;208'
  splash_set_sgr u '38;2;255;192;77' '38;5;214'
  splash_set_sgr c '38;2;235;235;242' '38;5;254'
  splash_set_sgr x '38;2;235;235;242;48;2;255;192;77' '38;5;254;48;5;214'
  splash_set_sgr y '38;2;235;235;242;48;2;255;154;31' '38;5;254;48;5;208'
  splash_set_sgr h '38;2;120;110;140' '38;5;60'
  splash_set_sgr f '38;2;120;125;150' '38;5;103'
  splash_set_sgr m '38;2;160;165;190' '38;5;146'
  splash_set_sgr n '38;2;205;205;220' '38;5;252'
  splash_set_sgr w '1;38;2;255;255;255' '1;38;5;231'
  splash_set_sgr s '38;2;190;140;90' '38;5;173'
  splash_set_sgr t '2' '2'
}

# Appends <text> at screen row <row> of the scene, one colour per run of <kinds>.
splash_paint_row() { # <row> <text> <kinds>
  local text=$2 kinds=$3 kind run code sgr
  splash_buf+=$'\e['"$((splash_top + $1));${splash_left}H"
  while [[ -n "$kinds" ]]; do
    kind=${kinds:0:1}
    run=${kinds%%[!"$kind"]*}
    kinds=${kinds:${#run}}
    run=${text:0:${#run}}
    text=${text:${#run}}
    run=${run//#/█}
    run=${run//%/$splash_sun_glyph}
    run=${run//&/▒}
    printf -v code '%d' "'$kind"
    sgr=${splash_sgr[code]:-}
    if [[ -n "$sgr" && "$kind" != 0 ]]; then
      splash_buf+=$'\e['"${sgr}m${run}"$'\e[0m'
    else
      splash_buf+=$run
    fi
  done
}

splash_fill() { # <var> <char>
  printf -v "$1" '%*s' "$splash_width" ''
  printf -v "$1" '%s' "${!1// /$2}"
}

# Lays <sprite> over splash_text and splash_kinds from column <x>; spaces are
# see-through, '~' a solid blank. Cloud keeps the sun's colour behind it.
splash_lay() { # <x> <sprite> <kind>
  local x=$1 sprite=$2 kind=$3 i ch cell trimmed
  trimmed=${sprite#"${sprite%%[! ]*}"}
  x=$((x + ${#sprite} - ${#trimmed}))
  sprite=${trimmed%"${trimmed##*[! ]}"}
  if [[ "$sprite" != *' '* ]] && ((x >= 0 && x + ${#sprite} <= splash_width)); then
    cell=${splash_kinds:x:${#sprite}}
    if [[ "$kind" != c || "$cell" != *[u1xy]* ]]; then
      printf -v cell '%*s' "${#sprite}" ''
      splash_text="${splash_text:0:x}${sprite//\~/ }${splash_text:x+${#sprite}}"
      splash_kinds="${splash_kinds:0:x}${cell// /$kind}${splash_kinds:x+${#sprite}}"
      return
    fi
  fi
  for ((i = 0; i < ${#sprite}; i++)); do
    ch=${sprite:i:1}
    [[ "$ch" == ' ' ]] && continue
    [[ "$ch" == '~' ]] && ch=' '
    ((x + i >= 0 && x + i < splash_width)) || continue
    cell=$kind
    if [[ "$kind" == c ]]; then
      case ${splash_kinds:x+i:1} in
        u | x) cell=x ;;
        1 | y) cell=y ;;
      esac
    fi
    splash_text="${splash_text:0:x+i}${ch}${splash_text:x+i+1}"
    splash_kinds="${splash_kinds:0:x+i}${cell}${splash_kinds:x+i+1}"
  done
}

# A sun setting into the horizon, centred behind the wordmark, with a lighter core.
splash_draw_sun() {
  local row col dx dy text kinds reach
  splash_sun_text=() splash_sun_kinds=()
  for ((row = 0; row < splash_horizon; row++)); do
    text='' kinds=''
    for ((col = 0; col < splash_width; col++)); do
      dx=$((col - splash_width / 2)) dy=$((row - 5))
      reach=$((dx * dx + 4 * dy * dy))
      if ((reach <= 40)); then
        text+='%' kinds+=u
      elif ((reach <= 110)); then
        text+='%' kinds+=1
      else
        text+=' ' kinds+=0
      fi
    done
    splash_sun_text[row]=$text splash_sun_kinds[row]=$kinds
  done
}

# Grass marks, jittered across each row; no row has fewer than the one behind it,
# so they run from sparse dots at the horizon to blades in the foreground.
splash_draw_ground() {
  local d k count step col h marks line far=$((splash_ground_rows - 1))
  splash_ground_text=() splash_ground_kinds_row=()
  for ((d = 0; d < splash_ground_rows; d++)); do
    splash_ground_depth "$d"
    count=$(((4 + 32 * d * d / (far * far)) * splash_width / 100))
    marks='.'
    ((splash_depth >= 3)) && marks=".,'"
    ((splash_depth >= 6)) && marks=",'\"\`;"
    splash_fill line "${splash_ground_kinds:splash_depth:1}"
    splash_ground_kinds_row[d]=$line
    printf -v line '%*s' "$splash_width" ''
    step=$((splash_width * 1000 / count))
    for ((k = 0; k < count; k++)); do
      if [[ "$splash_layout" == large ]]; then
        splash_mix "$k" "$d"
        h=$splash_hash
      else
        h=$(((k * 7919 + d * 104729) % 997)) # the medium scene's original jitter
      fi
      col=$((((k * step + h * step / 997) / 1000 + d * 23) % splash_width))
      line="${line:0:col}${marks:h % ${#marks}:1}${line:col+1}"
    done
    splash_ground_text[d]=$line
  done
}

# Splits the glyph and cloud entries once, so frames don't.
splash_parse() {
  local i r IFS='|'
  local -a fields
  splash_glyph_rows=()
  for i in "${!splash_glyphs[@]}"; do
    read -ra fields <<<"${splash_glyphs[i]}"
    for r in 0 1 2 3 4; do splash_glyph_rows[i * 5 + r]=${fields[r]}; done
  done
  splash_cloud_top=() splash_cloud_x=() splash_cloud_step=() splash_cloud_period=()
  splash_cloud_rows=() splash_cloud_shape=()
  for i in "${!splash_clouds[@]}"; do
    read -ra fields <<<"${splash_clouds[i]}"
    splash_cloud_top[i]=${fields[0]} splash_cloud_x[i]=${fields[1]}
    splash_cloud_step[i]=${fields[2]} splash_cloud_period[i]=${fields[3]}
    splash_cloud_rows[i]=$((${#fields[@]} - 4))
    for ((r = 0; r < splash_cloud_rows[i]; r++)); do
      splash_cloud_shape[i * splash_horizon + r]=${fields[4 + r]}
    done
  done
}

# Letters before <letter> landed and <letter> <rise> rows short, for wordmark row <row>.
splash_wordmark_mask() { # <row> <letter> <rise>
  local row=$1 letter=$2 rise=$3 i glyph_row
  splash_mask=''
  for i in "${!splash_glyphs[@]}"; do
    ((i > 0)) && splash_mask+='  '
    glyph_row=$row
    ((i == letter)) && glyph_row=$((row + rise))
    if ((i > letter || glyph_row > 4)); then
      splash_mask+='      '
    else
      splash_mask+=${splash_glyph_rows[i * 5 + glyph_row]}
    fi
  done
}

# Sun, then clouds, then letters; redrawn only when a cloud or letter has moved.
splash_sky() { # <frame> <letter> <rise>
  local frame=$1 letter=$2 rise=$3 i row shape key=''
  local -a at=()
  for i in "${!splash_cloud_top[@]}"; do
    at[i]=$((splash_cloud_x[i] + splash_cloud_step[i] * (frame / splash_cloud_period[i])))
    key+="${at[i]},"
  done
  key+="$letter.$rise"
  [[ "$key" == "${splash_sky_key:-}" ]] && return
  splash_sky_key=$key
  for ((row = 0; row < splash_horizon; row++)); do
    splash_text=${splash_sun_text[row]} splash_kinds=${splash_sun_kinds[row]}
    for i in "${!splash_cloud_top[@]}"; do
      shape=$((row - splash_cloud_top[i]))
      ((shape >= 0 && shape < splash_cloud_rows[i])) || continue
      splash_lay "${at[i]}" "${splash_cloud_shape[i * splash_horizon + shape]}" c
    done
    if ((row >= splash_wordmark_top && row < splash_wordmark_top + 5)); then
      splash_wordmark_mask $((row - splash_wordmark_top)) "$letter" "$rise"
      splash_lay "$splash_wordmark_x" "$splash_mask" $((2 + row - splash_wordmark_top))
    fi
    splash_paint_row "$row" "$splash_text" "$splash_kinds"
  done
}

splash_horizon_line() {
  local horizon
  splash_fill horizon '-'
  splash_paint_row "$splash_horizon" "$horizon" "${horizon//-/h}"
}

# Sets splash_sprite to the rows of a sheep of <size> facing <facing>.
splash_sheep_sprite() { # <size> <facing r|l> <head down 0|1>
  case "$1$2$3" in
    1*) splash_sprite=('@') ;;
    2r0) splash_sprite=('@o') ;;
    2r1) splash_sprite=('@.') ;;
    2l0) splash_sprite=('o@') ;;
    2l1) splash_sprite=('.@') ;;
    5r0) splash_sprite=('(@@)o') ;;
    5r1) splash_sprite=('(@@).') ;;
    5l0) splash_sprite=('o(@@)') ;;
    5l1) splash_sprite=('.(@@)') ;;
    9r0) splash_sprite=('  .--.   ' ' (@@@@)o ' '  "  "   ') ;;
    9r1) splash_sprite=('  .--.   ' ' (@@@@)  ' '  "  "o  ') ;;
    9l0) splash_sprite=('   .--.  ' ' o(@@@@) ' '   "  "  ') ;;
    9l1) splash_sprite=('   .--.  ' '  (@@@@) ' '  o"  "  ') ;;
  esac
}

# Lays splash_sprite over the ground rows from <row>.
splash_overlay() { # <row> <x> <kind>
  local row=$1 x=$2 kind=$3 r d
  for r in "${!splash_sprite[@]}"; do
    d=$((row + r - splash_ground_top))
    splash_text=${splash_rows_text[d]} splash_kinds=${splash_rows_kinds[d]}
    splash_lay "$x" "${splash_sprite[r]}" "$kind"
    splash_rows_text[d]=$splash_text splash_rows_kinds[d]=$splash_kinds
  done
}

# Further back is smaller, higher, slower and dimmer. Walkers drift left or
# right; grazers stay put and bob their heads.
splash_pasture() { # <frame>
  local frame=$1 entry d row kind size x motion period IFS=' '
  splash_rows_text=("${splash_ground_text[@]}")
  splash_rows_kinds=("${splash_ground_kinds_row[@]}")
  for entry in "${splash_herd[@]}"; do
    # Unquoted to split the entry's six fields.
    set -- $entry
    row=$1 kind=$2 size=$3 x=$4 motion=$5 period=$6
    case $motion in
      r) splash_sheep_sprite "$size" r 0; x=$((x + frame / period)) ;;
      l) splash_sheep_sprite "$size" l 0; x=$((x - frame / period)) ;;
      g?) splash_sheep_sprite "$size" "${motion:1}" $(((frame / period + x) % 2)) ;;
    esac
    splash_overlay "$row" "$x" "$kind"
  done
  local face='(o)'
  ((frame % 40 >= 37)) && face='(-)'
  splash_sprite=(' .-.~~?' " $face~~|" '/|=|\_|' ' |=|~~|' ' |~|~~|' ' ^~^~~|')
  splash_overlay "$splash_shepherd_row" "$splash_shepherd_x" s
  for ((d = 0; d < splash_ground_rows; d++)); do
    [[ "${splash_rows_text[d]}${splash_rows_kinds[d]}" == "${splash_painted[d]:-}" ]] && continue
    splash_painted[d]=${splash_rows_text[d]}${splash_rows_kinds[d]}
    splash_paint_row $((splash_ground_top + d)) "${splash_rows_text[d]}" "${splash_rows_kinds[d]}"
  done
}

splash_show_tagline() {
  local pad=$(((splash_width - ${#splash_tagline}) / 2)) text kinds
  printf -v text '%*s%s%*s' "$pad" '' "$splash_tagline" "$((splash_width - pad - ${#splash_tagline}))" ''
  kinds=${text//[! ]/t}
  splash_paint_row "$splash_tagline_row" "$text" "${kinds// /0}"
}

# Sleeps until <frame> is due, or a plain frame's length where the clock is missing.
splash_wait() { # <start_us> <frame>
  local now wait
  now=${EPOCHREALTIME:-}
  now=${now//[!0-9]/}
  if [[ -z "$1" || -z "$now" ]]; then
    sleep 0.05 || :
    return
  fi
  wait=$(($1 + ($2 + 1) * splash_frame_us - now))
  ((wait > 0)) || return 0
  printf -v wait '%d.%06d' $((wait / 1000000)) $((wait % 1000000))
  sleep "$wait" || :
}

splash_prepare() {
  local LC_ALL=C
  splash_parse
  splash_draw_sun
  splash_draw_ground
  splash_sky_key=
  splash_painted=()
}

# Sets splash_buf to what changed on screen for <frame>.
# The scene is ASCII until painted; the C locale makes slicing it about twice as fast.
splash_frame() { # <frame>
  local frame=$1 letters=${#splash_glyphs[@]} letter rise LC_ALL=C
  letter=$((frame / splash_drop))
  rise=$((splash_drop - 1 - frame % splash_drop))
  ((letter < letters)) || { letter=$letters; rise=0; }
  splash_buf=''
  if ((frame == 0)); then splash_horizon_line; fi
  splash_sky "$frame" "$letter" "$rise"
  if ((frame == letters * splash_drop)); then splash_show_tagline; fi
  splash_pasture "$frame"
}

splash_full() {
  local frame start
  splash_prepare
  start=${EPOCHREALTIME:-}
  start=${start//[!0-9]/}
  splash_alt_screen=1
  printf '\e[?1049h\e[2J' || :
  for ((frame = 0; frame < splash_frames && !splash_interrupted; frame++)); do
    splash_frame "$frame"
    printf '%s' "$splash_buf" || :
    splash_wait "$start" "$frame"
  done
}

# Narrow or short terminals get the name typed out on one line instead.
splash_title() {
  local word=loopherd i code sgr
  printf '  ' || :
  for ((i = 0; i < ${#word} && !splash_interrupted; i++)); do
    printf -v code '%d' "'$((2 + i * 4 / 7))"
    sgr=${splash_sgr[code]:-}
    if [[ -n "$sgr" ]]; then
      printf '\e[%sm%s\e[0m' "$sgr" "${word:i:1}" || :
    else
      printf '%s' "${word:i:1}" || :
    fi
    sleep 0.1 || :
  done
  printf '\n\n' || :
}

splash_restore() {
  if [[ -z "${NO_COLOR:-}" ]]; then printf '\e[0m' || :; fi
  printf '\e[?25h' || :
  if [[ -n "${splash_alt_screen:-}" ]]; then printf '\e[?1049l' || :; fi
  splash_alt_screen=
}

# Returns 130 when interrupted, leaving the caller's own traps in place.
splash_play() {
  local size rows= cols= callers_traps status=0
  # stty reads stdin, which may not be the terminal; fd 3 is stdout from outside the $(...).
  { size=$(stty size <&3 2>/dev/null); } 3>&1 || return 0
  IFS=' ' read -r rows cols <<<"$size"
  [[ "$rows" =~ ^[1-9][0-9]*$ && "$cols" =~ ^[1-9][0-9]*$ ]] || return 0
  splash_colours
  splash_alt_screen=
  splash_interrupted=0
  callers_traps=$(trap -p INT EXIT)
  trap 'splash_interrupted=1' INT
  trap splash_restore EXIT
  printf '\e[?25l' || :
  splash_layout "$rows" "$cols"
  if [[ "$splash_layout" == title ]]; then
    splash_title
  else
    splash_full
  fi
  splash_restore
  trap - INT EXIT
  eval "$callers_traps"
  ((splash_interrupted)) && status=130
  return "$status"
}
