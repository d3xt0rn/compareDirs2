#!/usr/bin/env bash
#
# compare_dirs.sh
#
# Compare all files in DIR1 with DIR2 recursively.
# Styled after OpenRC.
#
set -uo pipefail

#########################
# Colors (OpenRC style) #
#########################
RESET='\033[0m'
GREEN='\033[1;32m'
RED='\033[1;31m'
BLUE='\033[1;34m'
YELLOW='\033[1;33m' # used as "orange"
CYAN='\033[1;36m'
WHITE='\033[1;37m'

SPINNER=("/" "-" "\\" "|")

############################
# Defaults / options        #
############################
HASH_MODE=false
HASH_ALGO="sha256sum"
MIN_SIZE=0
MAX_SIZE=0
MAX_FIND_TIME=0
FIND_RENAMED=false
DIR1=""
DIR2=""

ALLOWED_ALGOS=("md5sum" "sha1sum" "sha256sum" "sha512sum" "b2sum")

usage() {
  cat <<EOF
Usage: $0 [OPTIONS] DIR1 DIR2

Options:
  -x, --hash           Compare files by hash instead of byte-for-byte cmp
  -a, --algo ALGO      Hash algorithm: md5sum|sha1sum|sha256sum|sha512sum|b2sum
                       (default: sha256sum)
      --min-size SIZE  Only hash-compare files >= SIZE (e.g., 500B, 2KB, 10MiB, 1GB)
                       (smaller files use cmp)
      --max-size SIZE  Only hash-compare files <= SIZE (e.g., 100MB, 2GiB)
                       (larger files use cmp)
      --max-find-time T Max time allowed for find search per file (e.g., 5s, 2m, 1h)
  -r, --find-renamed   If a file is missing in DIR2 — search DIR2 for a
                       file with identical content/hash (rename detection)
  -h, --help           Show this help
EOF
}

############################
# Parsers (Size & Time)    #
############################

# Converts human-readable size to bytes
parse_size() {
  local val="$1"
  # Convert to lowercase
  local clean=$(echo "$val" | tr '[:upper:]' '[:lower:]' | xargs)

  # Check for plain number
  if [[ "$clean" =~ ^[0-9]+$ ]]; then
    echo "$clean"
    return 0
  fi

  # Regex for number/suffix
  if [[ "$clean" =~ ^([0-9]+)([a-z]+)$ ]]; then
    local num="${BASH_REMATCH[1]}"
    local unit="${BASH_REMATCH[2]}"
    local factor=1

    case "$unit" in
    b) factor=1 ;;
    k | kb) factor=1000 ;;
    m | mb) factor=1000000 ;;
    g | gb) factor=1000000000 ;;
    t | tb) factor=100000000000 ;;
    p | pb) factor=100000000000000 ;;
    ki | kib) factor=1024 ;;
    mi | mib) factor=1048576 ;;
    gi | gib) factor=1073741824 ;;
    ti | tib) factor=1099511627776 ;;
    pi | pib) factor=1125899906842624 ;;
    *)
      echo "Error: Unknown size suffix '$unit' in argument '$val'" >&2
      exit 1
      ;;
    esac
    echo $((num * factor))
  else
    echo "Error: Invalid size format '$val'" >&2
    exit 1
  fi
}

# Converts human-readable time to seconds
parse_time() {
  local val="$1"
  local clean=$(echo "$val" | tr '[:upper:]' '[:lower:]' | xargs)

  if [[ "$clean" =~ ^[0-9]+$ ]]; then
    echo "$clean"
    return 0
  fi

  if [[ "$clean" =~ ^([0-9]+)([a-z]+)$ ]]; then
    local num="${BASH_REMATCH[1]}"
    local unit="${BASH_REMATCH[2]}"
    local factor=1

    case "$unit" in
    s) factor=1 ;;
    m) factor=60 ;;
    h) factor=3600 ;;
    d) factor=86400 ;;
    w) factor=604800 ;;
    *)
      echo "Error: Unknown time suffix '$unit' in argument '$val'" >&2
      exit 1
      ;;
    esac
    echo $((num * factor))
  else
    echo "Error: Invalid time format '$val'" >&2
    exit 1
  fi
}

############################
# Parse arguments          #
############################
POSITIONAL=()
while [[ $# -gt 0 ]]; do
  case "$1" in
  -h | --help)
    usage
    exit 0
    ;;
  -x | --hash)
    HASH_MODE=true
    shift
    ;;
  -a | --algo)
    HASH_ALGO="$2"
    shift 2
    ;;
  --min-size)
    MIN_SIZE=$(parse_size "$2")
    shift 2
    ;;
  --max-size)
    MAX_SIZE=$(parse_size "$2")
    shift 2
    ;;
  --max-find-time)
    MAX_FIND_TIME=$(parse_time "$2")
    shift 2
    ;;
  -r | --find-renamed)
    FIND_RENAMED=true
    shift
    ;;
  --)
    shift
    POSITIONAL+=("$@")
    break
    ;;
  -*)
    echo "Unknown option: $1" >&2
    usage
    exit 1
    ;;
  *)
    POSITIONAL+=("$1")
    shift
    ;;
  esac
