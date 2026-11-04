# Builds the homebrew games with GBDK-2020 (lcc).
#
#   make -C homebrew                        # uses ../tools/gbdk/, writes ../roms/homebrew
#   make -C homebrew GBDK_HOME=/path/gbdk/ OUT=/path/to/out
#
# Objects, maps and linker output go to $(BUILD) (gitignored); only the
# finished .gb files are copied to $(OUT).

GBDK_HOME ?= ../tools/gbdk/
OUT       ?= ../roms/homebrew
BUILD     ?= build

LCC      := $(GBDK_HOME)/bin/lcc
ROMUSAGE := $(GBDK_HOME)/bin/romusage

CFLAGS  := -Icommon
# Linker flags shared by every game: map file (-Wl-m) and NoICE symbol file (-Wl-j).
LINKFLAGS := -Wl-m -Wl-j
# Cart type 0x03 = MBC1+RAM+BATTERY, 1 RAM bank (header code 0x02, 8 KiB) for
# the high score (see common/common.h). ROM stays 32 KB.
LDFLAGS := $(LINKFLAGS) -Wm-yt0x03 -Wm-ya1

GAMES   := snake breakout cgbdemo
HEADERS := $(wildcard common/*.h snake/src/*.h breakout/src/*.h)
CGBDEMO_HEADERS := $(wildcard cgbdemo/src/*.h)

COMMON_OBJ   := $(patsubst common/%.c,$(BUILD)/common/%.o,$(wildcard common/*.c))
SNAKE_OBJ    := $(patsubst snake/src/%.c,$(BUILD)/snake/%.o,$(wildcard snake/src/*.c)) $(COMMON_OBJ)
BREAKOUT_OBJ := $(patsubst breakout/src/%.c,$(BUILD)/breakout/%.o,$(wildcard breakout/src/*.c)) $(COMMON_OBJ)
# cgbdemo is self-contained (no common/): CGB-only, plain 32 KB ROM.
CGBDEMO_OBJ  := $(patsubst cgbdemo/src/%.c,$(BUILD)/cgbdemo/%.o,$(wildcard cgbdemo/src/*.c))

.PHONY: all clean usage $(GAMES)

all: $(GAMES)

snake: $(OUT)/snake.gb
breakout: $(OUT)/breakout.gb
cgbdemo: $(OUT)/cgbdemo.gb

$(OUT)/snake.gb: $(BUILD)/snake/snake.gb | $(OUT)
	cp $< $@

$(OUT)/breakout.gb: $(BUILD)/breakout/breakout.gb | $(OUT)
	cp $< $@

$(OUT)/cgbdemo.gb: $(BUILD)/cgbdemo/cgbdemo.gb | $(OUT)
	cp $< $@

# Link step (title must be <= 15 chars for the header).
$(BUILD)/snake/snake.gb: $(SNAKE_OBJ)
	$(LCC) $(LDFLAGS) -Wm-yn"SNAKE" -o $@ $^

$(BUILD)/breakout/breakout.gb: $(BREAKOUT_OBJ)
	$(LCC) $(LDFLAGS) -Wm-yn"BREAKOUT" -o $@ $^

# CGB-only (-Wm-yC: header 0x143 = 0xC0). Cart type stays 0 (ROM only). The
# output keeps the .gb extension: CI uploads roms/homebrew/*.gb.
$(BUILD)/cgbdemo/cgbdemo.gb: $(CGBDEMO_OBJ)
	$(LCC) $(LINKFLAGS) -Wm-yC -Wm-yn"CGBDEMO" -o $@ $^

$(BUILD)/snake/%.o: snake/src/%.c $(HEADERS) | $(BUILD)/snake
	$(LCC) $(CFLAGS) -c -o $@ $<

$(BUILD)/breakout/%.o: breakout/src/%.c $(HEADERS) | $(BUILD)/breakout
	$(LCC) $(CFLAGS) -c -o $@ $<

# No -Icommon (it does not use common/). The Makefile is a prerequisite so a flag
# change rebuilds it, and the link step follows since it depends on these objects.
$(BUILD)/cgbdemo/%.o: cgbdemo/src/%.c $(CGBDEMO_HEADERS) Makefile | $(BUILD)/cgbdemo
	$(LCC) -c -o $@ $<

$(BUILD)/common/%.o: common/%.c $(HEADERS) | $(BUILD)/common
	$(LCC) $(CFLAGS) -c -o $@ $<

$(BUILD)/snake $(BUILD)/breakout $(BUILD)/cgbdemo $(BUILD)/common $(OUT):
	mkdir -p $@

# Print ROM/RAM usage from the linker maps (needs a prior build).
usage:
	@for g in $(GAMES); do echo "== $$g"; $(ROMUSAGE) $(BUILD)/$$g/$$g.map; done

clean:
	rm -rf $(BUILD)
	rm -f $(foreach g,$(GAMES),$(OUT)/$(g).gb)
