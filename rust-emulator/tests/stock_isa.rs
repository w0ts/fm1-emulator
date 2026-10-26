// SPDX-License-Identifier: GPL-3.0-only
// Instruction forms first reached by the stock FM-1_015 and Baud Girl FM-1_093
// applications. Each test encodes the exact halfwords seen in those images.
use fm1_emu::{bus::Bus, cpu::Cpu, RAM, XIP};

fn cpu(words: &[u16]) -> Cpu {
    Cpu::new(
        Bus::new(words.iter().flat_map(|w| w.to_le_bytes()).collect()).unwrap(),
        XIP,
    )
}

#[test]
fn byte_load_with_negative_nine_bit_offset_reads_below_the_base() {
    // 0x020035c4: r3 = b[r3 + -18] (u), bytes 51 ee 3e 3e.
    let mut c = cpu(&[0xee51, 0x3e3e]);
    c.r[3] = RAM + 0x40;
    c.bus.write(RAM + 0x40 - 18, 0xa5, 1).unwrap();
    c.bus
        .write(RAM + 0x40 + 0x1ee - 0x200 + 1, 0x11, 1)
        .unwrap();
    c.step().unwrap();
    assert_eq!(c.r[3], 0xa5);
    assert_eq!(c.pc, XIP + 4);
}

#[test]
fn byte_load_sign_and_store_variants_share_the_negative_offset() {
    let mut c = cpu(&[0xee55, 0x2e3e]); // r2 = b[r3 + -18] (s)
    c.r[3] = RAM + 0x40;
    c.bus.write(RAM + 0x40 - 18, 0x80, 1).unwrap();
    c.step().unwrap();
    assert_eq!(c.r[2], 0xffff_ff80);
    let mut c = cpu(&[0xee53, 0x2e3e]); // b[r3 + -18] = r2
    c.r[3] = RAM + 0x40;
    c.r[2] = 0x1234_5677;
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x40 - 18, 1).unwrap(), 0x77);
    assert_eq!(c.r[3], RAM + 0x40);
    let mut c = cpu(&[0xee59, 0x2e3e]); // r2 = b[++r3 = -18]
    c.r[3] = RAM + 0x40;
    c.bus.write(RAM + 0x40 - 18, 0x42, 1).unwrap();
    c.step().unwrap();
    assert_eq!(c.r[2], 0x42);
    assert_eq!(c.r[3], RAM + 0x40 - 18);
}

#[test]
fn word_postincrement_load_advances_by_the_encoded_immediate() {
    // 0x020347fc: r0 = [r4++=28], bytes d8 ec 4c 01 (device table walk).
    let mut c = cpu(&[0xecd8, 0x014c]);
    c.r[4] = RAM + 0x100;
    c.r[1] = 0x55; // not an index register in this form
    c.bus.write(RAM + 0x100, 0x0203_4b86, 4).unwrap();
    c.step().unwrap();
    assert_eq!(c.r[0], 0x0203_4b86);
    assert_eq!(c.r[4], RAM + 0x100 + 28);
    assert_eq!(c.pc, XIP + 4);
}

#[test]
fn word_indexed_load_and_store_keep_their_register_index() {
    let mut c = cpu(&[0xecd8, 0x214a, 0xecd8, 0x214b]); // r2=[r4+r1<<2]; [r4+r1<<2]=r2
    c.r[4] = RAM + 0x100;
    c.r[1] = 3;
    c.bus.write(RAM + 0x10c, 0xdead_beef, 4).unwrap();
    c.step().unwrap();
    assert_eq!(c.r[2], 0xdead_beef);
    assert_eq!(c.r[4], RAM + 0x100);
    c.r[2] = 0x1234_5678;
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x10c, 4).unwrap(), 0x1234_5678);
}

#[test]
fn long_multiply_low_partial_product_is_unsigned_by_default() {
    // 0x020021da: r1:r0 = r0 * r1, bytes f8 e1 10 00.
    let mut c = cpu(&[0xe1f8, 0x0010]);
    c.r[0] = 0xffff_fffe;
    c.r[1] = 3;
    c.step().unwrap();
    assert_eq!((c.r[0], c.r[1]), (0xffff_fffa, 2));
    let mut c = cpu(&[0xe1f8, 0x1010]); // bit 12: signed
    c.r[0] = 0xffff_fffe; // -2
    c.r[1] = 3;
    c.step().unwrap();
    assert_eq!((c.r[0], c.r[1]), (0xffff_fffa, 0xffff_ffff));
    let mut c = cpu(&[0xe1f8, 0x6420]); // r7:r6 = r2 * r4
    c.r[2] = 0x1234_5678;
    c.r[4] = 1_000_000;
    c.step().unwrap();
    let product = 0x1234_5678u64 * 1_000_000;
    assert_eq!((c.r[6], c.r[7]), (product as u32, (product >> 32) as u32));
}

#[test]
fn long_divide_and_carry_chain_rebuild_a_64_bit_remainder() {
    // Stock sequence at 0x020021e4..0x020021f8 (microsecond conversion):
    // r3:r2 = r1:r0 / r4; r5 = r3*r4; r7:r6 = r2*r4; r7 += r5;
    // r4 = r0 - r6; r5 = r1 - r7 - !C.
    let n: u64 = 0x0000_0123_8765_4321;
    let mut c = cpu(&[
        0xe1f6, 0x2400, 0xe1f0, 0x5430, 0xe1f8, 0x6420, 0x1857, 0x1f84, 0xe0b8, 0x5712,
    ]);
    c.r[0] = n as u32;
    c.r[1] = (n >> 32) as u32;
    c.r[4] = 1_000_000;
    for _ in 0..6 {
        c.step().unwrap();
    }
    assert_eq!(c.r[2] as u64 | (c.r[3] as u64) << 32, n / 1_000_000);
    assert_eq!(c.r[4] as u64 | (c.r[5] as u64) << 32, n % 1_000_000);
}

#[test]
fn add_with_carry_propagates_the_low_word_carry() {
    let mut c = cpu(&[0x1c10, 0xe0b8, 0x5230]); // r0 = r1 + r0 ; r5 = r3 + r2 + C
    c.r[0] = 0xffff_ffff;
    c.r[1] = 2;
    c.r[2] = 7;
    c.r[3] = 1;
    c.step().unwrap();
    c.step().unwrap();
    assert_eq!((c.r[0], c.r[5]), (1, 9));
}

