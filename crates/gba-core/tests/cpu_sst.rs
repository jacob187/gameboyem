//! SingleStepTests/ARM7TDMI harness (MIT, generated from NanoBoyAdvance, not
//! hardware). Files live in `roms/test/gba/sst/` (pinned commit and sha256s in
//! `tests/sst-files.txt`); a missing file skips the test unless `CI` is set.
//!
//! Each case loads a full CPU state and a 2-opcode pipeline, runs one `step`
//! against a recording bus and compares the final registers, the pipeline, the
//! next-fetch kind and every bus transaction (kind, size, address, data, N/S).
//! The `cycle` field is a soft metric: reported, never failing, because the
//! generator marks it experimental (stores, for one, repeat the previous cycle).
//!
//! Debug builds check a sample of each file; set `SST_FULL=1` (or use
//! `--release`) to run all 50 000 cases. `SST_VERBOSE=1` prints the first
//! failures of each file.

use gba_core::bus::{Access, CpuBus};
use gba_core::cpu::{Cpu, CpuState};
use std::path::PathBuf;

const MAGIC: u32 = 0xD33D_BAE0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tx {
    /// 0 = instruction read, 1 = data read, 2 = write.
    kind: u32,
    size: u32,
    addr: u32,
    data: u32,
    cycle: u32,
    access: u32,
}

struct Case {
    initial: CpuState,
    initial_access: u32,
    last: CpuState,
    last_access: u32,
    txs: Vec<Tx>,
    opcode: u32,
    base_addr: u32,
}

fn u32_at(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]])
}

/// Decode one state block; returns the state, its raw `access` word and the
/// block size (layout documented by `v1/transcode_json.py`).
fn parse_state(b: &[u8], p: usize) -> (CpuState, u32, usize) {
    let size = u32_at(b, p) as usize;
    let v: Vec<u32> = (0..40).map(|i| u32_at(b, p + 8 + i * 4)).collect();
    let mut s = CpuState::default();
    s.r.copy_from_slice(&v[0..16]);
    s.r_fiq.copy_from_slice(&v[16..23]);
    s.r_svc.copy_from_slice(&v[23..25]);
    s.r_abt.copy_from_slice(&v[25..27]);
    s.r_irq.copy_from_slice(&v[27..29]);
    s.r_und.copy_from_slice(&v[29..31]);
    s.cpsr = v[31];
    s.spsr.copy_from_slice(&v[32..37]);
    s.pipeline.copy_from_slice(&v[37..39]);
    s.next_fetch_seq = v[39] & 1 != 0;
    (s, v[39], size)
}

fn parse_file(b: &[u8]) -> Vec<Case> {
    assert_eq!(u32_at(b, 0), MAGIC, "bad SST magic");
    let n = u32_at(b, 4) as usize;
    let mut p = 8;
    let mut cases = Vec::with_capacity(n);
    for _ in 0..n {
        let test_size = u32_at(b, p) as usize;
        let start = p + 4;
        let (initial, initial_access, sz) = parse_state(b, start);
        let (last, last_access, sz2) = parse_state(b, start + sz);
        let mut q = start + sz + sz2;
        let tx_size = u32_at(b, q) as usize;
        let count = u32_at(b, q + 8) as usize;
        let txs = (0..count)
            .map(|i| {
                let o = q + 12 + i * 24;
                Tx {
                    kind: u32_at(b, o),
                    size: u32_at(b, o + 4),
                    addr: u32_at(b, o + 8),
                    data: u32_at(b, o + 12),
                    cycle: u32_at(b, o + 16),
                    access: u32_at(b, o + 20),
                }
            })
            .collect();
        q += tx_size;
        let opcode = u32_at(b, q + 8);
        let base_addr = u32_at(b, q + 12);
        cases.push(Case {
            initial,
            initial_access,
            last,
            last_access,
            txs,
            opcode,
            base_addr,
        });
        p += test_size;
    }
    cases
}

/// A flat bus that replays the recorded data and logs what the CPU did.
struct SstBus<'a> {
    expected: &'a [Tx],
    opcode: u32,
    base_addr: u32,
    log: Vec<Tx>,
    cycle: u32,
}

