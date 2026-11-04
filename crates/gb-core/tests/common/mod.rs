//! Shared helpers for the whole-machine robustness tests: a tiny deterministic
//! RNG (no external dependency), ROM builders, stable hashing and ROM discovery.
#![allow(dead_code)]

use gb_core::{Button, GameBoy, M_CYCLES_PER_FRAME};
use std::path::{Path, PathBuf};

/// xorshift64* (deterministic, seedable, good enough for fuzzing).
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // SplitMix-style scramble so small seeds give unrelated streams.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self(z | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn u8(&mut self) -> u8 {
        (self.next_u64() >> 32) as u8
    }

    pub fn u16(&mut self) -> u16 {
        (self.next_u64() >> 32) as u16
    }

    /// Uniform-ish value in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 11) % n as u64) as usize
    }

    /// Inclusive range.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    pub fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.u8();
        }
    }
}

/// FNV-1a, 64 bit.
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xCBF2_9CE4_8422_2325)
    }
}

impl Fnv {
    pub fn bytes(&mut self, data: &[u8]) -> &mut Self {
        for &b in data {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
        self
    }

    pub fn f32s(&mut self, data: &[f32]) -> &mut Self {
        for v in data {
            self.bytes(&v.to_bits().to_le_bytes());
        }
        self
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

pub fn hash_bytes(data: &[u8]) -> u64 {
    Fnv::default().bytes(data).finish()
}

pub fn hash_f32s(data: &[f32]) -> u64 {
    Fnv::default().f32s(data).finish()
}

/// Cartridge types the core claims to support.
pub const VALID_CART_TYPES: [u8; 17] = [
    0x00, 0x08, 0x09, 0x01, 0x02, 0x03, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x19, 0x1A, 0x1B, 0x1C, 0x1D,
    0x1E,
];

/// Header plus a program at 0x150, with a correct header checksum.
pub fn make_rom(len: usize, cart_type: u8, rom_code: u8, ram_code: u8, program: &[u8]) -> Vec<u8> {
    let len = len.max(0x8000);
    let mut rom = vec![0u8; len];
    // Interrupt vectors: RETI so enabled interrupts return instead of running
    // off into the entry point.
    for v in [0x40usize, 0x48, 0x50, 0x58, 0x60] {
        rom[v] = 0xD9;
    }
    // Entry point: NOP; JP 0x0150.
    rom[0x100..0x104].copy_from_slice(&[0x00, 0xC3, 0x50, 0x01]);
    rom[0x134..0x138].copy_from_slice(b"FUZZ");
    rom[0x147] = cart_type;
    rom[0x148] = rom_code;
    rom[0x149] = ram_code;
    let end = (0x150 + program.len()).min(0x4000);
    rom[0x150..end].copy_from_slice(&program[..end - 0x150]);
    fix_checksum(&mut rom);
    rom
}

pub fn fix_checksum(rom: &mut [u8]) {
    rom[0x14D] = rom[0x134..=0x14C]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
}

/// ROM-size code that exactly matches `len` bytes (32 KiB << code).
pub fn rom_code_for(len: usize) -> u8 {
    let mut code = 0u8;
    while (0x8000usize << code) < len && code < 8 {
        code += 1;
    }
    code
}

/// Incremental builder for a straight-line SM83 program that writes values to
/// arbitrary addresses, with optional delay loops, ending in a spin loop.
#[derive(Default)]
pub struct Program(pub Vec<u8>);

impl Program {
    /// `LD A,v ; LD (addr),A`
    pub fn write(&mut self, addr: u16, v: u8) -> &mut Self {
        self.0
            .extend_from_slice(&[0x3E, v, 0xEA, addr as u8, (addr >> 8) as u8]);
        self
    }

    /// `LD A,(addr)` (exercises read paths and their side effects).
    pub fn read(&mut self, addr: u16) -> &mut Self {
        self.0
            .extend_from_slice(&[0xFA, addr as u8, (addr >> 8) as u8]);
        self
    }

    /// `LD B,n ; DEC B ; JR NZ,-3` (n = 0 spins 256 times).
    pub fn delay(&mut self, n: u8) -> &mut Self {
        self.0.extend_from_slice(&[0x06, n, 0x05, 0x20, 0xFD]);
        self
    }

    pub fn ei(&mut self) -> &mut Self {
        self.0.push(0xFB);
        self
    }

    pub fn di(&mut self) -> &mut Self {
        self.0.push(0xF3);
        self
    }

    /// `JR -2` forever.
    pub fn spin(&mut self) -> &mut Self {
        self.0.extend_from_slice(&[0x18, 0xFE]);
        self
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Maximum program size that fits in ROM bank 0 after the header.
pub const MAX_PROGRAM: usize = 0x4000 - 0x150;

/// An input script: for each frame, a list of `(button, pressed)` events.
pub type InputScript = Vec<Vec<(Button, bool)>>;

pub fn random_input_script(rng: &mut Rng, frames: usize) -> InputScript {
    (0..frames)
        .map(|_| {
            let n = if rng.chance(1, 3) { rng.below(4) } else { 0 };
            (0..n)
                .map(|_| (rng.pick(&Button::ALL), rng.chance(1, 2)))
                .collect()
        })
        .collect()
}

pub fn apply_inputs(gb: &mut GameBoy, events: &[(Button, bool)]) {
    for &(b, p) in events {
        gb.set_button(b, p);
    }
}

/// Upper bound on M-cycles `step_frame` may take: the documented two-frame cap
/// plus the instruction that crosses it (an instruction is at most 6 M-cycles).
pub const STEP_FRAME_MAX_MCYCLES: u64 = 2 * M_CYCLES_PER_FRAME as u64 + 16;

/// Root of the optional real-ROM corpus, from `GB_ROMS_DIR`.
pub fn roms_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("GB_ROMS_DIR")?);
    dir.is_dir().then_some(dir)
}

/// All `.gb` files under `dir`, sorted for stable ordering.
pub fn find_roms(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "gb") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// Opcodes that lock the CPU.
pub const ILLEGAL_OPCODES: [u8; 11] = [
    0xD3, 0xDB, 0xDD, 0xE3, 0xE4, 0xEB, 0xEC, 0xED, 0xF4, 0xFC, 0xFD,
];

/// A random ROM image, 32 KiB to 1 MiB, with a mix of valid and invalid headers
/// and three styles of content (see inline comments).
pub fn random_rom(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(10) {
        0 => rng.range(0x150, 0x8000),     // short image
        1..=5 => 0x8000 << rng.below(6),   // 32 KiB ..= 1 MiB, power of two
        _ => rng.range(0x8000, 0x10_0000), // arbitrary size
    };
    let mut rom = vec![0u8; len];
    match rng.below(3) {
        // Pure noise, header included.
        0 => rng.fill(&mut rom),
        // Noise without illegal opcodes: the CPU runs wild instead of locking.
        1 => {
            for b in rom.iter_mut() {
                let mut v = rng.u8();
                while ILLEGAL_OPCODES.contains(&v) {
                    v = rng.u8();
                }
                *b = v;
            }
        }
        // Mostly-NOP image with a small noisy program at the entry point.
        _ => {
            let end = (0x150 + rng.range(16, 600)).min(len);
            for b in &mut rom[0x150..end] {
                *b = rng.u8();
            }
        }
    }
    if len >= 0x150 {
        rom[0x100..0x104].copy_from_slice(&[0x00, 0xC3, 0x50, 0x01]);
        // Cartridge type: valid, near-valid or garbage.
        rom[0x147] = match rng.below(8) {
            0 => rng.u8(),
            1 => rng.pick(&[
                0x04, 0x05, 0x06, 0x07, 0x0B, 0x0C, 0x0D, 0x14, 0x1F, 0xFC, 0xFF,
            ]),
            _ => rng.pick(&VALID_CART_TYPES),
        };
        // ROM size code: usually consistent with the image, sometimes not.
        rom[0x148] = match rng.below(6) {
            0 => rng.u8(),
            1 => rng.below(10) as u8,
            _ => rom_code_for(len),
        };
        rom[0x149] = match rng.below(6) {
            0 => rng.u8(),
            1 => rng.below(8) as u8,
            _ => rng.pick(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]),
        };
        if rng.chance(3, 4) {
            fix_checksum(&mut rom);
        }
    }
    rom
}

/// Iteration multiplier for soak runs: `GB_FUZZ_SCALE=50 cargo test ...`.
pub fn fuzz_scale() -> u64 {
    std::env::var("GB_FUZZ_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}
