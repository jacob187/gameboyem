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
# Cart type 0 = ROM only (32 KB, no MBC, no RAM).
LDFLAGS := -Wl-m -Wl-j -Wm-yt0

GAMES   := snake breakout
HEADERS := $(wildcard common/*.h snake/src/*.h breakout/src/*.h)

COMMON_OBJ   := $(BUILD)/common/common.o
SNAKE_OBJ    := $(patsubst snake/src/%.c,$(BUILD)/snake/%.o,$(wildcard snake/src/*.c)) $(COMMON_OBJ)
BREAKOUT_OBJ := $(patsubst breakout/src/%.c,$(BUILD)/breakout/%.o,$(wildcard breakout/src/*.c)) $(COMMON_OBJ)

.PHONY: all clean usage $(GAMES)

all: $(GAMES)

snake: $(OUT)/snake.gb
breakout: $(OUT)/breakout.gb

$(OUT)/snake.gb: $(BUILD)/snake/snake.gb | $(OUT)
	cp $< $@

$(OUT)/breakout.gb: $(BUILD)/breakout/breakout.gb | $(OUT)
	cp $< $@

# Link step (title must be <= 15 chars for the header).
$(BUILD)/snake/snake.gb: $(SNAKE_OBJ)
	$(LCC) $(LDFLAGS) -Wm-yn"SNAKE" -o $@ $^

$(BUILD)/breakout/breakout.gb: $(BREAKOUT_OBJ)
	$(LCC) $(LDFLAGS) -Wm-yn"BREAKOUT" -o $@ $^

$(BUILD)/snake/%.o: snake/src/%.c $(HEADERS) | $(BUILD)/snake
	$(LCC) $(CFLAGS) -c -o $@ $<

$(BUILD)/breakout/%.o: breakout/src/%.c $(HEADERS) | $(BUILD)/breakout
	$(LCC) $(CFLAGS) -c -o $@ $<

$(BUILD)/common/%.o: common/%.c $(HEADERS) | $(BUILD)/common
	$(LCC) $(CFLAGS) -c -o $@ $<

$(BUILD)/snake $(BUILD)/breakout $(BUILD)/common $(OUT):
	mkdir -p $@

# Print ROM/RAM usage from the linker maps (needs a prior build).
usage:
	@for g in $(GAMES); do echo "== $$g"; $(ROMUSAGE) $(BUILD)/$$g/$$g.map; done

clean:
	rm -rf $(BUILD)
	rm -f $(foreach g,$(GAMES),$(OUT)/$(g).gb)
