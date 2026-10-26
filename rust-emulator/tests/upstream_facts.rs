// SPDX-License-Identifier: GPL-3.0-only
// Instruction facts measured on a real FM-1 by the upstream QEMU emulator
// (simonjohansson/fm1-emulator) that this interpreter already matches; kept
// as regression tests. Each test names the upstream commit that found it.
use fm1_emu::{bus::Bus, cpu::Cpu, RAM, XIP};

fn cpu(words: &[u16]) -> Cpu {
    Cpu::new(
        Bus::new(words.iter().flat_map(|w| w.to_le_bytes()).collect()).unwrap(),
        XIP,
    )
}

const DATA: u32 = RAM + 0x1000;

fn word(c: &Cpu, address: u32) -> u32 {
    c.bus.read(address, 4).unwrap()
}

fn put(c: &mut Cpu, address: u32, value: u32) {
    c.bus.write(address, value, 4).unwrap();
}

fn pair(c: &Cpu, low: usize) -> u64 {
    c.r[low] as u64 | (c.r[low + 1] as u64) << 32
}

fn byte_store_negative(base: u16, source: u16, offset: i32) -> [u16; 2] {
    let imm = (offset + 256) as u16;
    [
        0xee53,
        source << 12 | (imm & 0xf0) << 4 | base << 4 | (imm & 15),
    ]
}

#[test]
fn negative_offset_byte_stores_keep_neighbours_registers_and_flags() {
    // EE53: b[base + (imm8 - 256)] = rS, no writeback (upstream 8eee298).
    for offset in [-256, -255, -128, -16, -1] {
        let mut c = cpu(&byte_store_negative(0, 8, offset));
        put(&mut c, DATA + 12, 0xdeadbeef);
        put(&mut c, DATA + 16, 0x44332211);
        put(&mut c, DATA + 20, 0xcafebabe);
        let base = (DATA as i32 + 16 - offset) as u32;
        c.r[0] = base;
        c.r[8] = 0x123456a5;
        c.sr[5] = 0x89abcde5;
        c.step()
            .unwrap_or_else(|e| panic!("offset {offset}: {e:?}"));
        let expected = 0x443322a5u32;
        let stored = c.bus.read(DATA + 16, 4).unwrap();
        assert_eq!(stored, expected, "offset {offset}");
        assert_eq!(word(&c, DATA + 12), 0xdeadbeef);
        assert_eq!(word(&c, DATA + 20), 0xcafebabe);
        assert_eq!(c.r[0], base);
        assert_eq!(c.sr[5], 0x89abcde5);
    }
}

#[test]
fn byte_store_source_may_alias_the_base() {
    let mut c = cpu(&byte_store_negative(9, 9, -1));
    put(&mut c, DATA + 4, 0x44332211);
    c.r[9] = DATA + 8;
    c.step().unwrap();
    assert_eq!(word(&c, DATA + 4) >> 24, (DATA + 8) & 0xff);
    assert_eq!(c.r[9], DATA + 8);
}

#[test]
fn equal_compare_with_immediate_sign_extends() {
    // F874 FC03: if (r4 == -2) goto +3 halfwords; stock firmware compares
    // against negative immediates (upstream 3ba44d5).
    for (value, taken) in [(0xffff_fffeu32, true), (0x3fe, false)] {
        let mut c = cpu(&[0xf874, 0xfc03, 0x0020, 0x0020, 0x0020, 0x0020]);
        c.r[4] = value;
        c.step().unwrap();
        assert_eq!(c.pc, XIP + if taken { 4 + 6 } else { 4 }, "{value:#x}");
    }
}

#[test]
fn qualified_pair_keeps_the_head_over_a_discarded_load() {
    // F146 60F0 604E: r6 = r6 | 0xf0 || r6 = h[r4]; both write r6, the
    // head's value remains (upstream 3ba44d5, FM-1 clk_get).
    let mut c = cpu(&[0xf146, 0x60f0, 0x604e]);
    put(&mut c, DATA, 0x8000beef);
    c.r[4] = DATA;
    c.r[6] = 8;
    c.step().unwrap();
    assert_eq!(c.r[6], 0xf8);
}

#[test]
fn pair_head_may_read_a_special_register_for_a_store_tail() {
    // F064 3E00 6092: r3 = sp || [r1] = r2 (upstream 8789037).
    let mut c = cpu(&[0xf064, 0x3e00, 0x6092]);
    c.r[2] = 0x1234;
    c.r[1] = DATA;
    c.step().unwrap();
    assert_eq!(word(&c, DATA), 0x1234);
    assert_eq!(c.r[3], c.sr[14]);
}

#[test]
fn overlapping_pair_writes_resolve_with_the_head() {
    // D603 3BDB: r3 = r0 || r3 += 123 (upstream 8789037).
    let mut c = cpu(&[0xd603, 0x3bdb]);
    c.r[0] = 0x1234;
    c.r[3] = 0x555;
    c.step().unwrap();
    assert_eq!(c.r[3], 0x1234);
}

#[test]
fn umax_head_pairs_with_a_store_tail() {
    // F434 0020 6092: r0 = umax(r2, r0) || [r1] = r2 (upstream 8789037).
    let mut c = cpu(&[0xf434, 0x0020, 0x6092]);
    c.r[1] = DATA;
    c.r[2] = 5;
    c.r[0] = 3;
    c.step().unwrap();
    assert_eq!(c.r[0], 5);
    assert_eq!(word(&c, DATA), 5);
}

