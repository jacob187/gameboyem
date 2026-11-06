//! The GamePak prefetch buffer (WAITCNT bit 14), P3-TIMING. DECISIONS #70.
//!
//! GBATEK "GBA GamePak Prefetch": while the CPU is not using the cartridge bus the
//! GBA reads the opcodes that follow the last ROM opcode fetch into a buffer of up to
//! eight halfwords, and a fetch of a buffered opcode takes one cycle. The buffer only
//! serves opcode fetches from ROM; data reads of ROM never use it.
//!
//! # Model
//!
//! Nothing runs per cycle. The state is the buffer head, how many halfwords are
//! buffered and when the halfword being read next will complete (`done_at`), and it is
//! brought up to date lazily, from the master clock, whenever the CPU or DMA touches
//! the cartridge bus (`sync`). Each prefetched halfword costs the S time of its
//! wait-state area (N across a 128 KiB page, like any sequential cartridge access), and
//! reading stops while eight are buffered and resumes when the CPU takes one.
//!
//! * A fetch whose address is the head takes the buffered halfword (a hit, 1 cycle for
//!   the whole opcode), or waits for the one in flight to finish.
//! * Any other ROM fetch (a branch) is a miss: the buffer is dropped, the CPU pays the
//!   classic N or S cost, and prefetching restarts after the last halfword it read.
//! * A data or DMA access to ROM keeps the buffered halfwords, abandons the halfword in
//!   flight and restarts prefetching after that access with a non-sequential first read.
//!
//! Approximated: a miss or data access never waits for the halfword in flight, and the
//! buffer is not dropped by a data access (hardware behaviour for both is unverified).
//! The "prefetch disable bug" (GBATEK: with prefetch off, the opcode after an
//! instruction with internal cycles that does not change R15 is fetched N, not S) needs
//! no code here: the CPU already clears its sequential flag after any internal cycle
//! (`Cpu::internal`, DECISIONS #51), so with prefetch off that fetch is charged N.

use crate::bus::{Access, WaitTable};

/// Halfwords the buffer holds.
pub const CAPACITY: u8 = 8;

/// Cost of one prefetched halfword at `addr`: S, or N for the first read after a
/// restart (`seq` false) or at the start of a 128 KiB page.
#[inline]
fn half_cost(waits: &WaitTable, addr: u32, seq: bool) -> u64 {
    let ws = (((addr >> 25) & 3) as usize).min(2);
    let s = seq && addr & 0x1_FFFF != 0;
    u64::from(waits.rom[ws][usize::from(s)][0])
}

#[derive(Debug, Clone, Default)]
pub struct Prefetch {
    /// WAITCNT bit 14.
    pub enabled: bool,
    /// A stream is being prefetched (the fields below mean something).
    valid: bool,
    /// Address of the oldest buffered halfword: the next one the CPU fetches if the
    /// stream continues.
    head: u32,
    /// Halfwords buffered, 0..=8.
    count: u8,
    /// While `count < 8`: when the read of the halfword at `head + 2 * count` completes.
    done_at: u64,
    /// The read in progress (or the next one, while full) is sequential.
    seq: bool,
}