#[test]
fn subtract_with_carry_sets_n_z_v_for_signed_64_bit_compares() {
    // SLOOP sy_at (clock_sync.c), a signed 64-bit min: r4 = r6 - r2;
    // r5 = r7 - r3 - !C; then N != V (PSR bits 3 and 0) is "less than".
    for (lhs, rhs, n, z, v) in [
        (5i64, 9i64, true, false, false),
        (9, 5, false, true, false), // high word 0 - 0: Z of that word
        (i64::MIN, 1, false, false, true),
        (-3, -3, false, true, false),
    ] {
        let mut c = cpu(&[0x1ee4, 0xe0b8, 0x5372]);
        (c.r[6], c.r[7]) = (lhs as u32, (lhs >> 32) as u32);
        (c.r[2], c.r[3]) = (rhs as u32, (rhs >> 32) as u32);
        c.step().unwrap();
        c.step().unwrap();
        let d = lhs.wrapping_sub(rhs);
        assert_eq!((c.r[4], c.r[5]), (d as u32, (d >> 32) as u32));
        let psr = c.sr[5];
        assert_eq!(
            (psr & 8 != 0, psr & 4 != 0, psr & 1 != 0),
            (n, z, v),
            "{lhs} - {rhs}"
        );
        assert_eq!((psr & 8 != 0) != (psr & 1 != 0), lhs < rhs);
    }
}

#[test]
fn stack_adjust_by_signed_thirteen_bit_immediate_allocates_and_frees_frames() {
    // Stock FM-1_015 0x0200c97c prologue: push, then e8f0 1d98 (sp -= 616);
    // its epilogues e8f0 0268 (sp += 616), then pop. Jangada 0x0200de16:
    // e8f0 1cc8 (sp -= 824).
    for (operand, delta) in [(0x1d98u16, -616i32), (0x0268, 616), (0x1cc8, -824)] {
        let mut c = cpu(&[0xe8f0, operand]);
        c.sr[14] = RAM + 0x4000;
        let before = c.r;
        c.step().unwrap();
        assert_eq!(c.sr[14], (RAM + 0x4000).wrapping_add(delta as u32));
        assert_eq!(c.r, before);
        assert_eq!(c.pc, XIP + 4);
    }
}

#[test]
fn unsigned_compare_branches_take_an_unsigned_ten_bit_immediate() {
    // SLOOP 2.2 fm1_delay_us at 0x02017002: f9f1 81fc = jb r1, #960, pc - 4
    // (loop while elapsed TIMER4 ticks < 960). Sign-extending made it
    // r1 < 0xffffffc0: a wait that never ended.
    for (elapsed, taken) in [(959u32, true), (960, false), (49_799_320, false)] {
        let mut c = cpu(&[0xf9f1, 0x81fc]);
        c.r[1] = elapsed;
        c.step().unwrap();
        assert_eq!(
            c.pc,
            if taken { XIP - 4 } else { XIP + 4 },
            "r1 = {elapsed}"
        );
    }
}

#[test]
fn equality_compare_branches_sign_extend_their_immediate() {
    // Baud Girl FM-1_093 0x02001820: f874 fc04 = je r?, #-2, pc + 8 with the
    // register holding 0xfffffffe; an unsigned imm10 (1022) sent the boot
    // down a path that read address 0.
    let mut c = cpu(&[0xf874, 0xfc04]);
    let n = (0xf874 & 15) as usize;
    c.r[n] = 0xffff_fffe;
    c.step().unwrap();
    assert_eq!(c.pc, XIP + 4 + 8);
}

#[test]
fn register_repeat_runs_its_block_count_times_even_when_the_body_reuses_the_register() {
    // FM-1_093 memcpy tail at 0x02044596: 0312 0712 07b2 0456 =
    // rep 4 bytes, r2 { r2 = b[r1++]; b[r3++] = r2 }; pop pc. The count
    // register is the byte temporary inside the body and no branch follows,
    // so the count must be latched: sdfile copying "btif" (4 bytes) got only
    // "b" and the BTIF directory lookup failed.
    let mut c = cpu(&[0x0312, 0x0712, 0x07b2, 0x0000]);
    c.r[1] = RAM;
    c.r[2] = 4;
    c.r[3] = RAM + 0x40;
    for (i, byte) in b"btif".iter().enumerate() {
        c.bus.write(RAM + i as u32, *byte as u32, 1).unwrap();
    }
    while c.pc != XIP + 6 {
        c.step().unwrap();
        assert!(c.steps <= 10);
    }
    for (i, byte) in b"btif".iter().enumerate() {
        assert_eq!(c.bus.read(RAM + 0x40 + i as u32, 1).unwrap(), *byte as u32);
    }
    assert_eq!(c.bus.read(RAM + 0x44, 1).unwrap(), 0);
    assert_eq!(c.r[1], RAM + 4);
    assert_eq!(c.r[3], RAM + 0x44);
}

#[test]
fn signed_greater_than_register_conditional_guards_its_then_block() {
    // FM-1_093 0x02002bea: ee15 4200 ed50 5097 1652 =
    // if (r5 > r2) { h[r9+6] = r5; mov } with r2 = h[r9+6] (s) = -1 just
    // loaded. Kind 0xe1 is the register form of the signed greater-than
    // test, matching the compare-branch family where 0xee00 is signed >.
    for (r5, taken) in [(3u32, true), (0xffff_fffe, false), (0xffff_ffff, false)] {
        let mut c = cpu(&[0xee15, 0x4200, 0xed50, 0x5097, 0x1652, 0x0000]);
        c.r[2] = 0xffff_ffff;
        c.r[5] = r5;
        c.r[9] = RAM;
        c.bus.write(RAM + 6, 0x7777, 2).unwrap();
        while c.pc < XIP + 10 {
            c.step().unwrap();
            assert!(c.steps <= 3);
        }
        assert_eq!(c.pc, XIP + 10, "r5 = {r5:#x}");
        let stored = c.bus.read(RAM + 6, 2).unwrap();
        assert_eq!(
            stored,
            if taken { r5 & 0xffff } else { 0x7777 },
            "r5 = {r5:#x}"
        );
    }
}

#[test]
fn special_register_pop_with_only_the_rets_bit_restores_rets() {
    // FM-1_093 variadic wrapper at 0x0200322a: push {r3..r0}; push rets; ...
    // call; sp += 16; 0488; sp += 16; rts. 0x0480 | mask pops special
    // registers by bitmap (0x04a9 = {psr, rets, reti}: bits 5, 3, 0), so
    // 0x0488 is pop {rets}.
    let mut c = cpu(&[0x0488, 0x0000]);
    c.sr[14] = RAM + 0x100;
    c.sr[5] = 0x55;
    c.bus.write(RAM + 0x100, 0x0200_3334, 4).unwrap();
    c.step().unwrap();
    assert_eq!(c.sr[3], 0x0200_3334);
    assert_eq!(c.sr[14], RAM + 0x104);
    assert_eq!(c.sr[5], 0x55);
    assert_eq!(c.pc, XIP + 2);
}

#[test]
fn register_list_store_puts_the_lowest_register_at_the_lowest_address() {
    // FM-1_093 list_add_tail at 0x02003338: eb20 0006 = [r0] = {r2, r1} with
    // r1 = head (new->next, offset 0) and r2 = old tail (new->prev,
    // offset 4). Storing r2 first linked the second entry to the first in a
    // loop that lost the head; list removal then spun until the watchdog.
    let mut c = cpu(&[0xeb20, 0x0006]);
    c.r[0] = RAM + 0x20;
    c.r[1] = 0x01c0_92c8;
    c.r[2] = 0x01c2_4b20;
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x20, 4).unwrap(), 0x01c0_92c8);
    assert_eq!(c.bus.read(RAM + 0x24, 4).unwrap(), 0x01c2_4b20);
    assert_eq!(c.r[0], RAM + 0x20);
}

