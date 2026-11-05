#!/usr/bin/env bash
# Downloads the public test ROMs into roms/test/. Idempotent; used locally and in CI.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/roms/test"
mkdir -p "$DEST"
cd "$DEST"

BLARGG_REV="${BLARGG_REV:-c240dd7d700e5c0b00a7bbba52b53e4ee67b5f15}"
MTS="${MTS:-mts-20240926-1737-443f6e1}"
ACID2_URL="https://github.com/mattcurrie/dmg-acid2/releases/download/v1.0/dmg-acid2.gb"
# Pinned to the v1.0 commit (the same one the release ROM is built from) rather
# than the moving master branch (identical to master at the time of pinning).
# The ROM and reference are verified against the sha256 values below.
ACID2_REV="${ACID2_REV:-dc2295408f881637ff69f784d8d93f3d2db30181}"
ACID2_REF_URL="https://raw.githubusercontent.com/mattcurrie/dmg-acid2/$ACID2_REV/img/reference-dmg.png"
ACID2_SHA256="464e14b7d42e7feea0b7ede42be7071dc88913f75b9ffa444299424b63d1dff1"
ACID2_REF_SHA256="ca966d50895c7efef05838590d148c2cbfd7fba57dab986f25b35b4da71abb57"
# cgb-acid2, same shape. The v1.1 release ROM (header 0x143 = 0xC0) and the reference
# from the commit the v1.1 tag points at (160x144, 4-bit indexed colour). A release asset
# can be replaced, so the ROM is only trusted when its sha256 matches.
CGB_ACID2_URL="https://github.com/mattcurrie/cgb-acid2/releases/download/v1.1/cgb-acid2.gbc"
CGB_ACID2_REV="${CGB_ACID2_REV:-fa5b7f86d6fb599f79e55169494d981a7af75a31}"
CGB_ACID2_REF_URL="https://raw.githubusercontent.com/mattcurrie/cgb-acid2/$CGB_ACID2_REV/img/reference.png"
CGB_ACID2_SHA256="197fb0bcec544f0400527fc707e0a94f55435974986e6986b424ace5de81720e"
CGB_ACID2_REF_SHA256="9ea9c262c5383353e77d715d021a0f7c5ccbe438f88082cb225756e50c4fdf01"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# With a sha256, an existing file is kept only if it matches, and a download that doesn't
# match fails the script instead of being installed.
fetch() { # url out [sha256]
  local want="${3:-}"
  if [ -s "$2" ] && { [ -z "$want" ] || [ "$(sha256_of "$2")" = "$want" ]; }; then return; fi
  mkdir -p "$(dirname "$2")"
  curl -fsSL --retry 3 -o "$2.tmp" "$1"
  if [ -n "$want" ]; then
    local got; got="$(sha256_of "$2.tmp")"
    if [ "$got" != "$want" ]; then
      rm -f "$2.tmp"
      echo "sha256 mismatch for $2: expected $want, got $got" >&2
      exit 1
    fi
  fi
  mv "$2.tmp" "$2"
}

# Blargg (mirror of the original archives, pinned to a commit)
B="https://raw.githubusercontent.com/retrio/gb-test-roms/$BLARGG_REV"
fetch "$B/cpu_instrs/cpu_instrs.gb" blargg/cpu_instrs/cpu_instrs.gb
for f in 01-special 02-interrupts 03-op\ sp,hl 04-op\ r,imm 05-op\ rp 06-ld\ r,r \
         07-jr,jp,call,ret,rst 08-misc\ instrs 09-op\ r,r 10-bit\ ops 11-op\ a,\(hl\); do
  enc="${f// /%20}"; enc="${enc//,/%2C}"; enc="${enc//(/%28}"; enc="${enc//)/%29}"
  fetch "$B/cpu_instrs/individual/$enc.gb" "blargg/cpu_instrs/individual/$f.gb"
