#!/usr/bin/env bash
# Screenshots every screen x state x theme of the Part 2.0 mockups with headless Chrome.
# Usage: OUT=/some/dir bash scripts/shoot-mockups.sh [--long] [--min] [page ...]
#   page      only that page (for example "inicio"); default: every page except index
#   --long    adds long=1 (text 30% longer)       -> files end in -long
#   --min     1024x700 window instead of 1280x800  -> files end in -min
# PNGs: ${OUT:-/tmp/revenant-shots}/<page>-<N>-<state>-<theme>[-long][-min].png
# MOCKUPS=<dir> overrides the folder with the pages (used to test the kit with a throwaway page).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="${MOCKUPS:-$ROOT/docs/superpowers/mockups/m2}"
OUT="${OUT:-/tmp/revenant-shots}"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"

LONG=0; MIN=0; PAGES=()
for a in "$@"; do
  case "$a" in
    --long) LONG=1 ;;
    --min) MIN=1 ;;
    -*) echo "unknown option $a" >&2; exit 2 ;;
    *) PAGES+=("${a%.html}") ;;
  esac
done

if [ ${#PAGES[@]} -eq 0 ]; then
  for f in "$DIR"/*.html; do
    [ -e "$f" ] || continue
    n="$(basename "$f" .html)"
    [ "$n" = index ] || PAGES+=("$n")
  done
fi
if [ ${#PAGES[@]} -eq 0 ]; then echo "no pages besides index: nothing to shoot"; exit 0; fi

W=1280; H=800; SUF=""; QS=""
if [ "$MIN" = 1 ]; then W=1024; H=700; QS="&w=1024&h=700"; SUF="-min"; fi
if [ "$LONG" = 1 ]; then QS="$QS&long=1"; SUF="-long$SUF"; fi
mkdir -p "$OUT"

chrome() { "$CHROME" --headless=new --disable-gpu --hide-scrollbars --no-first-run "$@" 2>/dev/null; }

count=0
for page in "${PAGES[@]}"; do
  file="$DIR/$page.html"
  [ -f "$file" ] || { echo "missing page $file" >&2; exit 1; }
  dom="$(chrome --virtual-time-budget=3000 --dump-dom "file://$file?manifest=1" || true)"
  json="$(printf '%s' "$dom" | node -e '
    let s = ""; process.stdin.on("data", d => s += d).on("end", () => {
      const m = s.match(/<pre id="manifest"[^>]*>([\s\S]*?)<\/pre>/);
      if (!m) { console.error("no manifest in page"); process.exit(1); }
      const t = m[1].replace(/&quot;/g, "\"").replace(/&amp;/g, "&").replace(/&lt;/g, "<").replace(/&gt;/g, ">");
      for (const e of JSON.parse(t)) for (const k of e.states) console.log(e.page + " " + e.screen + " " + k);
    });')" || { echo "could not read the manifest of $page" >&2; exit 1; }
  while read -r pg screen state; do
    [ -n "$pg" ] || continue
    for theme in light dark; do
      png="$OUT/$pg-$screen-$state-$theme$SUF.png"
      rm -f "$png"
      chrome --window-size="$W,$H" --virtual-time-budget=4000 --screenshot="$png" \
        "file://$file?screen=$screen&state=$state&theme=$theme$QS" >/dev/null || true
      if [ ! -s "$png" ]; then echo "no screenshot written: $png" >&2; exit 1; fi
      count=$((count + 1))
    done
  done <<< "$json"
done
echo "wrote $count screenshots to $OUT"
