// SPDX-License-Identifier: GPL-3.0-only
// 64-bit divide and multiply-accumulate facts measured on a real FM-1 by the
// upstream QEMU emulator (simonjohansson/fm1-emulator 1875a54).
use fm1_emu::{bus::Bus, cpu::Cpu, XIP};

fn cpu(words: &[u16]) -> Cpu {
    Cpu::new(
        Bus::new(words.iter().flat_map(|w| w.to_le_bytes()).collect()).unwrap(),
        XIP,
    )
}

fn pair(c: &Cpu, low: usize) -> u64 {
    c.r[low] as u64 | (c.r[low + 1] as u64) << 32
}

#[test]
fn signed_long_divide_matches_the_fm1() {
    // E1F6 1200: r1_r0 = r1_r0 / r2 (s). Truncates toward zero, divides by
    // zero to 0 and wraps INT64_MIN / -1.
    for (dividend, divisor, quotient) in [
        (-7i64, 2i32, -3i64),
        (7, -2, -3),
        (-7, -2, 3),
        (7, 0, 0),
        (1_000_000_000_000, 3, 333_333_333_333),
        (-1_000_000_000_000, 7, -142_857_142_857),
        (i64::MIN, -1, i64::MIN),
    ] {
        let mut c = cpu(&[0xe1f6, 0x1200]);
        c.r[0] = dividend as u32;
        c.r[1] = (dividend >> 32) as u32;
        c.r[2] = divisor as u32;
        c.sr[5] = 0xf;
        c.step().unwrap_or_else(|e| panic!("{dividend}/{divisor}: {e:?}"));
        assert_eq!(pair(&c, 0) as i64, quotient, "{dividend} / {divisor}");
    }
}

#[test]
fn unsigned_long_divide_by_zero_is_zero() {
    let mut c = cpu(&[0xe1f6, 0x0200]);
    c.r[0] = 5;
    c.r[1] = 1;
    c.r[2] = 0;
    c.step().unwrap();
    assert_eq!(pair(&c, 0), 0);
}

#[test]
fn long_multiply_accumulate_sets_only_the_64_bit_carry() {
    // E1FC xx60 (r11_r10 += r6 * r14), PSR preset to 0x8000000f: only C
    // changes, to the carry out of the 64-bit addition (upstream 1875a54).
    let m = u64::MAX;
    for (signed, accumulator, left, right, result, carry) in [
        (true, 0u64, -1i32, 1i32, m, false),
        (true, (1 << 63) - 1, 1, 1, 1 << 63, false),
        (true, m, 1, 1, 0, true),
        (true, 5, -2, 3, m, false),
        (true, 0, i32::MIN, i32::MIN, 1 << 62, false),
        (true, 1 << 63, -1, 1, (1 << 63) - 1, true),
        (true, 1, -1, -1, 2, false),
        (false, 0x2_0000_0001, 3, 4, 0x2_0000_000d, false),
        (false, m, 1, 1, 0, true),
    ] {
        let mut c = cpu(&[0xe1fc, if signed { 0xbe60 } else { 0xae60 }]);
        c.r[10] = accumulator as u32;
        c.r[11] = (accumulator >> 32) as u32;
        c.r[6] = left as u32;
        c.r[14] = right as u32;
        c.sr[5] = 0x8000_000f;
        c.step().unwrap();
        assert_eq!(pair(&c, 10), result, "{accumulator:#x} + {left} * {right}");
        let expected = 0x8000_000d | if carry { 2 } else { 0 };
        assert_eq!(c.sr[5], expected, "flags for {accumulator:#x} + {left} * {right}");
    }
}