impl SstBus<'_> {
    fn record(&mut self, kind: u32, size: u32, addr: u32, data: u32, access: Access) {
        self.cycle += 1;
        let access = u32::from(access == Access::Seq) | if kind == 0 { 2 } else { 0 };
        self.log.push(Tx {
            kind,
            size,
            addr,
            data,
            cycle: self.cycle,
            access,
        });
    }

    fn data_read(&mut self, size: u32, addr: u32, access: Access) -> u32 {
        let i = self.log.len();
        let data = match self.expected.get(i) {
            Some(t) if t.kind == 1 => t.data,
            _ => 0,
        };
        self.record(1, size, addr, data, access);
        data
    }

    fn fetch(&mut self, size: u32, addr: u32, access: Access) -> u32 {
        let data = if addr == self.base_addr {
            self.opcode
        } else {
            addr
        };
        self.record(0, size, addr, data, access);
        data
    }
}

impl CpuBus for SstBus<'_> {
    fn read8(&mut self, addr: u32, access: Access) -> u8 {
        self.data_read(1, addr, access) as u8
    }
    fn read16(&mut self, addr: u32, access: Access) -> u16 {
        self.data_read(2, addr, access) as u16
    }
    fn read32(&mut self, addr: u32, access: Access) -> u32 {
        self.data_read(4, addr, access)
    }
    fn write8(&mut self, addr: u32, value: u8, access: Access) {
        self.record(2, 1, addr, u32::from(value), access);
    }
    fn write16(&mut self, addr: u32, value: u16, access: Access) {
        self.record(2, 2, addr, u32::from(value), access);
    }
    fn write32(&mut self, addr: u32, value: u32, access: Access) {
        self.record(2, 4, addr, value, access);
    }
    fn fetch16(&mut self, addr: u32, access: Access) -> u16 {
        self.fetch(2, addr, access) as u16
    }
    fn fetch32(&mut self, addr: u32, access: Access) -> u32 {
        self.fetch(4, addr, access)
    }
    fn idle(&mut self, cycles: u32) {
        self.cycle += cycles;
    }
    fn irq_line(&self) -> bool {
        false
    }
}

fn mask(size: u32) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => u32::MAX,
    }
}

fn state_diff(got: &CpuState, want: &CpuState) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..16 {
        if got.r[i] != want.r[i] {
            out.push(format!("r{i} {:08X} != {:08X}", got.r[i], want.r[i]));
        }
    }
    let banks: [(&str, &[u32], &[u32]); 5] = [
        ("fiq", &got.r_fiq, &want.r_fiq),
        ("svc", &got.r_svc, &want.r_svc),
        ("abt", &got.r_abt, &want.r_abt),
        ("irq", &got.r_irq, &want.r_irq),
        ("und", &got.r_und, &want.r_und),
    ];
    for (name, g, w) in banks {
        if g != w {
            out.push(format!("r_{name} {g:08X?} != {w:08X?}"));
        }
    }
    if got.cpsr != want.cpsr {
        out.push(format!("cpsr {:08X} != {:08X}", got.cpsr, want.cpsr));
    }
    if got.spsr != want.spsr {
        out.push(format!("spsr {:08X?} != {:08X?}", got.spsr, want.spsr));
    }
    if got.pipeline != want.pipeline {
        out.push(format!(
            "pipe {:08X?} != {:08X?}",
            got.pipeline, want.pipeline
        ));
    }
    if got.next_fetch_seq != want.next_fetch_seq {
        out.push(format!(
            "next_fetch_seq {} != {}",
            got.next_fetch_seq, want.next_fetch_seq
        ));
    }
    out
}

#[derive(Default)]
struct Stats {
    total: usize,
    excluded: usize,
    passed: usize,
    state_bad: usize,
    tx_bad: usize,
    cycles_ok: usize,
    /// Cases without a write transaction, and how many of those match the
    /// `cycle` field (stores repeat the previous cycle in the data).
    no_write: usize,
    no_write_cycles_ok: usize,
    seq_ok: usize,
}