done
fetch "$B/instr_timing/instr_timing.gb" blargg/instr_timing/instr_timing.gb
fetch "$B/mem_timing/mem_timing.gb" blargg/mem_timing/mem_timing.gb
for f in 01-read_timing 02-write_timing 03-modify_timing; do
  fetch "$B/mem_timing/individual/$f.gb" "blargg/mem_timing/individual/$f.gb"
done
fetch "$B/dmg_sound/dmg_sound.gb" blargg/dmg_sound/dmg_sound.gb
for f in 01-registers 02-len\ ctr 03-trigger 04-sweep 05-sweep\ details 06-overflow\ on\ trigger \
         07-len\ sweep\ period\ sync 08-len\ ctr\ during\ power 09-wave\ read\ while\ on \
         10-wave\ trigger\ while\ on 11-regs\ after\ power 12-wave\ write\ while\ on; do
  fetch "$B/dmg_sound/rom_singles/${f// /%20}.gb" "blargg/dmg_sound/rom_singles/$f.gb"
done

# Mooneye test suite (prebuilt)
if [ ! -d mooneye ]; then
  curl -fsSL --retry 3 -o mts.zip "https://gekkio.fi/files/mooneye-test-suite/$MTS/$MTS.zip"
  unzip -q mts.zip && mv "$MTS" mooneye && rm mts.zip
fi

# dmg-acid2
fetch "$ACID2_URL" dmg-acid2/dmg-acid2.gb "$ACID2_SHA256"
fetch "$ACID2_REF_URL" dmg-acid2/reference-dmg.png "$ACID2_REF_SHA256"

# cgb-acid2
fetch "$CGB_ACID2_URL" cgb-acid2/cgb-acid2.gbc "$CGB_ACID2_SHA256"
fetch "$CGB_ACID2_REF_URL" cgb-acid2/reference.png "$CGB_ACID2_REF_SHA256"

# --- CGB: speed switch, double speed, HDMA (phase P2) ---
# Blargg cgb_sound (same retrio mirror commit as above; memory protocol like dmg_sound).
# These go in their own directory: roms/test/blargg may be a symlink to another checkout.
fetch "$B/cgb_sound/cgb_sound.gb" cgb_sound/cgb_sound.gb
for f in 01-registers 02-len\ ctr 03-trigger 04-sweep 05-sweep\ details 06-overflow\ on\ trigger \
         07-len\ sweep\ period\ sync 08-len\ ctr\ during\ power 09-wave\ read\ while\ on \
         10-wave\ trigger\ while\ on 11-regs\ after\ power 12-wave; do
  fetch "$B/cgb_sound/rom_singles/${f// /%20}.gb" "cgb_sound/rom_singles/$f.gb"
done

# SameSuite (LIJI32) DMA tests and the AGE speed-switch tests, taken from the c-sp/game-boy-test-roms
# v7.0 release (prebuilt with RGBDS; both report with the Fibonacci register protocol, like mooneye).
# The zip is verified against its sha256 before anything is extracted.
GBTR_URL="https://github.com/c-sp/game-boy-test-roms/releases/download/v7.0/game-boy-test-roms-v7.0.zip"
GBTR_SHA256="b9a9d7a1075aa35a3d07c07c34974048672d8520dca9e07a50178f5860c3832c"
if [ ! -s same-suite/dma/hdma_mode0.gb ] || [ ! -s age-test-roms/speed-switch/spsw-div-cgbBCE.gb ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL --retry 3 -o "$tmp/gbtr.zip" "$GBTR_URL"
  got="$(sha256_of "$tmp/gbtr.zip")"
  if [ "$got" != "$GBTR_SHA256" ]; then
    echo "game-boy-test-roms-v7.0.zip sha256 mismatch: $got" >&2; exit 1
  fi
  mkdir -p same-suite age-test-roms
  unzip -q -o "$tmp/gbtr.zip" 'same-suite/dma/*.gb' 'age-test-roms/speed-switch/*.gb' -d "$tmp/x"
  cp -R "$tmp/x/same-suite/dma" same-suite/
  cp -R "$tmp/x/age-test-roms/speed-switch" age-test-roms/
fi
echo "test ROMs ready in $DEST"