#[test]
fn register_list_load_fills_the_lowest_register_from_the_lowest_address() {
    // Vendor-compiled Felucca fm1_fault_c (fm1_irq.h) at 0x020006f2:
    // eb04 0022 = {r5, r1} = [r4+] reads fm1_crash.magic into r1 (compared
    // with "CRSH") and fm1_crash.count into r5 (incremented).
    let mut c = cpu(&[0xeb04, 0x0022]);
    c.r[4] = RAM + 0x40;
    c.bus.write(RAM + 0x40, 0x4352_5348, 4).unwrap();
    c.bus.write(RAM + 0x44, 7, 4).unwrap();
    c.step().unwrap();
    assert_eq!(c.r[1], 0x4352_5348);
    assert_eq!(c.r[5], 7);
    assert_eq!(c.r[4], RAM + 0x40);
}

#[test]
fn pair_shift_moves_the_high_word_down_for_a_forty_bit_field() {
    // FM-1_093 0x0207493e: f1d0 2a00 || 4489, then 478a. Bytes 3..6 of a
    // record come from r2 and byte 7 from r2 after r3:r2 >>= 32 (r3 holds
    // the byte-7 value read earlier), so the shift works on the pair.
    let mut c = cpu(&[0xe1d0, 0x2a00]);
    c.r[2] = 0x2308_1811;
    c.r[3] = 0x0000_00a5;
    c.step().unwrap();
    assert_eq!(c.r[2], 0xa5);
    assert_eq!(c.r[3], 0);
    assert_eq!(c.pc, XIP + 4);
}

#[test]
fn pair_shifts_sign_extend_and_scale_sixty_four_bit_products() {
    // Vendor-compiled Felucca 0x020102a8: e1d0 4200; e1d0 4e00 is
    // (int64)r4 in r5:r4 (shift left 32, arithmetic right 32), and
    // e1d0 490e after a signed 64-bit multiply is the Q30 >> 30.
    let mut c = cpu(&[0xe1d0, 0x4200, 0xe1d0, 0x4e00]);
    c.r[4] = 0xffff_fff0;
    c.r[5] = 0x1234_5678;
    c.step().unwrap();
    assert_eq!((c.r[5], c.r[4]), (0xffff_fff0, 0));
    c.step().unwrap();
    assert_eq!((c.r[5], c.r[4]), (0xffff_ffff, 0xffff_fff0));
    let mut c = cpu(&[0xe1d0, 0x490e]);
    let product = (0x3000_0000i64 * 0x2000_0000i64) as u64;
    c.r[4] = product as u32;
    c.r[5] = (product >> 32) as u32;
    c.step().unwrap();
    assert_eq!(c.r[4], (product >> 30) as u32);
    assert_eq!(c.r[5], (product >> 62) as u32);
}

#[test]
fn pair_rotate_right_is_xxh64s_rotate_left() {
    // Optimist's SD build (storage/sd/sd_hash.c, XXH64): e1d4 4201 is r5_r4 <>= 33, a rotate right of
    // the pair: its rotl(x, 31). The immediate is encoded as e1d0's.
    let mut c = cpu(&[0xe1d4, 0x4201, 0xe1d4, 0x830f]);
    let x: u64 = 0x9E37_79B1_85EB_CA87;
    c.r[4] = x as u32;
    c.r[5] = (x >> 32) as u32;
    c.step().unwrap();
    assert_eq!((c.r[5] as u64) << 32 | c.r[4] as u64, x.rotate_left(31));
    c.r[8] = x as u32;
    c.r[9] = (x >> 32) as u32;
    c.step().unwrap();
    assert_eq!(
        (c.r[9] as u64) << 32 | c.r[8] as u64,
        x.rotate_right(63),
        "r9_r8 <>= 63: rotl 1"
    );
    assert_eq!(c.pc, XIP + 8);
}

#[test]
fn pair_shift_by_register_normalises_a_double_mantissa() {
    // FM-1_093 unsigned-to-double at 0x02045210: e1d8 4700 = r5:r4 <<= r7
    // with r7 = 52 - (bit length 6 of 48) + 1 = 47, after which
    // e100 63ff adds the 1023 exponent bias and e1d0 0304 does r1:r0 <<= 52.
    let mut c = cpu(&[0xe1d8, 0x4700]);
    c.r[4] = 48;
    c.r[5] = 0;
    c.r[7] = 47;
    c.step().unwrap();
    assert_eq!(((c.r[5] as u64) << 32) | c.r[4] as u64, 48u64 << 47);
    let mut c = cpu(&[0xe1d8, 0x4702]); // r5:r4 >>= r7 (logical)
    c.r[5] = 0x8000_0000;
    c.r[7] = 36;
    c.step().unwrap();
    assert_eq!((c.r[5], c.r[4]), (0, 0x0800_0000));
}

#[test]
fn long_multiply_accumulate_adds_the_product_to_the_pair() {
    // e1fc follows the e1f8 field layout (pair rD&14, x bit 12 signed) and
    // accumulates. Felucca's DSP chains e1fc bde0 (signed, r11:r10 +=
    // r14 * r13) before a Q30 e1d0 490e; FM-1_093's soft-double multiply
    // at 0x02044f5c uses the unsigned e1fc ae60 (r11:r10 += r6 * r14).
    let mut c = cpu(&[0xe1fc, 0xbde0]);
    c.r[10] = 0xffff_fff0;
    c.r[11] = 0;
    c.r[13] = 3;
    c.r[14] = 0xffff_fffe; // -2
    c.step().unwrap();
    let expected = 0xffff_fff0u64.wrapping_add((-6i64) as u64);
    assert_eq!(
        (c.r[11], c.r[10]),
        ((expected >> 32) as u32, expected as u32)
    );
    let mut c = cpu(&[0xe1fc, 0xae60]);
    c.r[10] = 0xffff_ffff;
    c.r[11] = 1;
    c.r[6] = 0xffff_ffff;
    c.r[14] = 2;
    c.step().unwrap();
    let expected = 0x1_ffff_ffffu64 + 0xffff_ffffu64 * 2;
    assert_eq!(
        (c.r[11], c.r[10]),
        ((expected >> 32) as u32, expected as u32)
    );
}

