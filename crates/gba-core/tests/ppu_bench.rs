//! A print-only rendering benchmark, `#[ignore]`d so it never gates (timing varies
//! with machine load): `cargo test --release -p gba-core --test ppu_bench --
//! --ignored --nocapture`.

mod common;

use common::{new_ppu, run_frame, set16};
use std::time::Instant;

/// Mode 0 with four full 8bpp/4bpp text BGs, scrolled, and 128 sprites of
/// 32x32 and 64x64 (a deliberately heavy scene).
#[test]
#[ignore = "benchmark: prints ns per frame"]
fn bench_mode0_four_bgs_128_sprites() {
    let (mut ppu, mut sched) = new_ppu();
    for i in 0..512 {
        set16(&mut ppu.palette[..], i * 2, (i * 37 + 1) as u16 & 0x7FFF);
    }
    for (i, b) in ppu.vram.iter_mut().enumerate() {
        *b = (i * 7 + (i >> 8)) as u8 | 1;
    }
    for bg in 0..4usize {
        let cnt = bg as u16
            | ((bg as u16) << 2)
            | (u16::from(bg % 2 == 0) << 7)
            | ((16 + bg as u16) << 8);
        ppu.write_io(0x008 + 2 * bg as u32, cnt, 0xFFFF);
        ppu.write_io(0x010 + 4 * bg as u32, 13 * bg as u16 + 3, 0xFFFF);
        let screen = (16 + bg) * 0x800;
        for cell in 0..1024usize {
            set16(
                &mut ppu.vram[..],
                screen + cell * 2,
                ((cell * 5 + bg) % 60 + 1) as u16,
            );
        }
    }
    for i in 0..128usize {
        let o = i * 8;
        let size = 2 + (i % 2) as u16;
        set16(&mut ppu.oam[..], o, ((i * 11) % 150) as u16);
        set16(
            &mut ppu.oam[..],
            o + 2,
            (((i * 29) % 240) as u16) | size << 14,
        );
        set16(
            &mut ppu.oam[..],
            o + 4,
            (1 + (i * 3) % 500) as u16 | ((i % 4) as u16) << 10,
        );
    }
    ppu.write_io(0x000, 0x1F00 | 0x40, 0xFFFF);
    for _ in 0..5 {
        run_frame(&mut ppu, &mut sched);
    }
    let frames = 300u32;
    let start = Instant::now();
    for _ in 0..frames {
        run_frame(&mut ppu, &mut sched);
    }
    let per_frame = start.elapsed().as_nanos() / u128::from(frames);
    println!("mode 0, 4 BGs + 128 sprites: {per_frame} ns/frame");
    std::hint::black_box(ppu.framebuffer());
}
