//! The memory-moving SWIs: CpuSet, CpuFastSet, BgAffineSet, ObjAffineSet,
//! BitUnPack, LZ77, Huffman, run-length and the Diff unfilters (GBATEK "BIOS
//! Memory Copy", "BIOS Rotation/Scaling Functions", "BIOS Decompression
//! Functions"). Every access goes through `CpuBus` with the width the real BIOS
//! uses, so the clock is charged and the VRAM/palette/OAM byte-write rules apply.
//! The first access of a call is non-sequential, the rest sequential.

use super::math;
use crate::BIOS_SIZE;
use crate::bus::{Access, Bus, CpuBus};

/// Extra cycles per unit on top of the bus accesses (loop control and ALU work
/// of the BIOS code, DECISIONS #65).
pub(super) const SET_PER_UNIT: u64 = 2;
pub(super) const FAST_SET_PER_BLOCK: u64 = 4;
pub(super) const AFFINE_PER_ENTRY: u64 = 36;
pub(super) const UNPACK_PER_UNIT: u64 = 3;
pub(super) const DECOMP_PER_BYTE: u64 = 4;

/// Bus access with sequential tracking.
struct Mem<'a> {
    bus: &'a mut Bus,
    seq: bool,
}

impl<'a> Mem<'a> {
    fn new(bus: &'a mut Bus) -> Self {
        Self { bus, seq: false }
    }
    fn acc(&mut self) -> Access {
        let a = if self.seq {
            Access::Seq
        } else {
            Access::NonSeq
        };
        self.seq = true;
        a
    }
    fn r8(&mut self, a: u32) -> u8 {
        let k = self.acc();
        self.bus.read8(a, k)
    }
    fn r16(&mut self, a: u32) -> u16 {
        let k = self.acc();
        self.bus.read16(a, k)
    }
    fn r32(&mut self, a: u32) -> u32 {
        let k = self.acc();
        self.bus.read32(a, k)
    }
    fn w8(&mut self, a: u32, v: u8) {
        let k = self.acc();
        self.bus.write8(a, v, k);
    }
    fn w16(&mut self, a: u32, v: u16) {
        let k = self.acc();
        self.bus.write16(a, v, k);
    }
    fn w32(&mut self, a: u32, v: u32) {
        let k = self.acc();
        self.bus.write32(a, v, k);
    }
}

fn charge(bus: &mut Bus, cycles: u64) {
    bus.sched.advance(cycles);
}

/// Output of a decompressor: byte writes (Wram variants) or halfword writes
/// (Vram variants: the low byte waits for its partner, so a back reference to
/// the byte just produced reads stale memory, as GBATEK's CAUTION describes).
struct Sink {
    dst: u32,
    pos: u32,
    vram: bool,
    pending: u8,
}

impl Sink {
    fn new(dst: u32, vram: bool) -> Self {
        Self {
            dst: if vram { dst & !1 } else { dst },
            pos: 0,
            vram,
            pending: 0,
        }
    }
    fn put(&mut self, m: &mut Mem, b: u8) {
        let at = self.dst.wrapping_add(self.pos);
        if !self.vram {
            m.w8(at, b);
        } else if self.pos & 1 == 0 {
            self.pending = b;
        } else {
            m.w16(
                at.wrapping_sub(1),
                u16::from(self.pending) | u16::from(b) << 8,
            );
        }
        self.pos += 1;
    }
    /// The byte `back` positions behind the write position, read from memory.
    fn back(&self, m: &mut Mem, back: u32) -> u8 {
        m.r8(self.dst.wrapping_add(self.pos).wrapping_sub(back))
    }
}

// ---- CpuSet / CpuFastSet -----------------------------------------------------