done
set -- "${POSITIONAL[@]:-}"

if [[ $# -ne 2 ]]; then
  usage
  exit 1
fi

DIR1="${1%/}"
DIR2="${2%/}"

if [[ ! -d "$DIR1" ]]; then
  echo "Directory not found: $DIR1" >&2
  exit 1
fi
if [[ ! -d "$DIR2" ]]; then
  echo "Directory not found: $DIR2" >&2
  exit 1
fi

if $HASH_MODE; then
  valid=false
  for a in "${ALLOWED_ALGOS[@]}"; do
    [[ "$HASH_ALGO" == "$a" ]] && valid=true
  done
  if ! $valid; then
    echo "Unsupported hash algorithm: $HASH_ALGO" >&2
    echo "Available: ${ALLOWED_ALGOS[*]}" >&2
    exit 1
  fi
  if ! command -v "$HASH_ALGO" >/dev/null 2>&1; then
    echo "Utility $HASH_ALGO not found on this system." >&2
    exit 1
  fi
fi

############################
# Terminal / Status column #
############################
TERM_WIDTH=$(tput cols 2>/dev/null || echo 80)
trap 'TERM_WIDTH=$(tput cols 2>/dev/null || echo 80)' WINCH

OK_COUNT=0
ERR_COUNT=0
UNCHECKED_COUNT=0
SPID=""
INDEX=0
TOTAL=0
FILES=()
LAST_LINES=1

filesize() {
  stat -c%s "$1" 2>/dev/null || stat -f%z "$1" 2>/dev/null || echo 0
}

clear_status() {
  if ((LAST_LINES > 1)); then
    printf '\033[%dA' "$((LAST_LINES - 1))"
  fi
  printf '\r\033[J'
}

finish_line() {
  echo
  LAST_LINES=1
}

status_line() {
  local prefix="$1" pcolor="$2" msg="$3" status="$4" scolor="$5"
  local status_len=${#status}
  local fixed=$((status_len + 6))
  local msglen=${#msg}

  clear_status

  if ((TERM_WIDTH >= msglen + fixed + 2)); then
    local pad=$((TERM_WIDTH - msglen - fixed))
    ((pad < 1)) && pad=1
    printf "%b%s%b %s%*s%b[%b %b%s%b %b]%b" \
      "$pcolor" "$prefix" "$RESET" \
      "$msg" "$pad" "" \
      "$BLUE" "$RESET" \
      "$scolor" "$status" "$RESET" \
      "$BLUE" "$RESET"
    LAST_LINES=1
  elif ((TERM_WIDTH >= msglen + 2)); then
    printf "%b%s%b %s\n" "$pcolor" "$prefix" "$RESET" "$msg"
    printf "    %b[%b %b%s%b %b]%b" \
      "$BLUE" "$RESET" \
      "$scolor" "$status" "$RESET" \
      "$BLUE" "$RESET"
    LAST_LINES=2
  else
    local maxmsg=$((TERM_WIDTH - fixed - 2))
    ((maxmsg < 5)) && maxmsg=5
    if ((msglen > maxmsg)); then
      msg="...${msg: -$((maxmsg - 3))}"
      msglen=${#msg}
    fi
    local pad=$((TERM_WIDTH - msglen - fixed))
    ((pad < 1)) && pad=1
    printf "%b%s%b %s%*s%b[%b %b%s%b %b]%b" \
      "$pcolor" "$prefix" "$RESET" \
      "$msg" "$pad" "" \
      "$BLUE" "$RESET" \
      "$scolor" "$status" "$RESET" \
      "$BLUE" "$RESET"
    LAST_LINES=1
  fi
}

# Spinner supports custom status mode
spinner() {
  local msg="$1" color="$2" mode="${3:-default}"
  while :; do
    for c in "${SPINNER[@]}"; do
      if [[ "$mode" == "finding" ]]; then
        status_line " " "$RESET" "$msg" "finding $c" "$color"
      else
        status_line " " "$RESET" "$msg" "$c" "$color"
      fi
      sleep 0.08
    done
  done
}

stop_spinner() {
  if [[ -n "$SPID" ]]; then
    kill "$SPID" 2>/dev/null || true
    wait "$SPID" 2>/dev/null || true
    SPID=""
  fi
}

############################
# Ctrl+C / Ctrl+Z handling #
############################
INTERRUPTED=false
on_interrupt() {
  if $INTERRUPTED; then
    stop_spinner
    printf "\n%b*%b Force exit.\n" "$RED" "$RESET"
    exit 130
  fi

  INTERRUPTED=true
  stop_spinner

  UNCHECKED_COUNT=$((TOTAL - INDEX))

  clear_status
  status_line "*" "$YELLOW" "Verification interrupted" "${UNCHECKED_COUNT}/${TOTAL} unchecked" "$YELLOW"
  finish_line

  printf "\n%b*%b Interrupted by user.\n" "$RED" "$RESET"
  printf "%b*%b OK: %d  Errors: %d  Unchecked: %d\n" \
    "$BLUE" "$RESET" \
    "$OK_COUNT" "$ERR_COUNT" "$UNCHECKED_COUNT"

  exit 130
}

trap on_interrupt SIGINT SIGTSTP

############################
# Compare logic            #
############################
hash_of() {
  "$HASH_ALGO" "$1" 2>/dev/null | awk '{print $1}'
}

compare_files() {
  local f1="$1" f2="$2"
  if $HASH_MODE; then
    local size use_hash=true
    size=$(filesize "$f1")
    if ((MIN_SIZE > 0 && size < MIN_SIZE)); then use_hash=false; fi
    if ((MAX_SIZE > 0 && size > MAX_SIZE)); then use_hash=false; fi
    if $use_hash; then
      [[ "$(hash_of "$f1")" == "$(hash_of "$f2")" ]]
      return $?
    fi
  fi
  cmp -s "$f1" "$f2"
}

# find_renamed FILE1 -> search duplicate with timeout
find_renamed() {
  local f1="$1" cand target_sum=""
  local start_time=$(date +%s)

  $HASH_MODE && target_sum="$(hash_of "$f1")"

  while IFS= read -r -d '' cand; do
    # Search timeout check
    if ((MAX_FIND_TIME > 0)); then
      local current_time=$(date +%s)
      if ((current_time - start_time >= MAX_FIND_TIME)); then
        return 1
      fi
    fi

    if $HASH_MODE; then
      [[ "$(hash_of "$cand")" == "$target_sum" ]] && {
        printf '%s' "$cand"
        return 0
      }
    else
      cmp -s "$f1" "$cand" && {
        printf '%s' "$cand"
        return 0
      }
    fi
  done < <(find "$DIR2" -type f -print0)
  return 1
}

############################
# Collect files            #
############################
mapfile -d '' FILES < <(find "$DIR1" -type f -print0)
TOTAL=${#FILES[@]}

############################
# Main loop                #
############################
for ((INDEX = 0; INDEX < TOTAL; INDEX++)); do
  FILE1="${FILES[$INDEX]}"
  REL="${FILE1#$DIR1/}"
  FILE2="$DIR2/$REL"

  #########################################
  # Missing file
  #########################################
  if [[ ! -f "$FILE2" ]]; then
    if $FIND_RENAMED; then
      # Start spinner with "finding" to display [ finding / ]
      spinner "Find $REL" "$CYAN" "finding" &
      SPID=$!
      FOUND="$(find_renamed "$FILE1" || true)"
      stop_spinner
      if [[ -n "$FOUND" ]]; then
        status_line "*" "$GREEN" "Find $REL" "found" "$GREEN"
        finish_line
        printf "%b*%b Looks like the file was renamed/moved:\n" "$GREEN" "$RESET"
        printf "    Found as: %s\n\n" "$FOUND"
        OK_COUNT=$((OK_COUNT + 1))
        continue
      fi
    fi
    status_line "*" "$RED" "Check $REL" "!!" "$RED"
    finish_line
    printf "%b*%b File not found in destination:\n" "$RED" "$RESET"
    printf "    %s\n\n" "$FILE2"
    ERR_COUNT=$((ERR_COUNT + 1))
    continue
  fi

  #########################################
  # Spinner + compare
  #########################################
  spinner "Checking $REL" "$WHITE" &
  SPID=$!
  if compare_files "$FILE1" "$FILE2"; then
    stop_spinner
    status_line "*" "$GREEN" "Check $REL" "ok" "$GREEN"
    finish_line
    OK_COUNT=$((OK_COUNT + 1))
  else
    stop_spinner
    status_line "*" "$RED" "Check $REL" "!!" "$RED"
    finish_line
    printf "%b*%b File contents differ.\n" "$RED" "$RESET"
    printf "%b*%b The file may be corrupted or modified.\n" "$RED" "$RESET"
    printf "    Source      : %s\n" "$FILE1"
    printf "    Destination : %s\n\n" "$FILE2"
    ERR_COUNT=$((ERR_COUNT + 1))
  fi
done

printf "\n%b*%b Verification finished.\n" "$GREEN" "$RESET"
printf "%b*%b OK: %d  Errors: %d\n" "$BLUE" "$RESET" "$OK_COUNT" "$ERR_COUNT"