#[test]
fn memory_shift_high_form_sign_extends_ten_bit_rf_fields() {
    // FM-1_093 btctrler at 0x0205d304: each word of the record at r1 gets
    // bits 24-31 (e1a0 4c20) and 22-23 (e1a4 0b08) from RF registers, then
    // e86d 1602 / 1607 = [r1+0] >>= 22 (logical) / [r1+4] >>= 22
    // (arithmetic). h bit 0 is bit 4 of the shift amount (16 + 6); x bits
    // 0-1 select the shift as in e1c8. e86c keeps amounts 0-15: Felucca's
    // audio_block `out[2i + 1] <<= OUT_SHIFT` (7) is e86c 3704.
    let mut c = cpu(&[0xe86d, 0x1602, 0xe86d, 0x1607, 0xe86c, 0x3704]);
    c.r[1] = RAM;
    c.r[3] = RAM + 0x40;
    c.bus.write(RAM, 0xffc0_0000, 4).unwrap();
    c.bus.write(RAM + 4, 0xffc0_0000, 4).unwrap();
    c.bus.write(RAM + 0x44, 3, 4).unwrap();
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM, 4).unwrap(), 0x3ff);
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 4, 4).unwrap(), 0xffff_ffff);
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x44, 4).unwrap(), 3 << 7);
    assert_eq!(c.pc, XIP + 12);
}

#[test]
fn signed_greater_than_immediate_conditional_keeps_non_negative_bytes() {
    // FM-1_093 0x02004872: ee37 5fff 4cbf 21c5 4cb9 =
    // if (r7 > -1) { b[r3+12] = r7; r5 += 1 } else { b[r3+12] = r1 }
    // after r7 = b[r3+1] (signed). Kind 0xe3: signed >, signed imm12.
    for (r7, stored, count) in [(5u32, 5u32, 1u32), (0, 0, 1), (0xffff_ff80, 0xff, 0)] {
        let mut c = cpu(&[0xee37, 0x5fff, 0x4cbf, 0x21c5, 0x4cb9, 0x0000]);
        c.r[1] = 0xff;
        c.r[3] = RAM;
        c.r[7] = r7;
        // Taken: the skip over the else slot happens on the next step, which
        // then also runs the trailing nop.
        while c.pc < XIP + 10 {
            c.step().unwrap();
            assert!(c.steps <= 4);
        }
        assert_eq!(c.bus.read(RAM + 12, 1).unwrap(), stored, "r7 = {r7:#x}");
        assert_eq!(c.r[5], count, "r7 = {r7:#x}");
    }
}

fn f(value: f32) -> u32 {
    value.to_bits()
}

#[test]
fn single_float_arithmetic_follows_the_sdk_library_patterns() {
    // e53f x = d c s op: rD = rS op rC. AC79 SDK libVolcEngineRTCLite.a
    // (fprev1 ELF objects): rx_net_samples_avg is r1 = (float)r1
    // (e53f 119f); r0 = r0 / r1 (e53f 0103). Stock FM-1_093 clamps with
    // r0 = max(r0, r3); r4 = 100.0f; r0 = min(r0, r4) (e53f 0306, 0405 at
    // 0x0201ac16), and its complex multiply (0x0208bd30) is r8 = r4*r6;
    // r9 = r6*r5; r8 -= r5*r7 (e53f 8758); r9 += r4*r7 (e53f 9747).
    let mut c = cpu(&[0xe53f, 0x119f, 0xe53f, 0x0103]);
    c.r[0] = f(10.0);
    c.r[1] = 4;
    c.step().unwrap();
    assert_eq!(c.r[1], f(4.0));
    c.step().unwrap();
    assert_eq!(c.r[0], f(2.5));
    for (x, clamped) in [(-3.0f32, 0.0f32), (42.5, 42.5), (250.0, 100.0)] {
        let mut c = cpu(&[0xe53f, 0x0306, 0xe53f, 0x0405]);
        c.r[0] = f(x);
        c.r[3] = f(0.0);
        c.r[4] = f(100.0);
        c.step().unwrap();
        c.step().unwrap();
        assert_eq!(c.r[0], f(clamped), "x = {x}");
    }
    let (a, b, cc, d) = (1.5f32, -2.0f32, 0.5f32, 3.0f32);
    let mut c = cpu(&[
        0xe53f, 0x8642, 0xe53f, 0x9562, 0xe53f, 0x8758, 0xe53f, 0x9747,
    ]);
    c.r[4] = f(a);
    c.r[5] = f(b);
    c.r[6] = f(cc);
    c.r[7] = f(d);
    for _ in 0..4 {
        c.step().unwrap();
    }
    assert_eq!(c.r[8], f(a * cc - b * d));
    assert_eq!(c.r[9], f(b * cc + a * d));
    let mut c = cpu(&[0xe53f, 0x0101, 0xe53f, 0x0100]); // r0 = r0 - r1; r0 = r0 + r1
    c.r[0] = f(1.0);
    c.r[1] = f(0.25);
    c.step().unwrap();
    assert_eq!(c.r[0], f(0.75));
    c.step().unwrap();
    assert_eq!(c.r[0], f(1.0));
}

#[test]
fn float_integer_conversions_select_signedness_by_sub_operation() {
    // 0x8f signed and 0x9f unsigned int -> float (stock 0x02004ade converts
    // the result of an unsigned min with 9f); 0x1f float -> int, truncating.
    let mut c = cpu(&[0xe53f, 0x008f, 0xe53f, 0x119f, 0xe53f, 0x221f]);
    c.r[0] = 0xffff_fffe;
    c.r[1] = 0xffff_fffe;
    c.r[2] = f(-7.75);
    c.step().unwrap();
    c.step().unwrap();
    c.step().unwrap();
    assert_eq!(c.r[0], f(-2.0));
    assert_eq!(c.r[1], f(4294967294.0));
    assert_eq!(c.r[2], (-7i32) as u32);
}

#[test]
fn masked_push_and_pop_with_bit_zero_save_rets_and_return() {
    // FM-1_093 0x02074b18: e8d9 0df0 opens a function whose last
    // instruction is e8d5 0df0 (0x02074ba2, the next function follows), so
    // h bit 0 adds rets to the push and pc to the pop, like push/pop
    // {rets, r4..rN} (0x047n / 0x045n).
    let mut c = cpu(&[0xe8d9, 0x0df0, 0xe8d5, 0x0df0]);
    c.sr[14] = RAM + 0x100;
    c.sr[3] = XIP + 0x40;
    for n in 0..16 {
        c.r[n] = 0x100 + n as u32;
    }
    c.step().unwrap();
    assert_eq!(c.sr[14], RAM + 0x100 - 4 * 8);
    assert_eq!(c.bus.read(RAM + 0xfc, 4).unwrap(), XIP + 0x40);
    assert_eq!(c.bus.read(RAM + 0xe0, 4).unwrap(), 0x104);
    for n in [4, 5, 6, 7, 8, 10, 11] {
        c.r[n] = 0;
    }
    c.step().unwrap();
    assert_eq!(c.pc, XIP + 0x40);
    assert_eq!(c.sr[14], RAM + 0x100);
    for n in [4, 5, 6, 7, 8, 10, 11] {
        assert_eq!(c.r[n], 0x100 + n as u32);
    }
}

