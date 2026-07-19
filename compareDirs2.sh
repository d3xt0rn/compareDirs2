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
YELLOW='\033[1;33m' # используется как "orange"
CYAN='\033[1;36m'
WHITE='\033[1;37m'

SPINNER=("/" "-" "\\" "|")

############################
# Defaults / options       #
############################
HASH_MODE=false
HASH_ALGO="sha256sum"
MIN_SIZE=0
MAX_SIZE=0
FIND_RENAMED=false
DIR1=""
DIR2=""

ALLOWED_ALGOS=("md5sum" "sha1sum" "sha256sum" "sha512sum" "b2sum")

usage() {
  cat <<EOF
Usage: $0 [OPTIONS] DIR1 DIR2

Options:
  -x, --hash              Сравнивать файлы по хешу вместо побайтового cmp
  -a, --algo ALGO         Алгоритм хеша: md5sum|sha1sum|sha256sum|sha512sum|b2sum
                          (по умолчанию: sha256sum)
      --min-size BYTES    Хешем сравнивать только файлы >= BYTES
                          (файлы меньше — через cmp)
      --max-size BYTES    Хешем сравнивать только файлы <= BYTES
                          (файлы больше — через cmp)
  -r, --find-renamed      Если файл отсутствует в DIR2 — искать в DIR2
                          файл с идентичным содержимым/хешем (детект переименований)
  -h, --help              Показать эту справку
EOF
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
    MIN_SIZE="$2"
    shift 2
    ;;
  --max-size)
    MAX_SIZE="$2"
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
    echo "Неизвестная опция: $1" >&2
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
  echo "Директория не найдена: $DIR1" >&2
  exit 1
fi
if [[ ! -d "$DIR2" ]]; then
  echo "Директория не найдена: $DIR2" >&2
  exit 1
fi

if $HASH_MODE; then
  valid=false
  for a in "${ALLOWED_ALGOS[@]}"; do
    [[ "$HASH_ALGO" == "$a" ]] && valid=true
  done
  if ! $valid; then
    echo "Неподдерживаемый алгоритм хеша: $HASH_ALGO" >&2
    echo "Доступные: ${ALLOWED_ALGOS[*]}" >&2
    exit 1
  fi
  if ! command -v "$HASH_ALGO" >/dev/null 2>&1; then
    echo "Утилита $HASH_ALGO не найдена в системе." >&2
    exit 1
  fi
fi

############################
# Terminal / Status column #
############################
TERM_WIDTH=$(tput cols 2>/dev/null || echo 80)
# ширина обновляется при ресайзе терминала, без лишних форков tput на каждый кадр
trap 'TERM_WIDTH=$(tput cols 2>/dev/null || echo 80)' WINCH

OK_COUNT=0
ERR_COUNT=0
UNCHECKED_COUNT=0
SPID=""
INDEX=0
TOTAL=0
FILES=()
LAST_LINES=1 # сколько строк занимает текущий незавершённый статус (1 или 2)

filesize() {
  stat -c%s "$1" 2>/dev/null || stat -f%z "$1" 2>/dev/null || echo 0
}

# стирает предыдущий вывод статуса (учитывая перенос на 2 строки)
clear_status() {
  if ((LAST_LINES > 1)); then
    printf '\033[%dA' "$((LAST_LINES - 1))"
  fi
  printf '\r\033[J'
}

# закрывает текущий статус реальным переводом строки и сбрасывает состояние
finish_line() {
  echo
  LAST_LINES=1
}

# status_line PREFIX PREFIX_COLOR MESSAGE STATUS STATUS_COLOR
#
# В зависимости от ширины терминала:
#   - если помещается целиком  -> одна строка, как обычно
#   - если само имя помещается, а статус справа - нет -> статус переносится
#     на следующую строку с отступом
#   - если не помещается даже имя -> имя обрезается спереди ("...хвост")
status_line() {
  local prefix="$1" pcolor="$2" msg="$3" status="$4" scolor="$5"
  local status_len=${#status}
  local fixed=$((status_len + 6)) # "* " + "[ " + status + " ]"
  local msglen=${#msg}

  clear_status

  if ((TERM_WIDTH >= msglen + fixed + 2)); then
    # всё влезает в одну строку
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
    # имя целиком влезает, статус переносим на следующую строку с отступом
    printf "%b%s%b %s\n" "$pcolor" "$prefix" "$RESET" "$msg"
    printf "    %b[%b %b%s%b %b]%b" \
      "$BLUE" "$RESET" \
      "$scolor" "$status" "$RESET" \
      "$BLUE" "$RESET"
    LAST_LINES=2
  else
    # терминал совсем узкий — обрезаем имя спереди
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

# spinner MESSAGE COLOR
spinner() {
  local msg="$1" color="$2"
  while :; do
    for c in "${SPINNER[@]}"; do
      status_line " " "$RESET" "$msg" "$c" "$color"
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
on_interrupt() {
  stop_spinner
  local j rel
  for ((j = INDEX; j < TOTAL; j++)); do
    rel="${FILES[j]#$DIR1/}"
    status_line "*" "$YELLOW" "Check $rel" "unchecked" "$YELLOW"
    finish_line
    UNCHECKED_COUNT=$((UNCHECKED_COUNT + 1))
  done
  printf "\n%b*%b Прервано пользователем.\n" "$RED" "$RESET"
  printf "%b*%b OK: %d  Ошибок: %d  Непроверено: %d\n" \
    "$BLUE" "$RESET" "$OK_COUNT" "$ERR_COUNT" "$UNCHECKED_COUNT"
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

# find_renamed FILE1 -> печатает путь найденного файла в stdout, код возврата 0/1
find_renamed() {
  local f1="$1" cand target_sum=""
  $HASH_MODE && target_sum="$(hash_of "$f1")"
  while IFS= read -r -d '' cand; do
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
      spinner "Find $REL" "$CYAN" &
      SPID=$!
      FOUND="$(find_renamed "$FILE1" || true)"
      stop_spinner
      if [[ -n "$FOUND" ]]; then
        status_line "*" "$GREEN" "Find $REL" "found" "$GREEN"
        finish_line
        printf "%b*%b Похоже, файл был переименован/перемещён:\n" "$GREEN" "$RESET"
        printf "    Найден как: %s\n\n" "$FOUND"
        OK_COUNT=$((OK_COUNT + 1))
        continue
      fi
    fi
    status_line "*" "$RED" "Check $REL" "!!" "$RED"
    finish_line
    printf "%b*%b Файл не найден в назначении:\n" "$RED" "$RESET"
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
    printf "%b*%b Содержимое файлов отличается.\n" "$RED" "$RESET"
    printf "%b*%b Файл может быть повреждён или изменён.\n" "$RED" "$RESET"
    printf "    Источник    : %s\n" "$FILE1"
    printf "    Назначение  : %s\n\n" "$FILE2"
    ERR_COUNT=$((ERR_COUNT + 1))
  fi
done

printf "\n%b*%b Проверка завершена.\n" "$GREEN" "$RESET"
printf "%b*%b OK: %d  Ошибок: %d\n" "$BLUE" "$RESET" "$OK_COUNT" "$ERR_COUNT"
