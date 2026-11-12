#!/bin/bash
# Walks the tester through docs/beta/manual-checks.md one step at a time and records the run
# (#213). The doc is the only copy of the steps: they are parsed from it on every run.
#
#   bash tests/macos/manual-checks.sh            # run, or continue a run left with q
#   bash tests/macos/manual-checks.sh --restart  # forget this Mac's answers for this doc
#   bash tests/macos/manual-checks.sh --list     # the sections and their step counts
#   bash tests/macos/manual-checks.sh --check    # fail if the doc no longer parses (CI)
#
# A section is "## N. Title"; a step is a top-level "N. " item with the lines under it. A
# section without items is one step. Answers are saved after each one, per Mac and per revision
# of the doc, under $XDG_STATE_HOME or ~/Library/Application Support. The end prints the
# Results row to paste; the doc is never edited.
#
# It runs only commands the step names from a fixed read-only list (pgrep, `pinfold --version`,
# `--help` and `ctl list`, uptime), and only on r; mem-gate needs a further y. Nothing it runs
# kills, edits, logs out, uninstalls or ejects: those are the tester's to do.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
DOC=${MANUAL_CHECKS_DOC:-$(cd "$HERE/../.." && pwd)/docs/beta/manual-checks.md}
MODE=run
case ${1:-} in
  '') ;;
  --list | --check | --restart) MODE=${1#--} ;;
  *) echo "usage: $0 [--list | --check | --restart]" >&2; exit 2 ;;
esac
[ -r "$DOC" ] || { echo "cannot read $DOC" >&2; exit 2; }

