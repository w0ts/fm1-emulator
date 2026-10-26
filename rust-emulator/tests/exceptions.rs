// SPDX-License-Identifier: GPL-3.0-only
// The CPU exception (vector 1) raised by a divide by zero when EMU_CON bit 2
// is set. Vendor csfr.h: q32DSP(n) at 0x1eef000 + 0x200 n, EMU_CON +0xd0,
// EMU_MSG +0xd4, ICFG00 +0x100; debug.c: request_irq(IRQ_EXCEPTION_IDX = 1,
// 7, ...), EMU_CON |= BIT(2) | BIT(8), emu_msg bit 2 "div0_err". X0X
// ea3c665: on a physical FM-1 the trap also fires on float divides.
// Vendor objdump: e1f4 0120 = r0 = r2 / r1 (u), e1f4 0121 = (s),
// e1f6 0040 = r1_r0 = r5_r4 / r0 (u), e53f 0103 = r0 = r0 / r1 (f).
use fm1_emu::{bus::Bus, cpu::Cpu, XIP};

const EMU_CON: u32 = 0x01ee_f0d0;
const EMU_MSG: u32 = 0x01ee_f0d4;
const ICFG00: u32 = 0x01ee_f100;
const HANDLER: u32 = XIP + 64;

fn cpu(words: &[u16]) -> Cpu {
    let mut image = words.to_vec();
    image.resize(48, 0);
    let mut c = Cpu::new(
        Bus::new(image.iter().flat_map(|w| w.to_le_bytes()).collect()).unwrap(),
        XIP,
    );
    c.bus.write(0x01c7_fe04, HANDLER, 4).unwrap();
    c
}

/// Source 1 enabled at priority 7, the controller on and the trap armed.
fn arm(c: &mut Cpu) {
    c.bus.write(ICFG00, 0xf0, 4).unwrap();
    c.sr[11] |= 0x100;
    c.bus.write(EMU_CON, 4 | 1 << 8, 4).unwrap();
}

fn assert_trapped(c: &Cpu, at: u32) {
    assert_eq!(c.pc, HANDLER);
    assert_eq!(c.sr[0], at, "reti holds the dividing instruction");
    assert_eq!((c.sr[11] >> 16) & 0x7f, 1, "ICFG records source 1");
    assert_eq!((c.sr[11] >> 24) & 7, 7, "and its priority");
    assert_eq!(c.bus.read(EMU_MSG, 4).unwrap() & 4, 4);
    assert_eq!(c.exception_entries, 1);
}

#[test]
fn float_divide_by_zero_traps_when_armed() {
    for divisor in [0.0f32, -0.0] {
        let mut c = cpu(&[0xe53f, 0x0103]);
        arm(&mut c);
        c.r[0] = 1.0f32.to_bits();
        c.r[1] = divisor.to_bits();
        c.step().unwrap();
        assert_trapped(&c, XIP);
        assert_eq!(c.r[0], 1.0f32.to_bits(), "the destination is not written");
    }
}

#[test]
fn float_divide_by_zero_with_the_trap_off_gives_the_ieee_value() {
    // X0X clears EMU_CON bit 2 and discards hoisted 1 / 0 quotients.
    let mut c = cpu(&[0xe53f, 0x0103]);
    c.r[0] = (-2.0f32).to_bits();
    c.r[1] = 0;
    c.step().unwrap();
    assert_eq!(c.r[0], f32::NEG_INFINITY.to_bits());
    assert_eq!(c.pc, XIP + 4);
    assert_eq!(c.exception_entries, 0);
    // Other exceptional results still fault.
    let mut c = cpu(&[0xe53f, 0x0102]); // r0 = r0 * r1 (f)
    c.r[0] = f32::MAX.to_bits();
    c.r[1] = 2.0f32.to_bits();
    assert!(c.step().is_err());
}

#[test]
fn integer_divides_by_zero_trap_when_armed_and_fault_otherwise() {
    for words in [[0xe1f4u16, 0x0120], [0xe1f4, 0x0121], [0xe1f6, 0x0040]] {
        let mut c = cpu(&words);
        arm(&mut c);
        c.r[2] = 1234;
        c.r[4] = 99;
        c.step().unwrap();
        assert_trapped(&c, XIP);
        let mut unarmed = cpu(&words);
        unarmed.r[2] = 1234;
        unarmed.r[4] = 99;
        if words[0] == 0xe1f6 {
            // Measured on an FM-1 (upstream 1875a54): the 64-bit divide of
            // zero gives 0.
            unarmed.step().unwrap();
            assert_eq!((unarmed.r[0], unarmed.r[1]), (0, 0));
        } else {
            assert!(
                unarmed.step().is_err(),
                "unarmed: the quotient is unmeasured"
            );
        }
    }
}

#[test]
fn the_trap_is_taken_inside_a_repeat_block() {
    // rep 3 { r0 = r0 / r1 (f) }: an interrupt would wait for the block.
    let mut c = cpu(&[0x8210, 0xe53f, 0x0103, 0x0000]);
    arm(&mut c);
    c.r[0] = 1.0f32.to_bits();
    c.step().unwrap();
    c.step().unwrap();
    assert_trapped(&c, XIP + 2);
    for _ in 0..4 {
        c.step().unwrap();
    }
    assert_eq!(
        c.pc,
        HANDLER + 8,
        "the repeat does not loop over the handler"
    );
}

#[test]
fn an_undeliverable_trap_stops_and_each_core_has_its_own_bank() {
    let mut c = cpu(&[0xe53f, 0x0103]);
    c.sr[11] |= 0x100;
    c.bus.write(EMU_CON, 4, 4).unwrap();
    assert!(c.step().is_err(), "vector 1 disabled");
    assert_eq!(c.bus.read(EMU_MSG, 4).unwrap() & 4, 4);
    c.bus.write(EMU_MSG, u32::MAX, 4).unwrap();
    assert_eq!(c.bus.read(EMU_MSG, 4).unwrap(), 0, "write one to clear");
    // Core 1 (cnum = 1) uses the bank 0x200 higher: core 0's enable does not
    // arm it.
    let mut c = cpu(&[0xe53f, 0x0103]);
    arm(&mut c);
    c.sr[6] = 1;
    c.bus.write(ICFG00 + 0x200, 0xf0, 4).unwrap();
    c.r[0] = 1.0f32.to_bits();
    c.step().unwrap();
    assert_eq!(c.r[0], f32::INFINITY.to_bits());
    assert_eq!(c.exception_entries, 0);
}
