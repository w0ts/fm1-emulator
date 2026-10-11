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