/// SWI 0x0B. Silently does nothing if the source reaches into the BIOS (GBATEK).
pub(super) fn cpu_set(bus: &mut Bus, r: &[u32; 4]) {
    let (n, fill, word) = (r[2] & 0x1F_FFFF, r[2] >> 24 & 1 != 0, r[2] >> 26 & 1 != 0);
    let unit = if word { 4 } else { 2 };
    let (mut s, mut d) = (r[0] & !(unit - 1), r[1] & !(unit - 1));
    if s < BIOS_SIZE as u32 {
        return;
    }
    let mut m = Mem::new(bus);
    let mut v = 0;
    for i in 0..n {
        if !fill || i == 0 {
            v = if word { m.r32(s) } else { u32::from(m.r16(s)) };
        }
        if word {
            m.w32(d, v);
        } else {
            m.w16(d, v as u16);
        }
        if !fill {
            s = s.wrapping_add(unit);
        }
        d = d.wrapping_add(unit);
    }
    charge(bus, u64::from(n) * SET_PER_UNIT);
}

/// SWI 0x0C: words in blocks of eight (the count is rounded up on the GBA).
pub(super) fn cpu_fast_set(bus: &mut Bus, r: &[u32; 4]) {
    let n = ((r[2] & 0x1F_FFFF) + 7) & !7;
    let fill = r[2] >> 24 & 1 != 0;
    let (mut s, mut d) = (r[0] & !3, r[1] & !3);
    if s < BIOS_SIZE as u32 {
        return;
    }
    let mut m = Mem::new(bus);
    let mut v = 0;
    for i in 0..n {
        if !fill || i == 0 {
            v = m.r32(s);
        }
        m.w32(d, v);
        if !fill {
            s = s.wrapping_add(4);
        }
        d = d.wrapping_add(4);
    }
    charge(bus, u64::from(n / 8) * FAST_SET_PER_BLOCK);
}

// ---- affine ------------------------------------------------------------------

/// `(pa, pb, pc, pd)` for scales `sx`, `sy` (8.8) and angle `theta`:
/// `pa = sx cos`, `pb = -sx sin`, `pc = sy sin`, `pd = sy cos`, each `>> 14`.
/// APPROXIMATION: the negation happens before the shift (the real BIOS may
/// round `pb` the other way by one LSB). DECISIONS #65.
fn affine_params(sx: i32, sy: i32, theta: u16) -> (i32, i32, i32, i32) {
    let (sin, cos) = math::sin_cos(theta);
    (
        (sx * cos) >> 14,
        (-(sx * sin)) >> 14,
        (sy * sin) >> 14,
        (sy * cos) >> 14,
    )
}

/// SWI 0x0E: r0 source (20 bytes per entry), r1 destination (16 bytes per entry),
/// r2 count.
pub(super) fn bg_affine_set(bus: &mut Bus, r: &[u32; 4]) {
    let (mut s, mut d, n) = (r[0], r[1], r[2]);
    let mut m = Mem::new(bus);
    for _ in 0..n {
        let cx = m.r32(s) as i32;
        let cy = m.r32(s.wrapping_add(4)) as i32;
        let dcx = i32::from(m.r16(s.wrapping_add(8)) as i16);
        let dcy = i32::from(m.r16(s.wrapping_add(10)) as i16);
        let sx = i32::from(m.r16(s.wrapping_add(12)) as i16);
        let sy = i32::from(m.r16(s.wrapping_add(14)) as i16);
        let theta = m.r16(s.wrapping_add(16));
        s = s.wrapping_add(20);
        let (pa, pb, pc, pd) = affine_params(sx, sy, theta);
        let x = cx.wrapping_sub(pa * dcx + pb * dcy);
        let y = cy.wrapping_sub(pc * dcx + pd * dcy);
        m.w16(d, pa as u16);
        m.w16(d.wrapping_add(2), pb as u16);
        m.w16(d.wrapping_add(4), pc as u16);
        m.w16(d.wrapping_add(6), pd as u16);
        m.w32(d.wrapping_add(8), x as u32);
        m.w32(d.wrapping_add(12), y as u32);
        d = d.wrapping_add(16);
    }
    charge(bus, u64::from(n) * AFFINE_PER_ENTRY);
}

