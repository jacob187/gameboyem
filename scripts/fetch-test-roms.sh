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
ACID2_REF_URL="https://raw.githubusercontent.com/mattcurrie/dmg-acid2/master/img/reference-dmg.png"

fetch() { # url out
  if [ ! -s "$2" ]; then
    mkdir -p "$(dirname "$2")"
    curl -fsSL --retry 3 -o "$2.tmp" "$1" && mv "$2.tmp" "$2"
  fi
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
fetch "$ACID2_URL" dmg-acid2/dmg-acid2.gb
fetch "$ACID2_REF_URL" dmg-acid2/reference-dmg.png
echo "test ROMs ready in $DEST"
