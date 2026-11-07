#!/usr/bin/env bash
# Downloads the public GBA test ROMs into <roms dir>/test/gba/. Idempotent; used locally and in CI.
#
#   scripts/fetch-gba-test-roms.sh [--with-sst]
#
# The roms dir is $GB_ROMS_DIR if set, else <repo>/roms (the same rule as gb-test).
# Every file is pinned to a commit (or, for tonc, to a hash, since its URL is unversioned) and
# verified against a sha256 before it is installed; a mismatch fails the script. An existing file
# is kept only if it matches, so a corrupted file is re-downloaded (and fails if upstream changed).
#
# --with-sst also fetches the SingleStepTests/ARM7TDMI files into sst/ (about 20 MB each, 45 in the
# full set). They are only needed by the gba-core CPU tests, so they are opt-in.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${GB_ROMS_DIR:-$ROOT/roms}/test/gba"

WITH_SST=0
for arg in "$@"; do
  case "$arg" in
    --with-sst) WITH_SST=1 ;;
    *) echo "usage: $0 [--with-sst]" >&2; exit 2 ;;
  esac
done

mkdir -p "$DEST"
cd "$DEST"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# True if $1 exists and matches the sha256 $2.
verified() { [ -s "$1" ] && [ "$(sha256_of "$1")" = "$2" ]; }

# The sha256 is mandatory here: every GBA file is pinned.
fetch() { # url out sha256
  if [ "$#" -ne 3 ] || [ -z "$3" ]; then echo "fetch: sha256 is required for $2" >&2; exit 1; fi
  if verified "$2" "$3"; then return; fi
  mkdir -p "$(dirname "$2")"
  curl -fsSL --retry 3 -o "$2.tmp" "$1"
  local got; got="$(sha256_of "$2.tmp")"
  if [ "$got" != "$3" ]; then
    rm -f "$2.tmp"
    echo "sha256 mismatch for $2: expected $3, got $got" >&2
    exit 1
  fi
  mv "$2.tmp" "$2"
}

# jsmolka/gba-tests (MIT, (c) 2019 Julian Smolka). No tags or releases; pinned to the master commit
# of 2025-04-19. The prebuilt .gba files are committed in the tree.
JSMOLKA_REV=a7113b67e63f83a9b321696ddd7042ccfad6c881
J="https://raw.githubusercontent.com/jsmolka/gba-tests/$JSMOLKA_REV"
while read -r sum path; do
  fetch "$J/$path" "jsmolka/$path" "$sum"
done <<'EOF'
77ee88662552bdc885c1080c0172ff119d54db791bd73b21808cf1ff1fe5b40e  arm/arm.gba
b5cb2291df4ab314b31c598acd9bff2ccfa0b38efff29daadfe97422ce369b67  thumb/thumb.gba
21024fb6aae6343f5f0466dd54e3149de1fbeb23f78e7d85a015c983684d2f87  memory/memory.gba
9d7b369fa1aa661ff03692b3d79c6f644b623d72983d0fc890e6d87a0409a3c9  bios/bios.gba
d990df112763087d0415b3785c1b4d31c0237794a704d0446fc5f5e474a44f98  nes/nes.gba
a37ad99c31e3f805eb05a00e498b65bd78e6f43a0a139cd695bea1f88229af2c  save/sram.gba
7e2aa32e943aedde88bd750eadcdbf55152d3a1ec61385011b7f15cd85b07c02  save/flash64.gba
9ac50e51d3ce4209dbdf85e472e70c067d5827e9af1bb3e707f6bd9059d5f0c6  save/flash128.gba
edb34ba6590d070c8a50cf0f3566b1e3cc679377b978224ff1b872d27f2b1630  save/none.gba
38aed48b67bc0f701e8aa222b0c3334bd306bd29888707bb7224d81f5576c264  ppu/hello.gba
127ce348f17e4d5fd8a0821a7af757eda6109c66c9f7b8f7d1e0fa31831b407d  ppu/shades.gba
80d6cc05b5f289b9a2392621fc245270f08e9f0d77fd33982120cef240f5a504  ppu/stripes.gba
bb727d59fa81915a5f5c609f4befb64872d3a3d830bc6ae0149e26410e648f85  unsafe/unsafe.gba
EOF

# FuzzARM (DenSinH, GPL-3.0). Fetched only, never committed or redistributed. Pinned to the
# last commit (2022-06-14). Pass protocol: the EWRAM word at 0x0200_0000 stays 0 (DECISIONS #48).
FUZZARM_REV=a675329cd57da48e3e406216ba2d79dd7e09ee20
F="https://raw.githubusercontent.com/DenSinH/FuzzARM/$FUZZARM_REV"
while read -r sum name; do
  fetch "$F/$name" "fuzzarm/$name" "$sum"