#[test]
fn halfword_offset_store_extends_signed_through_ed53() {
    // ED53 0A75: h[r7 + -92] = r0 (upstream 193fce6).
    let mut c = cpu(&[0xed53, 0x0a75]);
    c.r[7] = DATA + 92;
    c.r[0] = 0xabcd;
    put(&mut c, DATA, 0x11111111);
    c.step().unwrap();
    assert_eq!(word(&c, DATA) & 0xffff, 0xabcd);
}

#[test]
fn final_then_register_jump_with_else_leaves_the_block() {
    // IF always THEN jump r2 ELSE one insn; the target opens its own block
    // (upstream 0f9c06d, Felucca 1.5 at 0x02002a56).
    let mut c = cpu(&[0xea20, 0x1000, 0x00d2, 0x2144, 0xea20, 0x0000, 0x2544]);
    c.r[2] = XIP + 8;
    for _ in 0..4 {
        c.step().unwrap();
    }
    assert_eq!((c.r[4], c.r[5]), (5, 0));
    assert_eq!(c.pc, XIP + 14);
}

#[test]
fn final_then_taken_branch_leaves_the_block() {
    // The taken path of a branch ending a THEN arm retires the block, so the
    // target's own IF does not nest (upstream f4ea8a2).
    let mut c = cpu(&[
        0xea20, 0x0000, 0xf874, 0xfc03, 0x2144, 0x2144, 0x2144, 0xea20, 0x0000, 0x2544,
    ]);
    c.r[4] = 0xffff_fffe;
    for _ in 0..4 {
        c.step().unwrap();
    }
    assert_eq!(c.r[4], 5);
    assert_eq!(c.r[3], 0);
}

#[test]
fn idle_waits_for_an_interrupt_and_returns_to_the_next_instruction() {
    let mut c = cpu(&[0x0001, 0x2544]);
    c.step().unwrap();
    assert_eq!(c.pc, XIP + 2);
}

#[test]
fn float_arithmetic_produces_and_consumes_subnormals() {
    // E53F 3212 etc.: gradual underflow as measured (upstream e74f1ee).
    for (x, a, b, expected) in [
        (0x3212u16, 0x0080_0000u32, 0x3f00_0000u32, 0x0040_0000u32),
        (0x3211, 0x0080_0001, 0x0080_0000, 0x0000_0001),
        (0x3212, 0x0040_0000, 0x4000_0000, 0x0080_0000),
        (0x3211, 0x0000_0000, 0x0000_0001, 0x8000_0001),
    ] {
        let mut c = cpu(&[0xe53f, x]);
        c.r[1] = a;
        c.r[2] = b;
        c.sr[5] = 0;
        c.step().unwrap();
        assert_eq!(c.r[3], expected, "{x:#x} {a:#x} {b:#x}");
    }
}

#[test]
fn bit_test_alu_leads_a_parallel_pair() {
    // F194 ext 6083: r2 = r2 <op> (1 << r3) || [r0] = r3.
    for (subop, expected) in [
        (0u16, 0xf0f0f0f0u32 | 1 << 5),
        (1, 0xf0f0f0f0 ^ 1 << 5),
        (2, 0xf0f0f0f0 & 1 << 5),
        (3, 0xf0f0f0f0 & !(1 << 5)),
    ] {
        let mut c = cpu(&[0xf194, 0x2000 | 3 << 8 | 2 << 4 | subop, 0x6083]);
        c.r[0] = DATA;
        c.r[2] = 0xf0f0f0f0;
        c.r[3] = 5;
        c.step().unwrap_or_else(|e| panic!("subop {subop}: {e:?}"));
        assert_eq!(c.r[2], expected, "subop {subop}");
        assert_eq!(word(&c, DATA), 5);
    }
}

#[test]
fn add_with_carry_leads_a_parallel_pair_with_a_load() {
    // F0B8 2430 || r6 = h[r4]: r2 = r3 + r4 + C, the mixer's pair (f522768).
    let mut c = cpu(&[0xf0b8, 0x2430, 0x604e]);
    put(&mut c, DATA, 0x0000_beef);
    c.r[3] = 1;
    c.r[4] = DATA;
    c.sr[5] = 0;
    c.step().unwrap();
    assert_eq!(c.r[6], 0xbeef);
    assert_eq!(c.r[2], 1 + DATA);
}

#[test]
fn multiply_accumulate_leads_a_parallel_pair_with_a_load() {
    // F1FC F710 || r6 = h[r4]: r15_r14 += r1 * r7 (s) (upstream 14cd2d3).
    let mut c = cpu(&[0xf1fc, 0xf710, 0x604e]);
    put(&mut c, DATA, 0x0000_beef);
    c.r[1] = (-3i32) as u32;
    c.r[7] = 7;
    c.r[14] = 100;
    c.r[15] = 0;
    c.r[4] = DATA;
    c.step().unwrap();
    assert_eq!(c.r[6], 0xbeef);
    assert_eq!(pair(&c, 14) as i64, 79);
}

#[test]
fn presets_and_algorithm_encoders_sit_on_the_contacts_stock_firmware_reads() {
    // Stock firmware steps the preset number on the encoder at columns 0/1,
    // row 5 and opens the algorithm overlay on columns 2/3, row 0
    // (upstream 62536b2 swapped its transcription to this).
    use fm1_emu::encoders::{knob, CONTACTS};
    assert_eq!(CONTACTS[knob::PRESETS], [0, 5, 1, 5]);
    assert_eq!(CONTACTS[knob::ALGORITHM], [2, 0, 3, 0]);
}