# One record per step: section, title, item (0 for a whole section), section intro, step text.
parse() {
  awk '
    function flush(   j) {
      if (sec == "") return
      if (n == 0) emit(0, "", intro)
      for (j = 1; j <= n; j++) emit(num[j], intro, body[j])
      sec = ""; n = 0; intro = ""
    }
    function trim(s) { sub(/^\n+/, "", s); sub(/\n+$/, "", s); return s }
    function emit(i, pre, text) {
      printf "%s\037%s\037%s\037%s\037%s\036", sec, title, i, trim(pre), trim(text)
    }
    /^[ \t]*```/ { fence = !fence }
    !fence && /^## / {
      flush()
      if ($0 ~ /^## [0-9]+\. /) { title = substr($0, 4); sec = title + 0; sub(/^[0-9]+\. /, "", title) }
      next
    }
    sec == "" { next }
    !fence && /^[0-9]+\. / { n++; num[n] = $0 + 0; body[n] = $0; next }
    n > 0 { body[n] = body[n] "\n" $0; next }
    { intro = intro "\n" $0 }
    END { flush() }
  ' "$DOC"
}

SEC=() TITLE=() ITEM=() INTRO=() BODY=() ID=()
while IFS=$'\037' read -r -d $'\036' s t n i b; do
  SEC+=("$s") TITLE+=("$t") ITEM+=("$n") INTRO+=("$i") BODY+=("$b")
  if [ "$n" = 0 ]; then ID+=("$s"); else ID+=("$s.$n"); fi
done < <(parse)
TOTAL=${#ID[@]}

if [ "$MODE" = check ]; then
  bad=0 prev_sec=0 prev_step=0
  [ "$TOTAL" -gt 0 ] || { echo "no \"## N. Title\" sections in $DOC"; bad=1; }
  for ((k = 0; k < TOTAL; k++)); do
    s=${SEC[k]} n=${ITEM[k]}
    if [ "$s" != "$prev_sec" ]; then
      [ "$s" -eq $((prev_sec + 1)) ] || { echo "section $s follows section $prev_sec"; bad=1; }
      prev_step=0
    fi
    [ "$n" = 0 ] || [ "$n" -eq $((prev_step + 1)) ] || { echo "step $s.$n follows step $s.$prev_step"; bad=1; }
    [ -n "${BODY[k]}" ] || { echo "section $s has no steps"; bad=1; }
    prev_sec=$s prev_step=$n
  done
  [ $bad = 0 ] && echo "ok: $prev_sec sections, $TOTAL steps in $DOC"
  exit $bad
fi

if [ "$MODE" = list ]; then
  for ((k = 0; k < TOTAL; k++)); do
    [ $k -gt 0 ] && [ "${SEC[k]}" = "${SEC[k - 1]}" ] && continue
    c=0
    for ((j = k; j < TOTAL; j++)); do [ "${SEC[j]}" = "${SEC[k]}" ] && c=$((c + 1)); done
    printf '%3s. %-40s %d\n' "${SEC[k]}" "${TITLE[k]}" $c
  done
  echo "$TOTAL steps"
  exit 0
fi

TODAY=$(date +%F)
if v=$(sw_vers -productVersion 2>/dev/null); then MACOS="$v ($(sw_vers -buildVersion))"; else MACOS=unknown; fi
CHIP="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown) ($(uname -m))"
if git -C "$(dirname "$DOC")" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  REV=$(git hash-object "$DOC" | cut -c1-12)
else
  REV=$(shasum -a 256 "$DOC" | cut -c1-12)
fi
MACHINE=$(printf %s "$(hostname -s)-$(uname -m)" | tr -c 'A-Za-z0-9._-' _)
DIR=${XDG_STATE_HOME:+$XDG_STATE_HOME/pinfold-manual-checks}
DIR=${DIR:-$HOME/Library/Application Support/pinfold-manual-checks}
STATE="$DIR/$MACHINE-$REV.state"
mkdir -p "$DIR" || exit 1
[ "$MODE" = restart ] && rm -f "$STATE"

RES=() NOTE=() ISSUE=()
for ((k = 0; k < TOTAL; k++)); do RES[k]="" NOTE[k]="" ISSUE[k]=""; done
if [ -f "$STATE" ]; then
  while IFS=$'\037' read -r id r note issue; do
    for ((k = 0; k < TOTAL; k++)); do
      [ "${ID[k]}" = "$id" ] && RES[k]=$r NOTE[k]=$note ISSUE[k]=$issue
    done
  done <"$STATE"
fi

save() {
  { for ((j = 0; j < TOTAL; j++)); do
      [ -z "${RES[j]}" ] || printf '%s\037%s\037%s\037%s\n' "${ID[j]}" "${RES[j]}" "${NOTE[j]}" "${ISSUE[j]}"
    done; } >"$STATE.tmp" && mv "$STATE.tmp" "$STATE"
}

# Sets REPLY, without control characters; fails at the end of input. Echoes piped answers.
ask() {
  printf '%s' "$1"
  IFS= read -r REPLY || { echo; return 1; }
  REPLY=$(printf %s "$REPLY" | tr -d '\000-\037')
  [ -t 0 ] || printf '%s\n' "$REPLY"
}

stop() {
  echo "Saved in $STATE. Run again to go on from here."
  exit 0
}

# The commands step $1 names that only read: backticked spans on the list, and mem-gate's line.
runnable() {
  # shellcheck disable=SC2016 # the backticks are markdown, not command substitution
  { printf '%s\n' "${BODY[$1]}" | grep -o '`[^`]*`' | tr -d '`' |
      grep -E '^(pgrep -[a-z]+ [A-Za-z0-9_.-]+|pinfold (--version|--help|ctl list)|uptime)$'
    printf '%s\n' "${BODY[$1]}" | grep -E '^[^`]*cargo xtask mem-gate$'
  } | awk '!seen[$0]++'
}

run() {
  while IFS= read -r c <&3; do
    case $c in
      *mem-gate)
        root=$(git -C "$HERE" rev-parse --show-toplevel 2>/dev/null) ||
          { echo "not in a checkout, so not running: $c"; continue; }
        echo "This builds and measures in $root for some minutes."
        ask "Run \`$c\` there? [y/N] " || stop
        [ "$REPLY" = y ] || continue
        printf '$ %s\n' "$c"
        (cd "$root" && bash -c "$c")
        ;;
      *)
        printf '$ %s\n' "$c"
        $c 2>&1
        ;;
    esac
    echo "(exit $?)"
  done 3<<<"$1"
}

K=0
while [ $K -lt "$TOTAL" ] && [ -n "${RES[K]}" ]; do K=$((K + 1)); done
echo "Date:  $TODAY"
echo "macOS: $MACOS"
echo "Chip:  $CHIP"
echo "Doc:   $DOC ($REV)"
echo "State: $STATE"
[ $K -gt 0 ] && echo "Going on from step ${ID[K]:-the end}; b goes back."