/// SWI 0x0F: r0 source (8 bytes per entry), r1 destination, r2 count, r3 byte
/// offset between the four parameters (2 = contiguous, 8 = OAM).
pub(super) fn obj_affine_set(bus: &mut Bus, r: &[u32; 4]) {
    let (mut s, mut d, n, off) = (r[0], r[1], r[2], r[3]);
    let mut m = Mem::new(bus);
    for _ in 0..n {
        let sx = i32::from(m.r16(s) as i16);
        let sy = i32::from(m.r16(s.wrapping_add(2)) as i16);
        let theta = m.r16(s.wrapping_add(4));
        s = s.wrapping_add(8);
        let (pa, pb, pc, pd) = affine_params(sx, sy, theta);
        for (i, p) in [pa, pb, pc, pd].into_iter().enumerate() {
            m.w16(d.wrapping_add(off.wrapping_mul(i as u32)), p as u16);
        }
        d = d.wrapping_add(off.wrapping_mul(4));
    }
    charge(bus, u64::from(n) * (AFFINE_PER_ENTRY * 2 / 3));
}

// ---- decompression -----------------------------------------------------------

/// SWI 0x10 BitUnPack: r0 source, r1 destination (word aligned), r2 unpack info.
pub(super) fn bit_unpack(bus: &mut Bus, r: &[u32; 4]) {
    let (src, mut dst, info) = (r[0], r[1] & !3, r[2]);
    let mut m = Mem::new(bus);
    let len = u32::from(m.r16(info));
    let sw = u32::from(m.r8(info.wrapping_add(2)));
    let dw = u32::from(m.r8(info.wrapping_add(3)));
    let flags = m.r32(info.wrapping_add(4));
    if !matches!(sw, 1 | 2 | 4 | 8) || !matches!(dw, 1 | 2 | 4 | 8 | 16 | 32) {
        return;
    }
    let (offset, zero) = (flags & 0x7FFF_FFFF, flags >> 31 != 0);
    let (mut out, mut used) = (0u32, 0u32);
    for i in 0..len {
        let b = u32::from(m.r8(src.wrapping_add(i)));
        for j in 0..8 / sw {
            let mut v = b >> (j * sw) & ((1 << sw) - 1);
            if v != 0 || zero {
                v = v.wrapping_add(offset);
            }
            out |= v << used;
            used += dw;
            if used >= 32 {
                m.w32(dst, out);
                dst = dst.wrapping_add(4);
                (out, used) = (0, 0);
            }
        }
    }
    charge(bus, u64::from(len) * u64::from(8 / sw) * UNPACK_PER_UNIT);
}

/// SWI 0x11 / 0x12 LZ77.
pub(super) fn lz77(bus: &mut Bus, r: &[u32; 4], vram: bool) {
    let mut m = Mem::new(bus);
    let mut src = r[0];
    let size = m.r32(src) >> 8;
    src = src.wrapping_add(4);
    let mut out = Sink::new(r[1], vram);
    while out.pos < size {
        let flags = m.r8(src);
        src = src.wrapping_add(1);
        for bit in 0..8 {
            if out.pos >= size {
                break;
            }
            if flags & (0x80 >> bit) == 0 {
                let b = m.r8(src);
                src = src.wrapping_add(1);
                out.put(&mut m, b);
            } else {
                let b1 = u32::from(m.r8(src));
                let b2 = u32::from(m.r8(src.wrapping_add(1)));
                src = src.wrapping_add(2);
                let (len, disp) = ((b1 >> 4) + 3, ((b1 & 0xF) << 8 | b2) + 1);
                for _ in 0..len {
                    if out.pos >= size {
                        break;
                    }
                    let v = out.back(&mut m, disp);
                    out.put(&mut m, v);
                }
            }
        }
    }
    charge(bus, u64::from(size) * DECOMP_PER_BYTE);
}

