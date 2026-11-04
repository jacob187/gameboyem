#!/usr/bin/env bash
# Downloads a corpus of open-source DMG homebrew games into roms/corpus/.
# Idempotent; used locally and in CI. Every file is pinned by URL and verified
# against a sha256, so a changed or truncated download fails loudly.
#
# Only freely redistributable games belong here (no commercial ROMs). The ROMs
# are NOT committed (roms/ is gitignored); this script is the whole record of
# where they come from. All files are fetched from the gbdev "Homebrew Hub"
# database, pinned to one commit, whose entries mirror each game's own release.
#
# | File                     | Game (source repository)                          | License    | Cart type (0x147)           | ROM / RAM     |
# |--------------------------|---------------------------------------------------|------------|-----------------------------|---------------|
# | tobutobugirl.gb          | Tobu Tobu Girl (SimonLarsen/tobutobugirl)         | MIT        | 0x03 MBC1+RAM+BATTERY       | 256 KiB / 8K  |
# | squishy-the-turtle.gb    | Squishy the Turtle (potatolain/SquishyTheTurtle)  | MIT        | 0x03 MBC1+RAM+BATTERY       | 128 KiB / 8K  |
# | porklike.gb              | Porklike GB (binji/porklike.gb)                   | MIT        | 0x01 MBC1                   | 64 KiB / -    |
# | sushi-nights.gb          | Sushi Nights (Zal0/SushiNights)                   | MIT        | 0x01 MBC1                   | 128 KiB / -   |
# | brekstascat.gb           | Breksta's Cat (NotImplementedLife/brekstascat)    | GPL-3.0    | 0x10 MBC3+TIMER+RAM+BATTERY | 128 KiB / 8K  |
# | adjustris.gb             | Adjustris (tbsp/Adjustris)                        | CC0-1.0    | 0x13 MBC3+RAM+BATTERY       | 32 KiB / 8K   |
# | max-pirate-extra-boom.gb | Max Pirate Extra Boom (MWehrstedt/MaxPirateExtra) | MIT        | 0x19 MBC5                   | 64 KiB / -    |
# | renegade-rush.gb         | Renegade Rush (QuinnPainter/Renegade-Rush)        | MIT        | 0x1B MBC5+RAM+BATTERY       | 64 KiB / 8K   |
#
# License column is the source repository's LICENSE. (Homebrew Hub lists
# Breksta's Cat as MIT; its repository says GPL-3.0. Either permits
# redistribution, the stricter one is recorded.) All eight are DMG-compatible
# (CGB flag 0x143 = 0x00).
#
# Raise HH_REV and refresh the sha256 values together if the pin is ever moved.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/roms/corpus"
mkdir -p "$DEST"
cd "$DEST"

HH_REV="${HH_REV:-94368e51470f14fe5c495e6e65c994f2c3b6c661}"
HH="https://raw.githubusercontent.com/gbdev/database/$HH_REV/entries"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

fetch() { # out url sha256
  if [ -s "$1" ] && [ "$(sha256_of "$1")" = "$3" ]; then return; fi
  curl -fsSL --retry 3 -o "$1.tmp" "$2"
  local got; got="$(sha256_of "$1.tmp")"
  if [ "$got" != "$3" ]; then
    rm -f "$1.tmp"
    echo "sha256 mismatch for $1: expected $3, got $got" >&2
    exit 1
  fi
  mv "$1.tmp" "$1"
}

fetch tobutobugirl.gb          "$HH/tobutobugirl/tobu.gb"                       5d3871cae77db2287807e8914fd21f76203e73aa3fec20955489d45cb4571d8f
fetch squishy-the-turtle.gb    "$HH/squishy-the-turtle/squishy-magfest.gb"      ac1e2eff95cdcf71ed931bd16c88f080038977b8b9dcde433bc023ee6b86c021
fetch porklike.gb              "$HH/porklike-gb/porklike.gb"                    d4ee86833a580c92aa789aef03407b90a6cc6eb27e70d3477c41c970d0cf3f11
fetch sushi-nights.gb          "$HH/sushi-nights/SushiNights.gb"                5b3203e60e8815acf75e25c5ae369815da047c0f46b81ad3bcc8513ce7167e25
fetch brekstascat.gb           "$HH/brekstascat/brekstascat_1_3.gb"             e46dc09ce51b0bf3ca5c4539350ab7e2d4ea4d428329540605aa0a978ac3ece8
fetch adjustris.gb             "$HH/adjustris/adjustris.gb"                     47f1fa0b5d0ee47f6380c8c388dce2a839f73a55cb4060a4decb44cf36c2a13b
fetch max-pirate-extra-boom.gb "$HH/maxpirateeb/maxpirateeb.gb"                 11320e82aea34d56c3b8b5cabd3c15122ff0bbb281ec397d39b3f80a0bda688c
fetch renegade-rush.gb         "$HH/renegade-rush/RenegadeRush.gb"              0511a3de1bb59a5bd8bcf4e1463df7ea03a2aae66f30542b85ee51aa25f527a6
echo "homebrew corpus ready in $DEST"