fn run_case(c: &Case, verbose_budget: &mut usize, stats: &mut Stats) {
    if unpredictable(c.opcode, thumb_state(&c.initial), &c.initial) {
        stats.excluded += 1;
        return;
    }
    stats.total += 1;
    let mut cpu = Cpu::reset();
    cpu.set_state(&c.initial);
    let mut bus = SstBus {
        expected: &c.txs,
        opcode: c.opcode,
        base_addr: c.base_addr,
        log: Vec::new(),
        cycle: 0,
    };
    cpu.step(&mut bus);
    let got = cpu.state();

    let mut got = got;
    let mut want = c.last.clone();
    if thumb_state(&c.initial) {
        // An odd PC in Thumb state (POP {} and friends) is not defined.
        got.r[15] &= !1;
        want.r[15] &= !1;
    }
    if flags_unspecified(c.opcode, thumb_state(&c.initial)) {
        // C after a multiply is architecturally meaningless (see above).
        got.cpsr &= !0x2000_0000;
        want.cpsr &= !0x2000_0000;
    }
    let mut problems = state_diff(&got, &want);
    let state_ok = problems.is_empty();
    let mut tx_ok = bus.log.len() == c.txs.len();
    let mut seq_ok = true;
    let mut cycles_ok = bus.log.len() == c.txs.len();
    for (g, w) in bus.log.iter().zip(&c.txs) {
        let m = mask(w.size);
        if g.kind != w.kind || g.size != w.size || g.addr != w.addr || g.data & m != w.data & m {
            tx_ok = false;
        }
        if g.access & 1 != w.access & 1 {
            tx_ok = false;
            seq_ok = false;
        }
        if g.cycle != w.cycle {
            cycles_ok = false;
        }
    }
    if !tx_ok {
        problems.push(format!(
            "tx got {:08X?} want {:08X?}",
            bus.log
                .iter()
                .map(|t| (t.kind, t.size, t.addr, t.data, t.access & 1))
                .collect::<Vec<_>>(),
            c.txs
                .iter()
                .map(|t| (t.kind, t.size, t.addr, t.data, t.access & 1))
                .collect::<Vec<_>>()
        ));
    }
    stats.state_bad += usize::from(!state_ok);
    stats.tx_bad += usize::from(!tx_ok);
    stats.cycles_ok += usize::from(cycles_ok);
    if c.txs.iter().all(|t| t.kind != 2) {
        stats.no_write += 1;
        stats.no_write_cycles_ok += usize::from(cycles_ok);
    }
    stats.seq_ok += usize::from(seq_ok);
    if state_ok && tx_ok {
        stats.passed += 1;
    } else if *verbose_budget > 0 {
        *verbose_budget -= 1;
        eprintln!(
            "  FAIL op={:08X} base={:08X} cpsr={:08X} access={} (initial r15={:08X})",
            c.opcode, c.base_addr, c.initial.cpsr, c.initial_access, c.initial.r[15]
        );
        for p in problems {
            eprintln!("    {p}");
        }
    }
    let _ = c.last_access;
}

/// Encodings the ARM architecture marks UNPREDICTABLE, where the reference data
/// shows implementation accidents rather than behaviour worth copying:
/// * load/store or LDM/STM with writeback and Rn = R15,
/// * a long multiply whose RdHi and RdLo are the same register.
fn unpredictable(op: u32, thumb: bool, initial: &CpuState) -> bool {
    if thumb {
        return false;
    }
    // TSTP/TEQP/CMPP/CMNP with Rd=R15 restore CPSR from SPSR. If that flips the
    // Thumb bit we refill in the new state; the reference does not.
    if op & 0x0C00_0000 == 0 && op & (1 << 20) != 0 && (8..=11).contains(&((op >> 21) & 0xF)) {
        let spsr = match initial.cpsr & 0x1F {
            0x11 => Some(initial.spsr[0]),
            0x13 => Some(initial.spsr[1]),
            0x17 => Some(initial.spsr[2]),
            0x12 => Some(initial.spsr[3]),
            0x1B => Some(initial.spsr[4]),
            _ => None,
        };
        if (op >> 12) & 0xF == 15 && spsr.is_some_and(|p| (p ^ initial.cpsr) & 0x20 != 0) {
            return true;
        }
    }
    let rn = (op >> 16) & 0xF;
    match (op >> 25) & 7 {
        0b010 | 0b011 => {
            let writeback = op & (1 << 24) == 0 || op & (1 << 21) != 0;
            rn == 15 && writeback
        }
        0b100 => rn == 15 && op & (1 << 21) != 0,
        0b000 if op & 0x0F80_00F0 == 0x0080_0090 => rn == (op >> 12) & 0xF,
        0b000 if op & 0x0E00_0090 == 0x0000_0090 && op & 0x60 != 0 => {
            // Halfword transfer: same writeback rule.
            let writeback = op & (1 << 24) == 0 || op & (1 << 21) != 0;
            rn == 15 && writeback
        }
        _ => false,
    }
}

