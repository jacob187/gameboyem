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
