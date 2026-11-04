#!/usr/bin/env bash
# Downloads a corpus of open-source homebrew games (DMG, then CGB) into roms/corpus/.
# Idempotent; used locally and in CI. Every file is pinned by URL and verified
# against a sha256, so a changed or truncated download fails loudly.
#
# Only freely redistributable games belong here (no commercial ROMs). The ROMs
# are NOT committed (roms/ is gitignored); this script is the whole record of
# where they come from. Renegade Rush and Breksta's Cat come from the upstream
# GitHub release (byte-identical to the Homebrew Hub copy); the rest from the
# gbdev Homebrew Hub database, pinned to one commit, because those projects
# publish no prebuilt .gb release (or, for Adjustris/AIRPLANZ, the release is a
# different build: Adjustris v1.1, or AIRPLANZ 1.2 which is a plain ROM-only cart).
#
# | File                     | Game (source repository)                          | License    | Cart type (0x147)           | ROM / RAM     |
# |--------------------------|---------------------------------------------------|------------|-----------------------------|---------------|
# | tobutobugirl.gb          | Tobu Tobu Girl (SimonLarsen/tobutobugirl)         | MIT        | 0x03 MBC1+RAM+BATTERY       | 256 KiB / 8K  |
# | squishy-the-turtle.gb    | Squishy the Turtle (potatolain/SquishyTheTurtle)  | MIT        | 0x03 MBC1+RAM+BATTERY       | 128 KiB / 8K  |
# | sushi-nights.gb          | Sushi Nights (Zal0/SushiNights)                   | MIT        | 0x01 MBC1                   | 128 KiB / -   |
# | brekstascat.gb           | Breksta's Cat (NotImplementedLife/brekstascat)    | GPL-3.0    | 0x10 MBC3+TIMER+RAM+BATTERY | 128 KiB / 8K  |
# | adjustris.gb             | Adjustris (tbsp/Adjustris)                        | CC0-1.0    | 0x13 MBC3+RAM+BATTERY       | 32 KiB / 8K   |
# | airplanz.gb              | AIRPLANZ (NotImplementedLife/AIRPLANZ)            | GPL-3.0    | 0x1B MBC5+RAM+BATTERY       | 32 KiB / 2K   |
# | renegade-rush.gb         | Renegade Rush (QuinnPainter/Renegade-Rush)        | MIT        | 0x1B MBC5+RAM+BATTERY       | 64 KiB / 8K   |
#
# License column is the source repository's LICENSE. Homebrew Hub lists Breksta's
# Cat as MIT; its repository says GPL-3.0 (either permits redistribution, the
# stricter one is recorded). Caveats: Tobu Tobu Girl's assets are CC BY 4.0
# (attribution: Simon Larsen); Renegade Rush credits third-party asset packs
# (public-domain Spy Fighter pack, WitchFont8, MaterialFuture) and Breksta's Cat
# embeds third-party pictures in its puzzles. Considered excluded on licence:
# Porklike (README: game content owned by the original holders) and Max Pirate
# Extra Boom (GB Oracles asset packs, no stated terms). The first seven
# corpus games are DMG-compatible (CGB flag 0x143 = 0x00).
#
# CGB corpus (run on a Game Boy Color by the `smoke-cgb` cases; DECISIONS #39). All six are
# from the Homebrew Hub pin below; none has a prebuilt release for the same build.
#
# | File                     | Game (source repository)                              | License    | CGB (0x143) | Cart type (0x147)     | ROM / RAM      |
# |--------------------------|-------------------------------------------------------|------------|-------------|-----------------------|----------------|
# | tobutobugirl-dx.gb       | Tobu Tobu Girl Deluxe (SimonLarsen/tobutobugirl-dx)   | MIT        | 0x80 both   | 0x03 MBC1+RAM+BATTERY | 256 KiB / 8K   |
# | escape-obliterizer-dx.gb | Escape Obliterizer DX (jcnmsg/escape-obliterizer-gbc) | GPL-3.0    | 0xC0 only   | 0x03 MBC1+RAM+BATTERY | 32 KiB / 8K    |
# | rebound.gb               | Rebound (DevEd2/ReboundGB)                            | MIT        | 0xC0 only   | 0x19 MBC5             | 128 KiB / -    |
# | aevilia.gb               | Aevilia (ISSOtm/Aevilia-GB)                           | Apache-2.0 | 0xC0 only   | 0x1B MBC5+RAM+BATTERY | 128 KiB / 128K |
# | ucity.gb                 | uCity (AntonioND/ucity)                               | GPL-3.0+   | 0xC0 only   | 0x1B MBC5+RAM+BATTERY | 128 KiB / 128K |
# | geometrix.gb             | Geometrix (AntonioND/geometrix)                       | GPL-3.0+   | 0x80 both   | 0x1B MBC5+RAM+BATTERY | 32 KiB / 8K    |
#
# Licences are from each source repository (LICENSE, or readme.rst for AntonioND's two, whose
# media is CC BY-SA 4.0; Tobu Tobu Girl Deluxe assets are CC BY 4.0). No MBC3 CGB game with a
# redistributable licence exists in the Hub: its only MBC3 CGB games (Temporal Light - Jem of
# Twilight, Super Bunny Mission, Marla and the Elemental Rings) have a bespoke fan-content
# licence or none, so they are excluded.
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
fetch sushi-nights.gb          "$HH/sushi-nights/SushiNights.gb"                5b3203e60e8815acf75e25c5ae369815da047c0f46b81ad3bcc8513ce7167e25
fetch brekstascat.gb           "https://github.com/NotImplementedLife/brekstascat/releases/download/1.3/brekstascat_1_3.gb" e46dc09ce51b0bf3ca5c4539350ab7e2d4ea4d428329540605aa0a978ac3ece8
fetch adjustris.gb             "$HH/adjustris/adjustris.gb"                     47f1fa0b5d0ee47f6380c8c388dce2a839f73a55cb4060a4decb44cf36c2a13b
fetch airplanz.gb              "$HH/airplanz/AIRPLANZ.gb"                       736b821e17f5509d9b0088289977e1232e18338cad923f3e5588300d8622a662
fetch renegade-rush.gb         "https://github.com/QuinnPainter/Renegade-Rush/releases/download/v1.0/RenegadeRush.gb" 0511a3de1bb59a5bd8bcf4e1463df7ea03a2aae66f30542b85ee51aa25f527a6
fetch tobutobugirl-dx.gb       "$HH/tobutobugirldeluxe/tobudx.gb"               0a0e8018dbbc8d7f8cd99f05e7cdc7b4cc9e358ecfe9377ebfb2291a84c6e310
fetch escape-obliterizer-dx.gb "$HH/escape-obliterizer-dx/ESCAPE_OBLITERIZER_DX.gb" 7784d2e829ece4c6da154701051c1881f49c9c069667f7b6a65e666849750e81
fetch rebound.gb               "$HH/rebound/Rebound.gbc"                        195765b8ca3b0fb7d37b7b92d0242d6c7e01ec71f27f93e565fa081e449cbb92
fetch aevilia.gb               "$HH/aevilia/aevilia.gbc"                        67e784ed61846bc84bfe9f45b9da343371eec6b896ed1b945b64c0aec7aa735b
fetch ucity.gb                 "$HH/ucity/ucity.gbc"                            0d608db175dd8ba22780be5df5fc17bf135876f6964f58ee141e4f8d81c62947
fetch geometrix.gb             "$HH/geometrix/geometrix.gbc"                    f13575af6bf87003c3eef8c2cab67a49ca6b02aa23ff6a31a38beb96d6eb1795
echo "homebrew corpus ready in $DEST"