fn thumb_state(s: &CpuState) -> bool {
    s.cpsr & 0x20 != 0
}

/// Multiplies with S set: the C flag is "unpredictable" on the ARM7TDMI. The
/// reference data models some adder carry we do not reproduce (this emulator
/// leaves C unchanged, as mGBA does), so C is not compared for them.
fn flags_unspecified(opcode: u32, thumb: bool) -> bool {
    if thumb {
        opcode & 0xFFC0 == 0x4340
    } else {
        opcode & 0x0FC0_00F0 == 0x0000_0090 && opcode & (1 << 20) != 0
            || opcode & 0x0F80_00F0 == 0x0080_0090 && opcode & (1 << 20) != 0
    }
}

/// Cases that are knowingly not matched, with the reason (NanoBoyAdvance's
/// output differs from the hardware/GBATEK behaviour we implement). Keyed by
/// file name; the count is how many failing cases the file may have.
fn known_failures(_name: &str) -> usize {
    0
}

fn run_file(name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../roms/test/gba/sst");
    let path = dir.join(format!("{name}.json.bin"));
    let Ok(bytes) = std::fs::read(&path) else {
        // The 900 MB set is opt-in (`fetch-gba-test-roms.sh --with-sst`): skip if
        // none of it is present; a partly fetched set is an error in CI.
        let populated = std::fs::read_dir(&dir).is_ok_and(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".json.bin"))
        });
        assert!(
            !(populated && std::env::var_os("CI").is_some()),
            "{} missing in CI but other SST files are present",
            path.display()
        );
        eprintln!("skip {name}: {} not found", path.display());
        return;
    };
    let cases = parse_file(&bytes);
    let full = cfg!(not(debug_assertions)) || std::env::var_os("SST_FULL").is_some();
    let step = if full { 1 } else { 10 };
    let mut verbose = if std::env::var_os("SST_VERBOSE").is_some() {
        8
    } else {
        0
    };
    let mut stats = Stats::default();
    for c in cases.iter().step_by(step) {
        run_case(c, &mut verbose, &mut stats);
    }
    eprintln!(
        "{name}: {}/{} pass, {} excluded (state bad {}, transactions bad {}, N/S ok {}, cycle field ok {}, without writes {}/{})",
        stats.passed,
        stats.total,
        stats.excluded,
        stats.state_bad,
        stats.tx_bad,
        stats.seq_ok,
        stats.cycles_ok,
        stats.no_write_cycles_ok,
        stats.no_write
    );
    let failed = stats.total - stats.passed;
    assert!(
        failed <= known_failures(name),
        "{name}: {failed} failing cases"
    );
}

macro_rules! sst {
    ($($name:ident),+ $(,)?) => {
        $( #[test] fn $name() { run_file(stringify!($name)); } )+
    };
}

sst!(
    arm_b_bl,
    arm_bx,
    arm_cdp,
    arm_data_proc_immediate,
    arm_data_proc_immediate_shift,
    arm_data_proc_register_shift,
    arm_ldm_stm,
    arm_ldr_str_immediate_offset,
    arm_ldr_str_register_offset,
    arm_ldrh_strh,
    arm_ldrsb_ldrsh,
    arm_mcr_mrc,
    arm_mrs,
    arm_msr_imm,
    arm_msr_reg,
    arm_mul_mla,
    arm_mull_mlal,
    arm_stc_ldc,
    arm_swi,
    arm_swp,
    thumb_add_cmp_mov_hi,
    thumb_add_sp_or_pc,
    thumb_add_sub,
    thumb_add_sub_sp,
    thumb_b,
    thumb_bcc,
    thumb_bl_blx_prefix,
    thumb_bl_suffix,
    thumb_bx,
    thumb_data_proc,
    thumb_ldm_stm,
    thumb_ldr_pc_rel,
    thumb_ldr_str_imm_offset,
    thumb_ldr_str_reg_offset,
    thumb_ldr_str_sp_rel,
    thumb_ldrb_strb_imm_offset,
    thumb_ldrh_strh_imm_offset,
    thumb_ldrh_strh_reg_offset,
    thumb_ldrsb_strb_reg_offset,
    thumb_ldrsh_ldrsb_reg_offset,
    thumb_lsl_lsr_asr,
    thumb_mov_cmp_add_sub,
    thumb_push_pop,
    thumb_swi,
    thumb_undefined_bcc,
);
