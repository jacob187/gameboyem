//! Shared helpers for the PPU integration tests: driving a bare `Ppu` for a frame,
//! hashing frames and writing them out as PNGs for review (no `png` dependency:
//! the file is written with uncompressed "stored" deflate blocks).
#![allow(dead_code)]

use gba_core::ppu::Ppu;
use gba_core::scheduler::Scheduler;
use gba_core::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};
use std::path::{Path, PathBuf};

/// Run `ppu` until the next frame completes (VBlank start).
pub fn run_frame(ppu: &mut Ppu, sched: &mut Scheduler) {
    loop {
        sched.skip_to_next();
        while let Some((ev, at)) = sched.pop_due() {
            if ppu.on_event(ev, at, sched).frame_done {
                return;
            }
        }
    }
}

/// A Ppu already started on its own scheduler.
pub fn new_ppu() -> (Ppu, Scheduler) {
    let mut ppu = Ppu::new();
    let mut sched = Scheduler::new();
    ppu.start(&mut sched);
    (ppu, sched)
}

/// FNV-1a of the framebuffer as little-endian bytes.
pub fn frame_hash(fb: &Framebuffer) -> u64 {
    let bytes: Vec<u8> = fb.iter().flat_map(|p| p.to_le_bytes()).collect();
    gba_core::fnv1a(&bytes)
}

pub fn set16(buf: &mut [u8], offset: usize, v: u16) {
    buf[offset..offset + 2].copy_from_slice(&v.to_le_bytes());
}

/// `<workspace>/target/gba-ppu`, created on demand.
pub fn out_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gba-ppu");
    std::fs::create_dir_all(&dir).expect("create target/gba-ppu");
    dir
}

/// Write `fb` as an RGB PNG, every pixel widened to 2x2.
pub fn write_png(fb: &Framebuffer, path: &Path) {
    const SCALE: usize = 2;
    let (w, h) = (SCREEN_WIDTH * SCALE, SCREEN_HEIGHT * SCALE);
    let widen = |c: u16| (c << 3 | c >> 2) as u8;
    let mut raw = Vec::with_capacity((w * 3 + 1) * h);
    for y in 0..h {
        raw.push(0); // filter: none
        for x in 0..w {
            let p = fb[(y / SCALE) * SCREEN_WIDTH + x / SCALE];
            raw.extend([widen(p & 31), widen(p >> 5 & 31), widen(p >> 10 & 31)]);
        }
    }
    // zlib: header, stored blocks of at most 65535 bytes, Adler-32.
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    while let Some(block) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        z.extend((block.len() as u16).to_le_bytes());
        z.extend((!(block.len() as u16)).to_le_bytes());
        z.extend(block);
    }
    z.extend(adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend((w as u32).to_be_bytes());
    ihdr.extend((h as u32).to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png).expect("write png");
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    b << 16 | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
