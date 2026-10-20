#!/usr/bin/env bash
# Downloads the real camera samples listed in samples/samples.toml into samples/cache/ and checks each SHA-256.
# A file already in the cache with the right checksum is not downloaded again. The files are untrusted test data: they
# are only read by the engine's tests, never executed.
#
# Usage: scripts/fetch-samples.sh [CACHE_DIR]   (default: samples/cache next to samples.toml)
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
list="$root/samples/samples.toml"
cache="${1:-$root/samples/cache}"
mkdir -p "$cache"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

fetch() {
    local name="$1" url="$2" sum="$3"
    if [[ -z "$name" || -z "$url" || -z "$sum" ]]; then
        echo "samples.toml: incomplete entry (name='$name')" >&2
        exit 1
    fi
    if [[ "$name" == */* || "$name" == .* ]]; then
        echo "samples.toml: bad file name '$name'" >&2
        exit 1
    fi
    local dest="$cache/$name"
    if [[ -f "$dest" && "$(sha256 "$dest")" == "$sum" ]]; then
        echo "ok (cached) $name"
        return
    fi
    echo "downloading $name"
    curl --fail --location --silent --show-error --retry 3 --proto '=https' --output "$dest.part" "$url"
    local got
    got="$(sha256 "$dest.part")"
    if [[ "$got" != "$sum" ]]; then
        rm -f "$dest.part"
        echo "$name: SHA-256 mismatch (expected $sum, got $got)" >&2
        exit 1
    fi
    mv "$dest.part" "$dest"
    echo "ok $name"
}

name="" url="" sum=""
while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" == "[[sample]]" ]]; then
        [[ -n "$name$url$sum" ]] && fetch "$name" "$url" "$sum"
        name="" url="" sum=""
    elif [[ "$line" =~ ^(name|url|sha256)\ =\ \"([^\"]*)\"$ ]]; then
        case "${BASH_REMATCH[1]}" in
            name) name="${BASH_REMATCH[2]}" ;;
            url) url="${BASH_REMATCH[2]}" ;;
            sha256) sum="${BASH_REMATCH[2]}" ;;
        esac
    fi
done < "$list"
[[ -n "$name$url$sum" ]] && fetch "$name" "$url" "$sum"
echo "samples ready in $cache"