#[test]
fn special_register_frame_without_rets_saves_psr_and_reti() {
    // FM-1_093 SPI interrupt handler (flash 0x020887f0, run from RAM):
    // 04e1; e8d8 01ff; ... e8d4 01ff; 04a1; csync; rti. Bitmap 0x21 =
    // {reti (bit 0), psr (bit 5)}, the same layout as 04e9/04a9 without rets.
    let mut c = cpu(&[0x04e1, 0x04a1]);
    c.sr[14] = RAM + 0x100;
    c.sr[0] = 0x0200_1234;
    c.sr[3] = 0x0200_5678;
    c.sr[5] = 0x2;
    c.step().unwrap();
    assert_eq!(c.sr[14], RAM + 0xf8);
    assert_eq!(c.bus.read(RAM + 0xfc, 4).unwrap(), 0x2);
    assert_eq!(c.bus.read(RAM + 0xf8, 4).unwrap(), 0x0200_1234);
    c.sr[0] = 0;
    c.sr[5] = 0;
    c.step().unwrap();
    assert_eq!((c.sr[0], c.sr[3], c.sr[5]), (0x0200_1234, 0x0200_5678, 0x2));
    assert_eq!(c.sr[14], RAM + 0x100);
}

#[test]
fn memory_register_add_mode_two_subtracts() {
    // FM-1_093 cbuf_read at 0x0202834e: e868 6c1a; e868 6c16 =
    // cbuf->data_len -= n; cbuf->tmp_len -= n (SDK circular_buf.h:
    // tmp_len at +0x14, data_len at +0x18; Quarkslab pi32v2: 0x868 with
    // imm1617 = 2 is sub). Adding made the btstack task re-read the same
    // HCI message forever, starving usr_app_task (and the LCD).
    let mut c = cpu(&[0xe868, 0x6c1a, 0xe868, 0x6c16, 0xe868, 0x6c18]);
    c.r[6] = RAM;
    c.r[12] = 4;
    c.bus.write(RAM + 0x18, 0x2c, 4).unwrap();
    c.bus.write(RAM + 0x14, 0x30, 4).unwrap();
    c.step().unwrap();
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x18, 4).unwrap(), 0x28);
    assert_eq!(c.bus.read(RAM + 0x14, 4).unwrap(), 0x2c);
    c.step().unwrap(); // mode 0 still adds
    assert_eq!(c.bus.read(RAM + 0x18, 4).unwrap(), 0x2c);
}

#[test]
fn float_compare_branch_orders_singles() {
    // FM-1_093 0x02021d3c: ee81 e8a5 = if (r14 <= r1) goto +0x14a, float
    // (x bit 11), with r14 = 0.192f and r1 = 1.0f from the e53f add before.
    // Negative operands show the float ordering (integer order would invert).
    // JieLi objdump: ee81 e8a5 = iff (r14 u<= r1) goto 330. Unordered
    // (NaN) operands are unmeasured on hardware, so they fault explicitly
    // (upstream policy) instead of following the objdump's u prefix.
    let mut c = cpu(&[0xe801, 0x2804]);
    c.r[2] = f32::NAN.to_bits();
    c.r[1] = f32::NAN.to_bits();
    assert!(c.step().is_err());
    for (r14, r1, taken) in [
        (0.192f32, 1.0f32, true),
        (2.0, 1.0, false),
        (-2.0, -1.0, true),
    ] {
        let mut c = cpu(&[0xee81, 0xe8a5]);
        c.r[14] = r14.to_bits();
        c.r[1] = r1.to_bits();
        c.step().unwrap();
        assert_eq!(
            c.pc,
            if taken { XIP + 4 + 0x14a } else { XIP + 4 },
            "{r14} <= {r1}"
        );
    }
}

#[test]
fn idle_hint_continues_with_the_next_instruction() {
    // FM-1_093 IDLE0 task at 0x0205b8da: e930 0002 eabf e7d9 0001 ff80 ...
    // 0x0001 is asm("idle") (SDK init.c / adc_api.c wait-for-interrupt).
    let mut c = cpu(&[0x0001, 0x0000]);
    assert_eq!(c.step().unwrap(), "idle");
    assert_eq!(c.pc, XIP + 2);
}

#[test]
fn float_to_integer_conversions_follow_the_vendor_rounding_modes() {
    // JieLi objdump --mattr=+fprev1: e53f 0f12..3f12 = r1 = ftoi(r2)
    // (even/trunc/ceil/floor), 4f..7f = ftou(...), cf..ff = r1 = ftof(r2)
    // (even/trunc/ceil/floor). FM-1_093 0x0201e4a4: e53f 5f22 =
    // r2 = ftou(r2) (trunc) after r2 = r2 + 0.5f.
    let cases: [(u16, f32, u32); 12] = [
        (0x120f, 2.5, 2),
        (0x121f, -2.7, (-2i32) as u32),
        (0x122f, -2.3, (-2i32) as u32),
        (0x123f, -2.3, (-3i32) as u32),
        (0x124f, 3.5, 4),
        (0x125f, 7.9, 7),
        (0x126f, 7.1, 8),
        (0x127f, 7.9, 7),
        (0x12cf, 2.5, 2.0f32.to_bits()),
        (0x12df, -2.7, (-2.0f32).to_bits()),
        (0x12ef, 2.1, 3.0f32.to_bits()),
        (0x12ff, -2.1, (-3.0f32).to_bits()),
    ];
    for (x, input, expected) in cases {
        let mut c = cpu(&[0xe53f, x]);
        c.r[2] = input.to_bits();
        c.step().unwrap();
        assert_eq!(c.r[1], expected, "e53f {x:04x} on {input}");
    }
}

#[test]
fn half_precision_conversions_use_the_low_halfword() {
    // objdump: e53f af12 = r1.l = ftof(r2); e53f bf12 = r1 = ftof(r2.l).
    let mut c = cpu(&[0xe53f, 0x12af, 0xe53f, 0x31bf]);
    c.r[1] = 0xabcd_0000;
    c.r[2] = 1.5f32.to_bits();
    c.step().unwrap();
    assert_eq!(c.r[1], 0xabcd_3e00); // 1.5 as binary16 in the low half
    c.step().unwrap(); // r3 = ftof(r1.l)
    assert_eq!(c.r[3], 1.5f32.to_bits());
}

#[test]
fn signed_field_extract_sign_extends() {
    // JieLi objdump: e1b0 b041 = r0 = sextra(r11, p:0, l:16) (Baud Girl
    // text renderer 0x02012c5e); e1b1 0d0c = r1 = uextra(r0, p:26, l:3),
    // e1b1 0d0d = r1 = sextra(r0, p:26, l:3).
    let mut c = cpu(&[0xe1b0, 0xb041, 0xe1b1, 0x0d0c, 0xe1b1, 0x0d0d]);
    c.r[11] = 0x1234_fff4;
    c.step().unwrap();
    assert_eq!(c.r[0], 0xffff_fff4);
    c.r[0] = 0b101 << 26;
    c.step().unwrap();
    assert_eq!(c.r[1], 5);
    c.step().unwrap();
    assert_eq!(c.r[1], 0xffff_fffd);
}