while [ $K -lt "$TOTAL" ]; do
  printf '\n== %s. %s  [step %s, %d of %d]\n' "${SEC[K]}" "${TITLE[K]}" "${ID[K]}" $((K + 1)) "$TOTAL"
  [ -n "${INTRO[K]}" ] && printf '%s\n\n' "${INTRO[K]}"
  printf '%s\n' "${BODY[K]}"
  [ -n "${RES[K]}" ] && echo "(answered: ${RES[K]}${NOTE[K]:+, ${NOTE[K]}})"
  CMDS=$(runnable $K)
  [ -n "$CMDS" ] && printf 'r runs: %s\n' "$(printf '%s' "$CMDS" | tr '\n' ';' | sed 's/;/; /g')"
  ask "p pass, f fail, s skip, b back, q quit${CMDS:+, r run}: " || stop
  case $REPLY in
    p | s)
      RES[K]=$([ "$REPLY" = p ] && echo pass || echo skip) NOTE[K]="" ISSUE[K]=""
      save; K=$((K + 1)) ;;
    f)
      ask "What failed, in one line: " || stop
      note=$REPLY
      ask "Issue number or URL (Enter if there is none yet): " || stop
      RES[K]=fail NOTE[K]=$note ISSUE[K]=$REPLY
      save; K=$((K + 1)) ;;
    b) [ $K -gt 0 ] && K=$((K - 1)) ;;
    q) stop ;;
    r) [ -n "$CMDS" ] && run "$CMDS" ;;
    *) echo "Answer p, f, s, b or q." ;;
  esac
done

# "#N" for an issue number or a GitHub issue or PR URL, else the text as given.
ref() {
  case $1 in
    '') ;;
    */issues/[0-9]* | */pull/[0-9]*) echo "#${1##*/}" ;;
    *[!0-9#]*) echo "$1" ;;
    *) echo "#${1#\#}" ;;
  esac
}

PASSED=0 FAILED="" NEEDS="" NOTES="" SKIPPED=""
for ((k = 0; k < TOTAL; k++)); do
  case ${RES[k]} in
    pass) PASSED=$((PASSED + 1)) ;;
    skip) SKIPPED="$SKIPPED, ${ID[k]}" ;;
    fail)
      r=$(ref "${ISSUE[k]}")
      [ -n "$r" ] || { r="(needs an issue)"; NEEDS="$NEEDS ${ID[k]}"; }
      FAILED="$FAILED, ${ID[k]} $r"
      NOTES="$NOTES; ${ID[k]}: ${NOTE[k]}" ;;
  esac
done
FAILED=${FAILED#, } NOTES=${NOTES#; } SKIPPED=${SKIPPED#, }
[ -n "$SKIPPED" ] && NOTES="${NOTES:+$NOTES; }skipped $SKIPPED"

DEFAULT=$(git config user.name 2>/dev/null)
ask "Tester [$DEFAULT]: " || REPLY=""
TESTER=${REPLY:-$DEFAULT}

cell() { printf '%s' "${1//|/\\|}"; }
ROW="| $TODAY | $(cell "$TESTER") | $MACOS | $CHIP | $PASSED/$TOTAL | $(cell "${FAILED:-none}") | $(cell "$NOTES") |"
REPORT="$DIR/report-$MACHINE-$REV-$TODAY.md"
{
  echo "# Manual checks, $TODAY"
  echo
  echo "Tester: $TESTER. macOS: $MACOS. Chip: $CHIP. Doc: $DOC at $REV."
  echo
  echo "| Date | Tester | macOS | Chip | Steps passed | Failed steps | Notes |"
  echo "| ---- | ------ | ----- | ---- | ------------ | ------------ | ----- |"
  echo "$ROW"
  echo
  echo "| Step | Result | Note | Issue |"
  echo "| ---- | ------ | ---- | ----- |"
  for ((k = 0; k < TOTAL; k++)); do
    echo "| ${ID[k]} | ${RES[k]} | $(cell "${NOTE[k]}") | $(cell "${ISSUE[k]}") |"
  done
} >"$REPORT"

echo
echo "Paste this row into the Results table of $DOC:"
echo
echo "$ROW"
echo
[ -n "$NEEDS" ] && echo "Needs an issue, as the doc asks one for every failure:$NEEDS"
echo "Report: $REPORT"
echo "The answers stay in $STATE; --restart starts a new run."
