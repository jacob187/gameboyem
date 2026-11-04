#include <gb/gb.h>
#include <gbdk/console.h>
#include <gbdk/font.h>
#include <rand.h>
#include <stdio.h>

#include "common.h"

void hb_init(void)
{
    font_init();
    BGP_REG = PALETTE_DEFAULT;
    OBP0_REG = PALETTE_DEFAULT;
}

void print_at(uint8_t x, uint8_t y, const char *s)
{
    gotoxy(x, y);
    printf("%s", s);
}

void print_uint(uint8_t x, uint8_t y, uint16_t v, uint8_t width)
{
    char buf[6];
    uint8_t i = width;

    buf[width] = '\0';
    do {
        buf[--i] = (char)('0' + (v % 10u));
        v /= 10u;
    } while (v && i);
    while (i) {
        buf[--i] = ' ';
    }
    print_at(x, y, buf);
}

void wait_start_and_seed(uint8_t x, uint8_t y)
{
    uint8_t frames = 0;
    uint8_t shown = 0xFF;
    uint8_t want;

    /* Ignore a Start press carried over from the previous screen. */
    while (joypad() & J_START) {
        wait_vbl_done();
    }
    for (;;) {
        wait_vbl_done();
        frames++;
        want = (frames & 32u) == 0u;
        if (want != shown) {
            shown = want;
            print_at(x, y, want ? "PRESS START" : "           ");
        }
        if (joypad() & J_START) {
            break;
        }
    }
    initrand(((uint16_t)DIV_REG << 8) | frames);
}

/* ---- high score in cartridge RAM ------------------------------------- */

#define SRAM_BASE ((volatile uint8_t *)0xA000)
#define HS_LO     4u
#define HS_HI     5u
#define HS_SUM    6u

static const uint8_t hs_magic[4] = { 'G', 'B', 'H', 'S' };

static uint8_t hs_checksum(uint8_t lo, uint8_t hi)
{
    return (uint8_t)((uint8_t)(lo * 31u + hi) ^ 0xA5u);
}

uint16_t hiscore_load(void)
{
    volatile uint8_t *p = SRAM_BASE;
    uint8_t i, ok = 1, lo, hi, sum;

    ENABLE_RAM;
    SWITCH_RAM(0);
    for (i = 0; i < 4u; i++) {
        if (p[i] != hs_magic[i]) {
            ok = 0;
        }
    }
    lo = p[HS_LO];
    hi = p[HS_HI];
    sum = p[HS_SUM];
    DISABLE_RAM;

    if (!ok || sum != hs_checksum(lo, hi)) {
        return 0u;
    }
    return ((uint16_t)hi << 8) | lo;
}

uint8_t hiscore_update(uint16_t *record, uint16_t score)
{
    volatile uint8_t *p = SRAM_BASE;
    uint8_t i, lo, hi;

    if (score <= *record) {
        return 0u;
    }
    *record = score;
    lo = (uint8_t)score;
    hi = (uint8_t)(score >> 8);

    ENABLE_RAM;
    SWITCH_RAM(0);
    p[HS_LO] = lo;
    p[HS_HI] = hi;
    p[HS_SUM] = hs_checksum(lo, hi);
    for (i = 0; i < 4u; i++) {
        p[i] = hs_magic[i]; /* magic last: a torn write leaves no valid record */
    }
    DISABLE_RAM;
    return 1u;
}