done <<'EOF'
5db4e020a61a0760043cb66b7149fa1777501080dbfc1b956c9600d44a4500f5  ARM_Any.gba
c9b96170c8a511eb3bb8adcf42fbf39eb51be09bbe0470150a51224238ea2f64  ARM_DataProcessing.gba
c89d9e0894d9ef5af5de6bf7819b32383acad535ec5ab8c7e5b4f6278dff34f6  THUMB_Any.gba
ad48a27d7a956f703a8813358f61de866b0e9a9988fb5603c7b2af91d38b0adc  THUMB_DataProcessing.gba
266e3d4f1dc231aadf9d296b13897cdc0de4c3cef73cf0c83806c0cef3422269  FuzzARM.gba
EOF

# armwrestler, GBA fork by destoer (licence: none stated upstream; fetch only, never commit).
# Menu driven and screen-only, so it is not in the manifest: for looking at by eye.
AW_REV=802e55ad61b421f4bbbc6f74f96aa4e5df4d630f
AW="https://raw.githubusercontent.com/destoer/armwrestler-gba-fixed/$AW_REV"
fetch "$AW/armwrestler-gba-fixed.gba" armwrestler/armwrestler-gba-fixed.gba \
  9f08d807c03ef296d38ef73e9b827a3d8c77cead9ced44b07d64c10f5f7d0746
fetch "$AW/armwrestler.gba" armwrestler/armwrestler.gba \
  0d8f0a2affaf74b18d0b22d75e598f73d121cf40e2883657a822bd9f7e59407a

# nba-emu/hw-test (BSD-3-Clause, (c) 2021 fleroviux). The GitHub copy is frozen (development moved
# to Codeberg); pinned to its last commit.
NBA_REV=3932a0e54dbb5d94b84798d2d5e462e080b95599
fetch "https://raw.githubusercontent.com/nba-emu/hw-test/$NBA_REV/dma/latch/latch.gba" \
  nba-hw-test/latch.gba 4e3a6510d30a67afe1b0e8bc8de4676bbbabeef5c0f0f3e960ff25c4c082373b

