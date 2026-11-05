#!/bin/sh
# Check the fixed fields of a GBA ROM header: ARM branch at 0x00, Nintendo
# logo start, fixed value 0x96 at 0xB2, complement checksum at 0xBD.
# usage: check-header.sh game.gba [game.gba ...]
set -eu

status=0
for f in "$@"; do
    byte() { od -An -tu1 -j "$1" -N 1 "$f" | tr -d ' \n'; }
    bad=0
    err() { echo "$f: $1" >&2; bad=1; status=1; }

    size=$(wc -c <"$f" | tr -d ' ')
    [ "$size" -ge 192 ] || { err "shorter than the 192 byte header"; continue; }

    [ "$(byte 3)" -eq 234 ] || err "byte 3 is not 0xEA (no ARM B at the entry point)"
    [ "$(od -An -tx1 -j 4 -N 4 "$f" | tr -d ' \n')" = 24ffae51 ] || err "logo does not start 24 FF AE 51"
    [ "$(byte 178)" -eq 150 ] || err "0xB2 is not the fixed value 0x96"

    sum=0
    for b in $(od -An -v -tu1 -j 160 -N 29 "$f"); do sum=$((sum + b)); done
    want=$(((-(sum + 25)) & 255))
    got=$(byte 189)
    [ "$got" -eq "$want" ] || err "complement check at 0xBD is $got, expected $want"

    title=$(dd if="$f" bs=1 skip=160 count=12 2>/dev/null | tr -d '\0')
    code=$(dd if="$f" bs=1 skip=172 count=4 2>/dev/null)
    [ "$bad" -eq 0 ] && echo "$f: header ok (title $title, code $code, $size bytes)"
done
exit "$status"
