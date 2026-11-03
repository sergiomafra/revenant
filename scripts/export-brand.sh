#!/usr/bin/env bash
# Regenerates the brand PNGs in assets/brand from the SVG masters, using headless Chrome,
# then checks each PNG's size and alpha channel with sips (macOS). Needs Google Chrome.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
brand="$root/assets/brand"
chrome="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# render <svg> <png> <size>: the SVG is shown at exactly size x size on a transparent page.
# The SVG is copied next to the page and referenced by relative name, so odd characters in
# the repo path (spaces, %) never end up in a file:// URL.
render() {
  local svg="$1" png="$2" n="$3" name; name="$(basename "$2" .png)"
  [ -f "$svg" ] || { echo "missing SVG: $svg" >&2; exit 1; }
  cp "$svg" "$tmp/$name.svg"
  printf '<!doctype html><html><body style="margin:0;background:transparent"><img src="%s.svg" width="%s" height="%s" style="display:block"></body></html>' \
    "$name" "$n" "$n" > "$tmp/$name.html"
  rm -f "$png"  # Chrome can exit 0 without writing; never let a stale PNG pass
  "$chrome" --headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 \
    --window-size="$n,$n" --screenshot="$png" "file://$tmp/$name.html" >/dev/null 2>&1 || true
  [ -s "$png" ] || { echo "FAIL: Chrome did not write $png" >&2; exit 1; }
}

# Flat tile (full-bleed rounded square, no shadow) with the small symbol, for 32 and 16 px.
small="$brand/symbol-small.svg"
[ -f "$small" ] || { echo "missing SVG: $small" >&2; exit 1; }
{
  echo '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">'
  echo '<rect width="100" height="100" rx="22.46" fill="#2f5d50"/>'
  echo '<g style="color:#f6f3ec;--d:#e9c27a">'
  sed -e 's/^<?xml[^>]*>//' -e 's/<svg /<svg x="11" y="11" width="78" height="78" /' "$small"
  echo '</g></svg>'
} > "$tmp/tile-small.svg"

render "$brand/app-icon.svg" "$brand/app-icon-1024.png" 1024
render "$brand/app-icon-flat.svg" "$brand/app-icon-flat-1024.png" 1024
render "$tmp/tile-small.svg" "$brand/app-icon-32.png" 32
render "$tmp/tile-small.svg" "$brand/app-icon-16.png" 16

fail=0
for spec in app-icon-1024:1024 app-icon-flat-1024:1024 app-icon-32:32 app-icon-16:16; do
  png="$brand/${spec%%:*}.png"; n="${spec##*:}"
  out="$(sips -g pixelWidth -g pixelHeight -g hasAlpha "$png" 2>&1)" || { echo "FAIL $png: sips error" >&2; fail=1; continue; }
  w="$(awk '/pixelWidth/{print $2}' <<<"$out")"
  h="$(awk '/pixelHeight/{print $2}' <<<"$out")"
  a="$(awk '/hasAlpha/{print $2}' <<<"$out")"
  if [ "$w" = "$n" ] && [ "$h" = "$n" ] && [ "$a" = "yes" ]; then
    echo "ok   $(basename "$png") ${w}x${h} alpha"
  else
    echo "FAIL $(basename "$png"): got ${w}x${h} alpha=$a, want ${n}x${n} alpha=yes" >&2; fail=1
  fi
done
exit $fail