impl Prefetch {
    /// WAITCNT bit 14 changed. Turning it off empties the buffer; turning it on
    /// starts with nothing buffered.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        self.valid = false;
        self.count = 0;
    }

    /// Complete every halfword read that finished by `now`, at `waits`' costs.
    #[inline]
    pub fn sync(&mut self, now: u64, waits: &WaitTable) {
        while self.count < CAPACITY && self.done_at <= now {
            self.count += 1;
            self.seq = true;
            if self.count < CAPACITY {
                let next = self.head.wrapping_add(2 * u32::from(self.count));
                self.done_at += half_cost(waits, next, true);
            }
        }
    }

    /// The CPU takes the head halfword at `t`.
    #[inline]
    fn pop(&mut self, t: u64, waits: &WaitTable) {
        let was_full = self.count == CAPACITY;
        self.head = self.head.wrapping_add(2);
        self.count -= 1;
        if was_full {
            // Reading resumes now that a slot is free.
            let next = self.head.wrapping_add(2 * u32::from(self.count));
            self.done_at = t + half_cost(waits, next, self.seq);
        }
    }

    /// Start prefetching at `next` once the CPU's own read ends at `t`.
    #[inline]
    fn restart(&mut self, t: u64, next: u32, waits: &WaitTable) {
        self.valid = true;
        self.head = next;
        self.count = 0;
        self.seq = true;
        self.done_at = t + half_cost(waits, next, true);
    }

    /// Cycles for an opcode fetch from ROM at `addr` (a halfword, or a word when
    /// `wide`) starting at `now`, with the buffer state updated. `access` is the
    /// CPU's N/S claim, used on a miss only.
    pub fn fetch(
        &mut self,
        now: u64,
        waits: &WaitTable,
        addr: u32,
        wide: bool,
        access: Access,
    ) -> u64 {
        let base = addr & if wide { !3 } else { !1 };
        let halves = if wide { 2 } else { 1 };
        let mut t = now;
        for i in 0..halves {
            let a = base.wrapping_add(2 * i);
            if self.valid && self.head == a {
                self.sync(t, waits);
                if self.count == 0 {
                    // Being read right now: wait for it (`sync` took everything due,
                    // so `done_at` is in the future).
                    t = self.done_at;
                    self.sync(t, waits);
                }
                self.pop(t, waits);
                continue;
            }
            // Miss: the CPU reads the rest from the cartridge itself.
            t += if i == 0 {
                waits.rom_access(base, wide, access)
            } else {
                half_cost(waits, a, true)
            };
            self.restart(t, base.wrapping_add(2 * halves), waits);
            return t - now;
        }
        (t - now).max(1)
    }

    /// The CPU or a DMA channel accessed ROM as data, costing `cost` cycles from
    /// `now`: the halfword in flight is abandoned and prefetching resumes after the
    /// access, non-sequentially.
    pub fn data_access(&mut self, now: u64, waits: &WaitTable, cost: u64) {
        if !self.valid {
            return;
        }
        self.sync(now, waits);
        self.seq = false;
        if self.count < CAPACITY {
            let next = self.head.wrapping_add(2 * u32::from(self.count));
            self.done_at = now + cost + half_cost(waits, next, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::bios::Bios;
    use crate::bus::{Access, Bus, CpuBus};
    use crate::cart::Cart;

    const ROM: u32 = 0x0800_0000;

    /// A bus over a small zeroed ROM with WAITCNT = `waitcnt`.
    fn bus(waitcnt: u16) -> Bus {
        let mut bus = Bus::new(Cart::from_rom(vec![0; 0x400]).unwrap(), Bios::Hle);
        bus.write_io16(0x204, waitcnt, 0xFFFF);
        bus
    }

    /// Cycles one `f(bus)` call takes.
    fn took(bus: &mut Bus, f: impl FnOnce(&mut Bus)) -> u64 {
        let t = bus.sched.now();
        f(bus);
        bus.sched.now() - t
    }

    // WAITCNT with ws0 N = 3 waits (4 cycles) and S = 1 wait (2 cycles); bit 14 = prefetch.
    const WS0_FAST: u16 = 0x0014;
    const PREFETCH: u16 = 0x4000;

    #[test]
    fn prefetch_off_charges_the_classic_table() {
        let mut b = bus(WS0_FAST);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM, Access::NonSeq)), 4);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 2, Access::Seq)), 2);
        // Idle time buys nothing without the buffer.
        b.idle(50);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 4, Access::Seq)), 2);
        // A word is N+S, or S+S.
        assert_eq!(
            took(&mut b, |b| _ = b.fetch32(ROM + 0x100, Access::NonSeq)),
            6
        );
        assert_eq!(took(&mut b, |b| _ = b.fetch32(ROM + 0x104, Access::Seq)), 4);
    }

    #[test]
    fn bus_bound_code_costs_the_same_with_prefetch_on() {
        // Straight-line Thumb with no idle time: the prefetcher is busy the whole
        // time, so each opcode still takes one S read.
        let mut b = bus(WS0_FAST | PREFETCH);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM, Access::NonSeq)), 4);
        for i in 1..6 {
            let c = took(&mut b, |b| _ = b.fetch16(ROM + 2 * i, Access::Seq));
            assert_eq!(c, 2, "halfword {i}");
        }
        // ARM: N+S for the first word, then S+S.
        let mut b = bus(WS0_FAST | PREFETCH);
        assert_eq!(took(&mut b, |b| _ = b.fetch32(ROM, Access::NonSeq)), 6);
        assert_eq!(took(&mut b, |b| _ = b.fetch32(ROM + 4, Access::Seq)), 4);
        assert_eq!(took(&mut b, |b| _ = b.fetch32(ROM + 8, Access::Seq)), 4);
    }

    #[test]
    fn idle_cycles_fill_the_buffer_and_buffered_opcodes_cost_one() {
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq);
        // 20 cycles away from the bus: ten more halfwords could be read, the buffer
        // keeps eight.
        b.idle(20);
        for i in 1..=8 {
            let c = took(&mut b, |b| _ = b.fetch16(ROM + 2 * i, Access::Seq));
            assert_eq!(c, 1, "buffered halfword {i}");
        }
        // The buffer is empty again and reading resumed as the first one was taken:
        // by now (8 cycles later) it has read the ninth, which costs the rest.
        let c = took(&mut b, |b| _ = b.fetch16(ROM + 18, Access::Seq));
        assert!((1..=2).contains(&c));
        // A full word from the buffer is one cycle too.
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch32(ROM, Access::NonSeq);
        b.idle(40);
        assert_eq!(took(&mut b, |b| _ = b.fetch32(ROM + 4, Access::Seq)), 1);
        assert_eq!(took(&mut b, |b| _ = b.fetch32(ROM + 8, Access::Seq)), 1);
    }

    #[test]
    fn a_partly_read_opcode_waits_only_for_the_rest() {
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq); // t = 4; halfword 1 completes at 6
        b.idle(1); // t = 5
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 2, Access::Seq)), 1);
        // The halfword after it completes at 8 and the next at 10: at t = 6 asking
        // for a word (two halfwords) takes until 10.
        assert_eq!(took(&mut b, |b| _ = b.fetch32(ROM + 4, Access::Seq)), 4);
    }

    #[test]
    fn a_branch_drops_the_buffer_and_pays_the_non_sequential_cost() {
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq);
        b.idle(20);
        assert_eq!(
            took(&mut b, |b| _ = b.fetch16(ROM + 0x80, Access::NonSeq)),
            4
        );
        // Prefetching restarted after the target.
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 0x82, Access::Seq)), 2);
        b.idle(10);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 0x84, Access::Seq)), 1);
        // The old stream is gone.
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 2, Access::Seq)), 2);
    }

    #[test]
    fn code_running_from_ram_keeps_filling_the_buffer() {
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq);
        // 30 cycles of IWRAM opcode fetches.
        for _ in 0..30 {
            b.fetch16(0x0300_0000, Access::Seq);
        }
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 2, Access::Seq)), 1);
    }

    #[test]
    fn rom_data_reads_bypass_the_buffer_but_keep_its_contents() {
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq);
        b.idle(30);
        // A data read costs its own table entry, whatever is buffered.
        assert_eq!(took(&mut b, |b| _ = b.read16(ROM + 2, Access::NonSeq)), 4);
        // The buffered opcodes are still there.
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 2, Access::Seq)), 1);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 4, Access::Seq)), 1);
    }

    #[test]
    fn a_data_read_restarts_reading_non_sequentially() {
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq); // t = 4, reading ROM+2
        b.idle(1); // t = 5
        // The read takes 4 cycles (t = 9); the halfword in flight is abandoned and
        // ROM+2 is read again from t = 9 with an N read (4 cycles).
        b.read16(ROM + 0x200, Access::NonSeq);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 2, Access::Seq)), 4);
    }

    #[test]
    fn disabling_prefetch_empties_the_buffer() {
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq);
        b.idle(30);
        b.write_io16(0x204, WS0_FAST, 0xFFFF);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 2, Access::Seq)), 2);
        // Back on: nothing buffered until the next fetch starts a stream.
        b.write_io16(0x204, WS0_FAST | PREFETCH, 0xFFFF);
        b.idle(30);
        assert_eq!(took(&mut b, |b| _ = b.fetch16(ROM + 4, Access::Seq)), 2);
    }

    #[test]
    fn the_128k_page_start_is_non_sequential_for_the_prefetcher_too() {
        let mut b = bus(WS0_FAST | PREFETCH);
        let last = ROM + 0x1_FFFC;
        b.fetch16(last, Access::NonSeq); // t = 4
        b.fetch16(last + 2, Access::Seq); // reads 0x1FFFE: t = 6; next is the page start
        // Reading the first halfword of the next page takes N (4), not S (2).
        assert_eq!(
            took(&mut b, |b| _ = b.fetch16(ROM + 0x2_0000, Access::Seq)),
            4
        );
    }

    #[test]
    fn other_wait_state_areas_use_their_own_timings() {
        // Default WAITCNT: ws0 N=5 S=3, ws1 N=5 S=5, ws2 N=5 S=9 (cycles, 16-bit).
        for (base, s) in [(0x0800_0000u32, 3), (0x0A00_0000, 5), (0x0C00_0000, 9)] {
            let mut b = bus(0);
            assert_eq!(took(&mut b, |b| _ = b.fetch16(base, Access::NonSeq)), 5);
            assert_eq!(took(&mut b, |b| _ = b.fetch16(base + 2, Access::Seq)), s);
            let mut b = bus(PREFETCH);
            assert_eq!(took(&mut b, |b| _ = b.fetch16(base, Access::NonSeq)), 5);
            assert_eq!(took(&mut b, |b| _ = b.fetch16(base + 2, Access::Seq)), s);
        }
    }

    #[test]
    fn a_word_access_is_n_plus_s_and_the_page_rule_applies() {
        let b = bus(0);
        // N + S for a non-sequential word, S + S for a sequential one.
        assert_eq!(b.rom_cost(ROM, crate::bus::Width::W, Access::NonSeq), 5 + 3);
        assert_eq!(b.rom_cost(ROM + 4, crate::bus::Width::W, Access::Seq), 6);
        // A "sequential" access at the start of a 128 KiB page is charged N.
        assert_eq!(
            b.rom_cost(ROM + 0x2_0000, crate::bus::Width::W, Access::Seq),
            8
        );
        assert_eq!(
            b.rom_cost(ROM + 0x2_0000, crate::bus::Width::H, Access::Seq),
            5
        );
    }

    #[test]
    fn the_buffer_is_deterministic() {
        let run = || {
            let mut b = bus(WS0_FAST | PREFETCH);
            let mut v = Vec::new();
            for i in 0..40u32 {
                if i % 7 == 6 {
                    b.idle(i % 5);
                }
                let a = if i % 11 == 10 {
                    ROM + 0x40
                } else {
                    ROM + 2 * i
                };
                v.push(took(&mut b, |b| _ = b.fetch16(a, Access::Seq)));
            }
            v
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn a_waitcnt_write_syncs_the_buffer_under_the_old_timings() {
        // code-review T#1: halfwords read before the write were read at the old S
        // time (2 cycles), not the new one (3).
        let mut b = bus(WS0_FAST | PREFETCH);
        b.fetch16(ROM, Access::NonSeq); // t = 4, reading ROM+2 until 6
        b.idle(10); // t = 14: reads finished at 6, 8, 10, 12, 14
        b.write_io16(0x204, (WS0_FAST & !0x10) | PREFETCH, 0xFFFF); // ws0 S: 1 wait -> 2
        assert_eq!(b.prefetch.count, 5);
        // The same state as syncing explicitly first, then changing the table.
        let mut c = bus(WS0_FAST | PREFETCH);
        c.fetch16(ROM, Access::NonSeq);
        c.idle(10);
        c.prefetch.sync(c.sched.now(), &c.waits);
        c.write_io16(0x204, (WS0_FAST & !0x10) | PREFETCH, 0xFFFF);
        assert_eq!(c.prefetch.count, b.prefetch.count);
        assert_eq!(c.prefetch.done_at, b.prefetch.done_at);
    }

    #[test]
    fn hle_bios_reads_of_rom_use_the_cartridge_bus() {
        // code-review T#2: a CpuSet from ROM is cartridge-bus traffic, so the buffer
        // does not fill "for free" while it runs. ws2 is slow enough (S = 9 cycles)
        // that the SWI's 48-cycle overhead does not fill the buffer on its own. Same
        // elapsed time, once as a CpuSet and once as idle: the CpuSet's reads keep
        // restarting the prefetcher, so fewer halfwords are buffered afterwards.
        const WS2: u32 = 0x0C00_0000;
        let mut a = bus(PREFETCH);
        a.fetch16(WS2, Access::NonSeq);
        let t = a.sched.now();
        let mut r = [WS2 + 0x100, 0x0300_0000, 4, 0]; // four halfwords
        crate::bios::swi(&mut a, 0x0B, &mut r);
        let elapsed = a.sched.now() - t;
        let mut idle = bus(PREFETCH);
        idle.fetch16(WS2, Access::NonSeq);
        idle.idle(u32::try_from(elapsed).unwrap());
        for b in [&mut a, &mut idle] {
            let now = b.sched.now();
            b.prefetch.sync(now, &b.waits);
        }
        assert!(
            a.prefetch.count < idle.prefetch.count,
            "{} vs {}",
            a.prefetch.count,
            idle.prefetch.count
        );
    }
}