# tonc demo binaries (Jasper Vijn / Cearn; licence not stated on coranac.com). The URL is
# unversioned (v1.4.2, 2013), so the zip and every extracted file are pinned by sha256.
TONC_URL="https://www.coranac.com/files/tonc-bin.zip"
TONC_ZIP_SHA256="f172fc9308eb030ae8d3479872e9f3521b7f0cae3a64b0cd5f526b7b4b1650ba"
TONC_FILES="$(cat <<'EOF'
04d1d2a88985400eee83fbf5c00c95f024df858bb123b9f2a04aef65a96046e1  bigmap.gba
2990fbe308b5d5b28117eecf8721b36847e3d561ff08fe6e32bb1ee0eb5bc472  bld_demo.gba
98df73f1fbefc35f90696fce07572677697e7d456f2f1c67c02e507f20799e6a  bm_modes.gba
a00fcb792a0b24e97c958c4c81c98c49e7f20164ecfa323bcf8548b3c4cf6e89  brin_demo.gba
0fbc4a0a26eb8393e7aa57e801ec2e03e24bb286445315a1867756a6240e9a67  cbb_demo.gba
da15dc62fffc70bdb5768d6d8424310ef50aca129b44e9b6b3a71a9934a72987  dma_demo.gba
de22e95c42c433d69c66774cdd1eb88b17a199fa864542cdcd4e9a03f84c24b9  first.gba
0931334be5c0584fcea977fce9c3ea1bc68622c59aa1ce9ba1ddbf251402199e  hello.gba
9063aec13ce62675d7ee952609e2f9097adfacf41709b99d9f2cd89ddc87db2e  irq_demo.gba
db46e68f58640fa0add06adc903af7a364e8028e4a19ef88d437dc8c6bfda06d  key_demo.gba
00cbf4b8898041f4fe557d50cbd6fb6eaa4234c3682bf5b361534da8576a5b1c  m3_demo.gba
e1361e7d27d63fcacd95a2193c2595ff0a3a626e2e322ad5ffa23d66d97457fe  m7_demo_mb.gba
86644b01cd046fe267402c05d98d2a09f919ad6d407d484a12b1b20fcbc9ca82  m7_demo.gba
2adc5fc03b53486eb1c929453d9f6ffeed04002ec7203aaa8cb9e60d2409796f  m7_ex.gba
68f328f815606968a26ff145f78d7f0397918c15547a68a235daa144d9496785  mos_demo.gba
e35f276de2d22bbfe228e70b79b0d902721393cf0f6560a6f167f6ad66e29263  oacombo.gba
995316ffe41f04926994d3c65d343a398b4127b3036a172da4d9963bd1c6882a  obj_aff.gba
16779efecac741ee042cb2d69f8faa0c233524d988bdedcc60b02853cbe62fad  obj_demo.gba
16d571754c7177d905d9250257a6f7bb0874eca56472e633274e6faea5fce428  octtest.gba
b93399851568b1773094e2c84cde743e88187e50373a163a9bb73d821c7bb017  pageflip.gba
0a06fd52e005555759b3b8f1242259f7988b85f7c1bb2d1b539adb347c68cefe  prio_demo.gba
13cf10496eedd894aec56d7b0eba3cc9403c6de598bfe9a1432874f5f9fe5e80  sbb_aff.gba
1b429f9b0a5b91245925b8ae568ae8e2f419a3f1d1a56d16eb7be245f35c88be  sbb_reg.gba
59f3588473b20af8255a3c29fb6c8d963cde636d745901c05161444cec164c5b  second.gba
8e958a11b70d010775c9945a6f053e06924c1e51184f778e54cf5b75df021c65  snd1_demo.gba
6793e9c90ce1796b0125b095d03050e40287e5071be3bf30be65976962f0c46f  swi_demo.gba
781b263ff460b0e41b4011cb12ed754690d07a5631b025a2a2a5285cc7e8424a  swi_vsync.gba
55c649d336b53499d009aa076754988b12e9588085f1b46e5a81cc4091e51fc7  tmr_demo.gba
1a2b1910b2830ae3823a1bef50cf8b7d1408cdaf2fcba4d921bc81005e78199a  tonc-bin.txt
79ac8d96fdd4ff92da018003c23af4b96d57cbb158989b5a6d7200e59969f0e7  tte_demo.gba
338efcb844f9965878d1e973d18f4084b682d80003590f6e30f0c6f7843f2cbc  txt_bm.gba
a3370ebdbb7c6512e966cabe3def8f9a9ea34ff10df3693e8f9871b8cd47af3d  txt_obj.gba
c4720fb29f72b28f3cba7138fa7b9487f145977a71c6b7cd6604c71d3012ae40  txt_se1.gba
b461d63d1a60cc8d73ceebc79ff37d4242534d90d799a8be6c68ef71d66bb086  txt_se2.gba
1bb86a5af8c0653c941047194d21598c34892e86e4e0c63b2c4ec64150376e1a  win_demo.gba
EOF
)"
tonc_ok() {
  local sum name
  while read -r sum name; do
    verified "tonc/bin/$name" "$sum" || return 1
  done <<<"$TONC_FILES"
}
if ! tonc_ok; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL --retry 3 -o "$tmp/tonc-bin.zip" "$TONC_URL"
  got="$(sha256_of "$tmp/tonc-bin.zip")"
  if [ "$got" != "$TONC_ZIP_SHA256" ]; then
    echo "tonc-bin.zip sha256 mismatch: expected $TONC_ZIP_SHA256, got $got" >&2; exit 1
  fi
  mkdir -p "$tmp/x"
  unzip -q -o "$tmp/tonc-bin.zip" -d "$tmp/x"
  mkdir -p tonc/bin
  while read -r sum name; do
    if ! verified "$tmp/x/bin/$name" "$sum"; then
      echo "tonc/bin/$name missing from the zip or sha256 mismatch" >&2; exit 1
    fi
    cp "$tmp/x/bin/$name" "tonc/bin/$name"
  done <<<"$TONC_FILES"
fi

# SingleStepTests/ARM7TDMI (MIT, (c) 2024 SingleStepTests; generated from NanoBoyAdvance, not from
# hardware). Opt-in because of the size. The file list is the manifest the CPU tests use,
# crates/gba-core/tests/sst-files.txt: one file per line, a 64-hex sha256 and a `<name>.json.bin`
# name separated by whitespace (any leading path is dropped); `#` starts a comment.
if [ "$WITH_SST" -eq 1 ]; then
  SST_REV=e3097d88d428752736b949d94c094d5da50f0db6
  S="https://raw.githubusercontent.com/SingleStepTests/ARM7TDMI/$SST_REV/v1"
  MANIFEST="$ROOT/crates/gba-core/tests/sst-files.txt"
  [ -f "$MANIFEST" ] || { echo "error: $MANIFEST not found" >&2; exit 1; }
  SST_LIST="$(sed 's/#.*//' "$MANIFEST" | grep -v '^[[:space:]]*$' || true)"
  echo "SST: reading the file list from $MANIFEST"
  while read -r sum name; do
    [ -n "$sum" ] || continue
    name="${name##*/}"
    if ! [[ "$sum" =~ ^[0-9a-f]{64}$ ]] || [[ "$name" != *.json.bin ]] || [[ "$name" =~ [^A-Za-z0-9_.] ]]; then
      echo "bad line in the SST manifest: $sum $name" >&2; exit 1
    fi
    fetch "$S/$name" "sst/$name" "$sum"
  done <<<"$SST_LIST"
fi
echo "GBA test ROMs ready in $DEST"