#[test]
fn halfword_offset_forms_take_a_signed_ten_bit_offset() {
    // JieLi objdump: ed5b cf2a = r12 = h[++r2=-6] (u) (Baud Girl glyph
    // blender 0x02012788); ed59 0f2a = r0 = h[++r2=506] (u);
    // ed52 0f2a = r0 = h[r2+-262] (u); ed53 0f2b = h[r2+-6] = r0;
    // ed55 0f2b = h[r2+506] = r0.h (bit 2 stores the upper halfword).
    let mut c = cpu(&[
        0xed5b, 0xcf2a, 0xed59, 0x0f2a, 0xed52, 0x0f2a, 0xed53, 0x0f2b, 0xed55, 0x0f2b,
    ]);
    c.r[2] = RAM + 0x400;
    c.bus.write(RAM + 0x400 - 6, 0xbeef, 2).unwrap();
    c.step().unwrap();
    assert_eq!(c.r[12], 0xbeef);
    assert_eq!(c.r[2], RAM + 0x400 - 6);
    c.r[2] = RAM + 0x400;
    c.bus.write(RAM + 0x400 + 506, 0x1234, 2).unwrap();
    c.step().unwrap();
    assert_eq!((c.r[0], c.r[2]), (0x1234, RAM + 0x400 + 506));
    c.r[2] = RAM + 0x400;
    c.bus.write(RAM + 0x400 - 262, 0x5678, 2).unwrap();
    c.step().unwrap();
    assert_eq!((c.r[0], c.r[2]), (0x5678, RAM + 0x400));
    c.r[0] = 0xaaaa_5555;
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x400 - 6, 2).unwrap(), 0x5555);
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x400 + 506, 2).unwrap(), 0xaaaa);
    assert_eq!(c.r[2], RAM + 0x400);
}

#[test]
fn packed_immediates_repeat_bytes_as_the_vendor_objdump_shows() {
    // JieLi objdump: e1e0 0101 = r0 = r0 * 0x10001 (stock fill at
    // 0x02012c82 builds a two-pixel word: colour * 0x10001);
    // e1e0 01ab = * 0xAB00AB, e1e0 02ab = * 0xAB00AB00, e1e0 03ab =
    // * 0xABABABAB, e1e0 00ab = * 0xAB.
    for (x, factor) in [
        (0x0101u16, 0x0001_0001u32),
        (0x01ab, 0x00ab_00ab),
        (0x02ab, 0xab00_ab00),
        (0x03ab, 0xabab_abab),
        (0x00ab, 0xab),
    ] {
        let mut c = cpu(&[0xe1e0, x]);
        c.r[0] = 1;
        c.step().unwrap();
        assert_eq!(c.r[0], factor, "e1e0 {x:04x}");
    }
}

#[test]
fn register_repeat_with_a_zero_count_skips_its_block() {
    let mut c = cpu(&[0x0312, 0x0712, 0x07b2, 0x0000]);
    c.r[1] = RAM;
    c.r[3] = RAM + 0x40;
    c.step().unwrap();
    assert_eq!(c.pc, XIP + 6);
    assert_eq!(c.r[1], RAM);
}

#[test]
fn register_shift_covers_both_left_forms_and_arithmetic_right() {
    // Felucca 0.9-beta 0x02011b36 (a synth engine's DSP): e1c8 2253 =
    // r2 = r5 >>> r2 (arithmetic). Quarkslab pi32v2: imm1619 0 and 1 lsl,
    // 2 lsr, 3 asr.
    let mut c = cpu(&[0xe1c8, 0x2253]);
    c.r[5] = 0xffff_f000; // -4096
    c.r[2] = 4;
    c.step().unwrap();
    assert_eq!(c.r[2], 0xffff_ff00); // -256
    let mut c = cpu(&[0xe1c8, 0x2251]); // r2 = r5 << r2
    c.r[5] = 3;
    c.r[2] = 4;
    c.step().unwrap();
    assert_eq!(c.r[2], 48);
    let mut c = cpu(&[0xe1c8, 0x2253]); // shifts past 31 keep the sign
    c.r[5] = 0x8000_0000;
    c.r[2] = 40;
    c.step().unwrap();
    assert_eq!(c.r[2], 0xffff_ffff);
}

#[test]
fn register_list_load_puts_the_lowest_register_at_the_base() {
    // Felucca 0.9-beta 0x02011fcc (GRAIN, gr_run inlined): eb04 8004 loads
    // g->z (offset 0) and g->pos (offset 4) of a gr_grain_t; the next
    // instructions read z->n through r2. Descending order put pos in r2 and
    // faulted on the read of [pos + 4].
    let mut c = cpu(&[0xeb04, 0x8004]);
    c.r[4] = RAM;
    c.bus.write(RAM, 0x0201_2da4, 4).unwrap(); // z
    c.bus.write(RAM + 4, 0x256c, 4).unwrap(); // pos
    c.step().unwrap();
    assert_eq!(c.r[2], 0x0201_2da4);
    assert_eq!(c.r[15], 0x256c);
    assert_eq!(c.r[4], RAM);
    assert_eq!(c.pc, XIP + 4);
}

#[test]
fn register_list_store_and_load_keep_stock_linked_lists_consistent() {
    // Stock FM-1 0x02002284: list insertion before the head r1:
    // 6112 r2 = [r1+4]; 6190 [r1+4] = r0; eb20 0006 {r1, r2} -> [r0];
    // 60a0 [r2] = r0. Then 0x0201fb7a removes it again:
    // eb00 0006 {r1, r2} <- [r0]; 6192 [r1+4] = r2; 60a1 [r2] = r1.
    // Nodes are {next, prev}; the list starts as head <-> a.
    let (head, a, node) = (RAM + 0x100, RAM + 0x180, RAM + 0x200);
    let mut c = cpu(&[
        0x6112, 0x6190, 0xeb20, 0x0006, 0x60a0, 0xeb00, 0x0006, 0x6192, 0x60a1,
    ]);
    for (at, value) in [(head, a), (head + 4, a), (a, head), (a + 4, head)] {
        c.bus.write(at, value, 4).unwrap();
    }
    c.r[0] = node;
    c.r[1] = head;
    for _ in 0..4 {
        c.step().unwrap();
    }
    // head <-> a <-> node <-> head
    assert_eq!(c.bus.read(head + 4, 4).unwrap(), node); // head.prev
    assert_eq!(c.bus.read(a, 4).unwrap(), node); // a.next
    assert_eq!(c.bus.read(node, 4).unwrap(), head); // node.next
    assert_eq!(c.bus.read(node + 4, 4).unwrap(), a); // node.prev
    for _ in 0..3 {
        c.step().unwrap();
    }
    for (at, value) in [(head, a), (head + 4, a), (a, head), (a + 4, head)] {
        assert_eq!(c.bus.read(at, 4).unwrap(), value);
    }
}

#[test]
fn halfword_postincrement_with_low_bit_set_stores() {
    // Felucca 0.9-beta 0x02011e98 (GRAIN gr_fill: rb[q - lo] = pred):
    // edd0 10f3 = h[r15 ++= 2] = r1. As in the edd8 register forms, x bit 0
    // selects the store and the increment is even; reading it as a load
    // advanced r15 by 3 and faulted on the next unaligned access.
    let mut c = cpu(&[0xedd0, 0x10f3]);
    c.r[15] = RAM + 0x3a0;
    c.r[1] = 0xffff_9271;
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x3a0, 2).unwrap(), 0x9271);
    assert_eq!(c.r[15], RAM + 0x3a2);
    assert_eq!(c.r[1], 0xffff_9271);
    assert_eq!(c.pc, XIP + 4);
}