/// SWI 0x14 / 0x15 run-length.
pub(super) fn rl(bus: &mut Bus, r: &[u32; 4], vram: bool) {
    let mut m = Mem::new(bus);
    let mut src = r[0];
    let size = m.r32(src) >> 8;
    src = src.wrapping_add(4);
    let mut out = Sink::new(r[1], vram);
    while out.pos < size {
        let flag = m.r8(src);
        src = src.wrapping_add(1);
        if flag & 0x80 != 0 {
            let v = m.r8(src);
            src = src.wrapping_add(1);
            for _ in 0..(flag & 0x7F) + 3 {
                if out.pos < size {
                    out.put(&mut m, v);
                }
            }
        } else {
            for _ in 0..(flag & 0x7F) + 1 {
                let v = m.r8(src);
                src = src.wrapping_add(1);
                if out.pos < size {
                    out.put(&mut m, v);
                }
            }
        }
    }
    charge(bus, u64::from(size) * DECOMP_PER_BYTE);
}

/// SWI 0x13 Huffman (word writes).
pub(super) fn huff(bus: &mut Bus, r: &[u32; 4]) {
    let mut m = Mem::new(bus);
    let src = r[0] & !3;
    let hdr = m.r32(src);
    let (bits, size) = (hdr & 0xF, hdr >> 8);
    if !(1..=8).contains(&bits) {
        return;
    }
    let tree_size = u32::from(m.r8(src.wrapping_add(4)));
    let tree = src.wrapping_add(5);
    let mut stream = src.wrapping_add(4 + (tree_size + 1) * 2);
    let mut dst = r[1] & !3;
    let (mut word, mut have, mut sw, mut left) = (0u32, 0u32, 0u32, 0u32);
    let mut written = 0;
    'outer: while written < size {
        let mut node_at = tree;
        for _ in 0..256 {
            if left == 0 {
                sw = m.r32(stream);
                stream = stream.wrapping_add(4);
                left = 32;
            }
            let bit = sw >> 31;
            sw <<= 1;
            left -= 1;
            let node = u32::from(m.r8(node_at));
            let next = (node_at & !1) + (node & 0x3F) * 2 + 2 + bit;
            let end_flag = if bit == 0 { 0x80 } else { 0x40 };
            let end = node & end_flag != 0;
            if end {
                let data = u32::from(m.r8(next)) & ((1 << bits) - 1);
                word = word >> bits | data << (32 - bits);
                have += bits;
                if have >= 32 {
                    m.w32(dst, word);
                    dst = dst.wrapping_add(4);
                    written += 4;
                    have = 0;
                }
                continue 'outer;
            }
            node_at = next;
        }
        break; // a tree deeper than its size allows: malformed, stop
    }
    charge(bus, u64::from(size) * DECOMP_PER_BYTE * 2);
}

/// SWI 0x16 / 0x17 (8-bit units) and 0x18 (16-bit units) difference unfilter.
pub(super) fn diff_unfilter(bus: &mut Bus, r: &[u32; 4], wide: bool, vram: bool) {
    let mut m = Mem::new(bus);
    let mut src = r[0] & !3;
    let size = m.r32(src) >> 8;
    src = src.wrapping_add(4);
    if wide {
        let mut dst = r[1] & !1;
        let mut acc = 0u16;
        for i in 0..size / 2 {
            let d = m.r16(src);
            src = src.wrapping_add(2);
            acc = if i == 0 { d } else { acc.wrapping_add(d) };
            m.w16(dst, acc);
            dst = dst.wrapping_add(2);
        }
    } else {
        let mut out = Sink::new(r[1], vram);
        let mut acc = 0u8;
        for i in 0..size {
            let d = m.r8(src);
            src = src.wrapping_add(1);
            acc = if i == 0 { d } else { acc.wrapping_add(d) };
            out.put(&mut m, acc);
        }
    }
    charge(bus, u64::from(size) * 2);
}
