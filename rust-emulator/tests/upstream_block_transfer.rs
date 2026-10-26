// SPDX-License-Identifier: GPL-3.0-only
// Multiple-register (EB00-EB7F) and doubleword register-sum (EC58-EC5F)
// accesses, with the orders measured on a real FM-1 by the upstream QEMU
// emulator (simonjohansson/fm1-emulator 8789037).
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

/// Four marker words at DATA.
fn markers(c: &mut Cpu) {
    for (i, v) in [0x11111111u32, 0x22222222, 0x33333333, 0x44444444]
        .into_iter()
        .enumerate()
    {
        c.bus.write(DATA + 4 * i as u32, v, 4).unwrap();
    }
}

#[test]
fn register_sum_store_has_no_writeback() {
    // EC58 0233: d[r3+r2] = r1_r0; neither base nor index changes.
    let mut c = cpu(&[0xec58, 0x0233]);
    markers(&mut c);
    c.r[3] = DATA;
    c.r[2] = 8;
    c.r[0] = 0xaabbccdd;
    c.r[1] = 0x55667788;
    c.step().unwrap();
    assert_eq!(
        (word(&c, DATA + 8), word(&c, DATA + 12)),
        (0xaabbccdd, 0x55667788)
    );
    assert_eq!((c.r[3], c.r[2]), (DATA, 8));
}

#[test]
fn register_sum_store_scales_the_index_by_eight() {
    // EC58 3B02 (kind bit 3): d[r3+r2<<3] = r1_r0.
    let mut c = cpu(&[0xec58, 0x023b]);
    markers(&mut c);
    c.r[3] = DATA;
    c.r[2] = 1;
    c.r[0] = 0xaabbccdd;
    c.r[1] = 0x55667788;
    c.step().unwrap();
    assert_eq!(
        (word(&c, DATA + 8), word(&c, DATA + 12)),
        (0xaabbccdd, 0x55667788)
    );
}

#[test]
fn register_sum_load_reads_the_unscaled_incoming_sum() {
    // EC58 0232: r1_r0 = d[r3+r2], no writeback.
    let mut c = cpu(&[0xec58, 0x0232]);
    markers(&mut c);
    c.r[3] = DATA;
    c.r[2] = 8;
    c.step().unwrap();
    assert_eq!((c.r[0], c.r[1]), (0x33333333, 0x44444444));
    assert_eq!((c.r[3], c.r[2]), (DATA, 8));
}

#[test]
fn register_sum_with_the_pre_index_opcode_writes_back() {
    // EC5C 8012: r9_r8 = d[++r1=r0].
    let mut c = cpu(&[0xec5c, 0x8012]);
    markers(&mut c);
    c.r[1] = DATA;
    c.r[0] = 8;
    c.step().unwrap();
    assert_eq!((c.r[8], c.r[9]), (0x33333333, 0x44444444));
    assert_eq!(c.r[1], DATA + 8);
}

#[test]
fn register_sum_rejects_a_scaled_pre_index() {
    // Vendor EC5C 0A00 has no decoding: the pre-index opcode does not scale.
    let mut c = cpu(&[0xec5c, 0x0a3a]);
    assert!(c.step().is_err());
}

#[test]
fn load_may_include_the_base_register() {
    // EB02 0006 {r2, r1} = [r2+]: every word is read from the incoming base.
    let mut c = cpu(&[0xeb02, 0x0006]);
    markers(&mut c);
    c.r[2] = DATA;
    c.step().unwrap();
    assert_eq!((c.r[1], c.r[2]), (0x11111111, 0x22222222));
}

#[test]
fn load_post_increment_charges_the_count() {
    let mut c = cpu(&[0xeb10, 0x0110]);
    markers(&mut c);
    c.r[0] = DATA;
    c.step().unwrap();
    assert_eq!((c.r[4], c.r[8], c.r[0]), (0x11111111, 0x22222222, DATA + 8));
}

#[test]
fn store_post_increment_charges_the_count() {
    let mut c = cpu(&[0xeb31, 0x000c]);
    c.r[1] = DATA;
    c.r[2] = 0xaabbccdd;
    c.r[3] = 0x55667788;
    c.step().unwrap();
    assert_eq!(
        (word(&c, DATA), word(&c, DATA + 4)),
        (0xaabbccdd, 0x55667788)
    );
    assert_eq!(c.r[1], DATA + 8);
}

#[test]
fn store_pre_decrement_captures_the_incoming_registers() {
    // EB71 0006 in Bubba's ABI pushes the incoming r1 among the set.
    let mut c = cpu(&[0xeb71, 0x0006]);
    c.r[1] = DATA + 16;
    c.r[2] = 0xaabbccdd;
    c.step().unwrap();
    assert_eq!(
        (word(&c, DATA + 8), word(&c, DATA + 12)),
        (DATA + 16, 0xaabbccdd)
    );
    assert_eq!(c.r[1], DATA + 8);
}

#[test]
fn load_pre_decrement_reads_the_block_below() {
    let mut c = cpu(&[0xeb51, 0x000c]);
    markers(&mut c);
    c.r[1] = DATA + 8;
    c.step().unwrap();
    assert_eq!((c.r[2], c.r[3], c.r[1]), (0x11111111, 0x22222222, DATA));
}

#[test]
fn store_below_the_base_has_no_writeback() {
    let mut c = cpu(&[0xeb61, 0x000c]);
    c.r[1] = DATA + 8;
    c.r[2] = 0xaabbccdd;
    c.r[3] = 0x55667788;
    c.step().unwrap();
    assert_eq!(
        (word(&c, DATA), word(&c, DATA + 4)),
        (0xaabbccdd, 0x55667788)
    );
    assert_eq!(c.r[1], DATA + 8);
}

#[test]
fn load_below_the_base_has_no_writeback() {
    let mut c = cpu(&[0xeb41, 0x000c]);
    markers(&mut c);
    c.r[1] = DATA + 8;
    c.step().unwrap();
    assert_eq!((c.r[2], c.r[3], c.r[1]), (0x11111111, 0x22222222, DATA + 8));
}

#[test]
fn a_loaded_base_wins_over_the_writeback() {
    // The writeback lands before a masked base's own load, so the loaded
    // value stays: EB11 0003 {r0, r1} = [r1++].
    let mut c = cpu(&[0xeb11, 0x0003]);
    markers(&mut c);
    c.r[1] = DATA;
    c.step().unwrap();
    assert_eq!((c.r[0], c.r[1]), (0x11111111, 0x22222222));
}