#[test]
fn pair_shift_by_immediate_shifts_the_64_bit_register_pair() {
    // Felucca 0.9-beta 0x02010462 (VOICE, eng_formant.c: f0 >> 4 with
    // f0 = ((uint64_t)inc * 705600) >> 32): e1d0 2a04 = r3:r2 >>= 36
    // (logical). Mode (x >> 10) & 3 as in e1c0: 0 lsl, 2 lsr, 3 asr; the
    // pair is x >> 12 (even), the count ((x >> 8) & 3) * 16 + (x & 15).
    let mut c = cpu(&[0xe1d0, 0x2a04]);
    c.r[2] = 0xa2dd_ec80;
    c.r[3] = 0x0000_125a;
    c.step().unwrap();
    assert_eq!((c.r[2], c.r[3]), (0x125, 0));
    assert_eq!(c.pc, XIP + 4);
    let mut c = cpu(&[0xe1d0, 0x4e00]); // r5:r4 >>>= 32 (Felucca 0x020102a4)
    c.r[4] = 0x1234_5678;
    c.r[5] = 0x8000_0001;
    c.step().unwrap();
    assert_eq!((c.r[4], c.r[5]), (0x8000_0001, 0xffff_ffff));
    let mut c = cpu(&[0xe1d0, 0x2002]); // r3:r2 <<= 2 (Felucca 0x02010294)
    c.r[2] = 0xc000_0001;
    c.r[3] = 1;
    c.step().unwrap();
    assert_eq!((c.r[2], c.r[3]), (4, 7));
}

#[test]
fn pair_store_with_low_bits_three_writes_the_base_back() {
    // Felucca 0.9-beta 0x02010ac0 (TRIO trio_note_on): ec50 2213 stores
    // v->ph[0], v->ph[1] = r2:r3 at [r1 + 0x20] and leaves r1 = v + 0x20;
    // the following stores (ec50 2019 at +8 = ph[2], 6490 at +16 = s[1])
    // and loads (6512 s[2], 6b12 age) only hit their fields from there.
    let mut c = cpu(&[0xec50, 0x2213, 0xec50, 0x2019]);
    c.r[1] = RAM + 0x100;
    c.r[2] = 0;
    c.r[3] = 0x1c00_0000;
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x120, 4).unwrap(), 0);
    assert_eq!(c.bus.read(RAM + 0x124, 4).unwrap(), 0x1c00_0000);
    assert_eq!(c.r[1], RAM + 0x120);
    c.r[2] = 0x3800_0000;
    c.r[3] = 0;
    c.bus.write(RAM + 0x12c, 0xffff_ffff, 4).unwrap();
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x128, 4).unwrap(), 0x3800_0000);
    assert_eq!(c.bus.read(RAM + 0x12c, 4).unwrap(), 0);
    assert_eq!(c.r[1], RAM + 0x120);
}

#[test]
fn multiply_accumulate_adds_the_signed_product_to_the_pair() {
    // Felucca 0.9-beta 0x020108cc (VOICE formant resonators, int64 sums):
    // e1fc 50a0 = r5:r4 += r10 * r0. x bit 12 set selects signed, as for
    // e1f8; the pair is (x >> 12) & 14.
    let mut c = cpu(&[0xe1fc, 0x50a0]);
    c.r[4] = 0x2000_0000;
    c.r[5] = 0;
    c.r[10] = 0xffff_fffe; // -2
    c.r[0] = 0x4000_0000;
    c.step().unwrap();
    // 0x20000000 - 0x80000000 = -0x60000000
    assert_eq!((c.r[4], c.r[5]), (0xa000_0000, 0xffff_ffff));
    assert_eq!(c.pc, XIP + 4);
}

#[test]
fn long_divide_with_bit_12_set_is_signed() {
    // Felucca 0.9-beta 0x02010524 / Jangada 0x020127be (eng_formant.c:
    // b = ((int64_t)b * (int32_t)(f0 + (f0 >> 4))) / f): e1f6 3620 =
    // r3:r2 = r3:r2 / r6, signed (bit 12, as for e1f8 and e1fc).
    let mut c = cpu(&[0xe1f6, 0x3620]);
    c.r[2] = 0xffff_ff00; // -256
    c.r[3] = 0xffff_ffff;
    c.r[6] = 16;
    c.step().unwrap();
    assert_eq!((c.r[2], c.r[3]), (0xffff_fff0, 0xffff_ffff)); // -16
    let mut c = cpu(&[0xe1f6, 0x3620]);
    c.r[2] = 0x0016_0434;
    c.r[3] = 0;
    c.r[6] = 0xffff_fffe; // -2
    c.step().unwrap();
    assert_eq!((c.r[2], c.r[3]), (0xfff4_fde6, 0xffff_ffff));
}

#[test]
fn byte_load_and_store_post_increment_by_a_register() {
    // Stock FM-1_015 / Baud Girl 0x0200de98: 13c0. The vendor objdump (JieLi
    // pi32v2 toolchain) prints it as "r0 = b[r4++=r15] (u)", and decodes the
    // whole 0x1000-0x13ff range as this family: bits 0-2 data register,
    // bit 3 store, bits 4-6 base, bits 7-9 increment register r8-r15
    // (all 1024 codes checked against objdump). "++=" is post-increment, as
    // its "h[r15++=2]" for edd0 (and "b[++r3=-18]" for the pre form ee59).
    let mut c = cpu(&[0x13c0]);
    c.r[4] = RAM + 0x40;
    c.r[15] = 3;
    c.bus.write(RAM + 0x40, 0x0a, 1).unwrap();
    c.bus.write(RAM + 0x43, 0x55, 1).unwrap();
    c.step().unwrap();
    assert_eq!(c.r[0], 0x0a); // read at the old r4
    assert_eq!(c.r[4], RAM + 0x43);
    assert_eq!(c.pc, XIP + 2);
    let mut c = cpu(&[0x13c8]); // b[r4++=r15] = r0
    c.r[4] = RAM + 0x40;
    c.r[15] = 1;
    c.r[0] = 0x1234_56fe;
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0x40, 1).unwrap(), 0xfe);
    assert_eq!(c.bus.read(RAM + 0x41, 1).unwrap(), 0);
    assert_eq!(c.r[4], RAM + 0x41);
}

#[test]
fn rotate_right_by_immediate() {
    // Stock / Baud Girl 0x0201eaa8 (printf's length-letter switch): r0 = c - 'h';
    // e1c4 0001; if (r0 > 9) ...; tbb [r0]. JieLi's objdump prints e1c4 0001 as
    // "r0 = r0 <> 1" and decodes rD = x >> 12, rS = (x >> 4) & 15, amount =
    // ((x >> 8) & 1) * 16 + (x & 15), 0 printed as 32. Right rotation is what
    // makes the switch work: odd offsets become huge and fail the range check.
    let mut c = cpu(&[0xe1c4, 0x0001]);
    c.r[0] = 4; // 'l' - 'h'
    c.step().unwrap();
    assert_eq!(c.r[0], 2);
    let mut c = cpu(&[0xe1c4, 0x0001]);
    c.r[0] = 3; // 'k' - 'h': not a case
    c.step().unwrap();
    assert_eq!(c.r[0], 0x8000_0001);
    let mut c = cpu(&[0xe1c4, 0x2101]); // r2 = r0 <> 17
    c.r[0] = 0x0001_0000; // bit 16 rotated right by 17 lands on bit 31
    c.step().unwrap();
    assert_eq!(c.r[2], 0x8000_0000);
    let mut c = cpu(&[0xe1c4, 0x0010]); // r0 = r1 <> 32
    c.r[1] = 0x1234_5678;
    c.step().unwrap();
    assert_eq!(c.r[0], 0x1234_5678);
    assert_eq!(c.pc, XIP + 4);
}

#[test]
fn memory_read_modify_write_offsets_are_signed() {
    // JieLi objdump: e868 12fc = [r1+-4] += r2, e86c 16fc = [r1+-4] <<= 6,
    // e866 12fc = [r1+-4] |= 1 << r2 (x bits 2-7 are a signed byte). ANALOG 2
    // SYNC SWEEP writes b[i - 1] this way; read as +252 it hit the caller.
    let mut c = cpu(&[0xe868, 0x12fc, 0xe86c, 0x16fc, 0xe866, 0x12fc]);
    c.r[1] = RAM + 0x100;
    c.r[2] = 3;
    c.bus.write(RAM + 0xfc, 5, 4).unwrap();
    c.bus.write(RAM + 0x1fc, 0x77, 4).unwrap();
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0xfc, 4).unwrap(), 8);
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0xfc, 4).unwrap(), 8 << 6);
    c.step().unwrap();
    assert_eq!(c.bus.read(RAM + 0xfc, 4).unwrap(), (8 << 6) | (1 << 3));
    assert_eq!(c.bus.read(RAM + 0x1fc, 4).unwrap(), 0x77);
}

/// Where a one-instruction conditional block `[h, x, r1 = 1]` continues.
fn conditional_target(h: u16, x: u16, register: usize, value: u32) -> u32 {
    let mut c = cpu(&[h, x, 0x2141, 0x2140]);
    c.r[register] = value;
    c.step().unwrap();
    c.pc - XIP
}

#[test]
fn mask_move_uses_the_packed_immediate_forms() {
    // Stock 0x020259ac: e060 3264, r3 = 0x64006400 (was rejected); e060
    // 3164 is r3 = 0x640064 in objdump (was 0x64006400).
    for (x, expected) in [
        (0x3264, 0x6400_6400),
        (0x3164, 0x0064_0064),
        (0x3364, 0x6464_6464),
        (0x3064, 0x64),
    ] {
        let mut c = cpu(&[0xe060, x]);
        c.step().unwrap();
        assert_eq!(c.r[3], expected, "x {x:04x}");
        assert_eq!(c.pc, XIP + 4);
    }
}

#[test]
fn unsigned_conditional_immediates_are_not_sign_extended() {
    // clang -target pi32v2 -O2 for `if (x < 3000u) y = y * 3 + 1;`:
    // r2 = r1 * 0x3; r2 += 1; if (r0 >= 3000) { r2 = r1 }; r0 = r2; rts.
    // 3000 is 0xbb8: bit 11 set, still an unsigned 3000 (objdump agrees for
    // e93X and e9bX alike).
    for (x, expected) in [(100, 16), (2999, 16), (3000, 5), (4000, 5)] {
        let mut c = cpu(&[
            0xe1e2, 0x1003, 0x21c2, 0xe930, 0x0bb8, 0x1612, 0x1620, 0x0080,
        ]);
        c.r[0] = x;
        c.r[1] = 5;
        c.sr[3] = XIP + 0x40;
        while c.pc != XIP + 0x40 {
            c.step().unwrap();
        }
        assert_eq!(c.r[0], expected, "x = {x}");
    }
    for (value, enters) in [(2999, true), (3000, false), (0xffff_ffff, false)] {
        let expected = if enters { 4 } else { 6 };
        assert_eq!(conditional_target(0xe9b0, 0x0bb8, 0, value), expected); // if (r0 < 3000) {
    }
    // ec33 0ba5 `if (r3 > 2981) {`: imm12 too, not packed.
    for (value, enters) in [(2982, true), (2981, false)] {
        let expected = if enters { 4 } else { 6 };
        assert_eq!(conditional_target(0xec33, 0x0ba5, 3, value), expected);
    }
    // e8b3 0ba5 `if (r3 != -1115) {` and ea33 0ba5
    // `if ((r3 & 0x14A00) != 0) {` (a packed mask).
    for (value, enters) in [(-1115i32 as u32, false), (0, true)] {
        let expected = if enters { 4 } else { 6 };
        assert_eq!(conditional_target(0xe8b3, 0x0ba5, 3, value), expected);
    }
    for (value, enters) in [(0x200, true), (0x1_0000, true), (0x1ff, false)] {
        let expected = if enters { 4 } else { 6 };
        assert_eq!(conditional_target(0xea33, 0x0ba5, 3, value), expected);
    }
}

#[test]
fn long_compare_branch_sign_extends_the_immediate_except_for_unsigned_kinds() {
    // JieLi objdump, x = 0x0fff: ff00 `if (r0 == -1)`, ff01 `!= -1`, ff0a-ff0d
    // `ifs (r0 >= / < / > / <= -1)`, but ff02/ff03/ff08/ff09 compare with 4095
    // (unsigned). Stock FM-1_015's preset list wraps its index with
    // ff0d 0fff (0x020247a0, `ifs (r0 <= -1)`): read as 4095, one detent of
    // PRESETS went from 001 to 130.
    let taken = |h: u16, r0: u32| {
        let mut c = cpu(&[h, 0x0fff, 0x0010]);
        c.r[0] = r0;
        c.step().unwrap();
        c.pc == XIP + 6 + 32
    };
    assert!(!taken(0xff0d, 1));
    assert!(taken(0xff0d, u32::MAX));
    assert!(taken(0xff0b, (-2i32) as u32));
    assert!(!taken(0xff0b, 0));
    assert!(taken(0xff0c, 0));
    assert!(taken(0xff0a, (-1i32) as u32));
    assert!(taken(0xff00, u32::MAX));
    assert!(!taken(0xff00, 4095));
    assert!(taken(0xff01, 4095));
    // Unsigned kinds keep 4095.
    assert!(taken(0xff02, 4095));
    assert!(!taken(0xff02, 4094));
    assert!(taken(0xff03, 4094));
    assert!(taken(0xff08, 4096));
    assert!(taken(0xff09, 4095));
    assert!(!taken(0xff09, u32::MAX));
}
