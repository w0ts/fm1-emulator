// SPDX-License-Identifier: GPL-3.0-only
// Compiler-emitted forms, decoded against vendor disassembly and the pinned
// Quarkslab pi32v2 SLEIGH reference. Unknown/reserved forms still fault.
use crate::{
    cpu::{signed, Cpu, Fault, Step},
    decode::{Extended, Wide},
    simd::{self, half, set_half},
};
pub(crate) fn packed(x: u32) -> u32 {
    match (x >> 10) & 3 {
        0 => match x & 0x300 {
            0x300 => (x & 255) * 0x01010101,
            0x100 => (x & 255) * 0x00010001,
            0x200 => (x & 255) * 0x01000100,
            _ => x & 255,
        },
        mode => ((0x80 | (x & 127)) << (32 - mode * 8)) >> ((x >> 7) & 7),
    }
}
/// Word offset of the E866/E868/E86C/E86D read-modify-write forms: x bits 2-7
/// as a signed byte (vendor objdump: E868 12FC is [r1+-4] += r2).
fn memory_offset(x: u32) -> u32 {
    signed(x & 0xfc, 8) as u32
}

/// Signed nine-bit byte offset of the EE5X and EEDX forms; h bit 0 is its sign.
fn word_increment(h: u32, x: u32) -> u32 {
    // ECD8-ECDF: (h & 7) : x[11:8] : x[3:2], signed (vendor objdump: ECDF 4F00
    // is r4 = [r0++=-16], ECDA 1238 is r1 = [r3++=552]).
    signed(((h & 7) << 8) | (((x >> 8) & 15) << 4) | (x & 12), 11) as u32
}

fn byte_offset(h: u32, x: u32) -> u32 {
    signed(((h & 1) << 8) | (((x >> 8) & 15) << 4) | (x & 15), 9) as u32
}

/// Store the upper halfword of rD (the vendor's `= rD.h` stores).
fn store_upper_half(cpu: &mut Cpu, address: u32, d: usize) -> Step<()> {
    cpu.write_sized(address, cpu.r[d] >> 16, 2)
}

/// Load or store the register pair rD+1:rD, the even register at `address`.
fn pair_access(cpu: &mut Cpu, address: u32, d: usize, store: bool) -> Step<()> {
    let reg = d & 14;
    if store {
        cpu.write(address, cpu.r[reg])?;
        cpu.write(address.wrapping_add(4), cpu.r[reg + 1])?;
    } else {
        let low = cpu.read(address, 4)?;
        let high = cpu.read(address.wrapping_add(4), 4)?;
        cpu.r[reg] = low;
        cpu.r[reg + 1] = high;
    }
    Ok(())
}

/// IEEE single test of a register conditional block with x bit 7 (vendor
/// objdump -mattr=+fprev1 prints `iff`; the order of the compare-branches).
/// Unordered operands are rejected by the caller.
fn float_condition(kind: u32, lhs: f32, rhs: f32) -> bool {
    match kind {
        0x81 => lhs == rhs,
        0x89 => lhs != rhs,
        0x91 | 0xd1 => lhs >= rhs,
        0x99 | 0xd9 => lhs < rhs,
        0xa1 => lhs != rhs,
        0xc1 | 0xe1 => lhs > rhs,
        _ => lhs <= rhs,
    }
}

pub(crate) fn execute(
    cpu: &mut Cpu,
    h: u32,
    pc: u32,
    kind: Extended,
) -> Step<Option<(u32, &'static str)>> {
    let a = (h & 7) as usize;
    let b = ((h >> 4) & 7) as usize;
    let mut next = pc + 2;
    let mut mem = None;
    let op;
    match kind {
        Extended::MoveRegisterPair => {
            let destination = (h & 14) as usize;
            let source = ((h >> 4) & 14) as usize;
            let values = [cpu.r[source], cpu.r[source + 1]];
            cpu.r[destination..destination + 2].copy_from_slice(&values);
            op = "move_register_pair";
        }
        Extended::Multiply => {
            let n = (h & 15) as usize;
            cpu.r[n] = cpu.r[n].wrapping_mul(cpu.r[((h >> 4) & 15) as usize]);
            op = "multiply";
        }
        Extended::ClearPair => {
            let destination = (h & 14) as usize;
            cpu.r[destination] = 0;
            cpu.r[destination + 1] = 0;
            op = "clear_pair";
        }
        Extended::AddRegister => {
            let n = (h & 15) as usize;
            cpu.r[n] = cpu.arithmetic(cpu.r[n], cpu.r[((h >> 4) & 15) as usize], false, 0);
            op = "add_register";
        }
        Extended::ClearHighRegister => {
            cpu.r[8 + a] = 0;
            op = "clear_high_register";
        }
        Extended::CacheFlushInvalidate => {
            // flush [rN] (0220) and flushinv [rN] (0230) act on one data cache
            // line; memory is not cached here, so neither changes anything.
            op = "cache_flush_invalidate";
        }
        Extended::StackWord => {
            mem = Some((
                // Bit 3 selects r8-r15 (vendor objdump: 2709 is r9 = [sp+28]).
                (h & 15) as usize,
                0,
                // Bit 5 supplies offset bit 7; Felucca spills beyond 128 bytes.
                cpu.sr[14].wrapping_add((((h >> 8) & 31) | (h & 32)) * 4),
                4,
                h & 128 != 0,
                false,
                None,
            ));
            op = "stack_word";
        }
        Extended::Extend => {
            let bits = if h & 0x80 == 0 { 8 } else { 16 };
            cpu.r[a] = if h & 8 != 0 {
                signed(cpu.r[b], bits) as u32
            } else {
                cpu.r[b] & ((1 << bits) - 1)
            };
            op = "extend";
        }
        Extended::MoveNegative => {
            cpu.r[a] = (h >> 8) | 0xffffffe0;
            op = "mov_negative";
        }
        Extended::BitRegister => {
            let bit = 1 << ((h >> 8) & 31);
            match h & 0xf8 {
                0x30 => cpu.r[a] |= bit,
                0x38 => cpu.r[a] ^= bit,
                _ => cpu.r[a] &= !bit,
            }
            op = "bit_register";
        }
        Extended::ShiftRegister => {
            let shift = cpu.r[b];
            cpu.r[a] = if shift >= 32 {
                if h & 0x88 == 0x88 && (cpu.r[a] as i32) < 0 {
                    u32::MAX
                } else {
                    0
                }
            } else {
                match h & 0x88 {
                    0 => cpu.r[a] << shift,
                    0x80 => cpu.r[a] >> shift,
                    _ => ((cpu.r[a] as i32) >> shift) as u32,
                }
            };
            op = "shift_register";
        }
        Extended::AddStack => {
            cpu.r[a] = cpu.sr[14].wrapping_add((((h >> 5) & 3) << 5) | ((h >> 8) & 31));
            op = "add_stack";
        }
        Extended::GotoRegister => {
            next = cpu.r[(h & 15) as usize];
            op = "goto_register";
        }
        Extended::Ssync => {
            op = "ssync";
        }
        Extended::TableBranch => {
            let size = if h & 0x10 == 0 { 1 } else { 2 };
            next = (pc + 2)
                .wrapping_add(cpu.read((pc + 2).wrapping_add(cpu.r[(h & 15) as usize]), size)? * 2);
            op = "table_branch";
        }
        Extended::MemorySmall => {
            let size = if h & 0x2000 == 0 { 1 } else { 2 };
            let address = cpu.r[b].wrapping_add((signed((h >> 8) & 31, 5) * size as i32) as u32);
            mem = Some((a, b, address, size, h & 0x80 != 0, false, None));
            op = "memory_small";
        }
        Extended::MemoryPostincrementRegister => {
            let store = h & 8 != 0;
            if !store && a == b {
                // Aliased load/writeback ordering needs hardware measurement.
                return Ok(None);
            }
            let size = match h & 0xfc00 {
                0x0800 => 4,
                0x0c00 => 2,
                _ => 1,
            };
            let address = cpu.r[b];
            mem = Some((
                a,
                b,
                address,
                size,
                store,
                false,
                Some(address.wrapping_add(cpu.r[8 + ((h >> 7) & 7) as usize])),
            ));
            op = "memory_postincrement_register";
        }
        Extended::MemoryPostincrement => {
            let kind = (h >> 7) & 7;
            let size = match kind {
                2 | 3 => 4,
                4 | 5 => 2,
                _ => 1,
            };
            let address = cpu.r[b];
            let delta = if h & 8 == 0 {
                size as i32
            } else {
                -(size as i32)
            };
            mem = Some((
                a,
                b,
                address,
                size,
                kind & 1 != 0,
                false,
                Some(address.wrapping_add(delta as u32)),
            ));
            op = "memory_postincrement";
        }
        Extended::Wide => {
            let x = cpu.read(pc + 2, 2)?;
            let n = (h & 15) as usize;
            let d = (x >> 12) as usize;
            let s = ((x >> 4) & 15) as usize;
            let c = ((x >> 8) & 15) as usize;
            next = pc + 4;
            match cpu.decoded_wide(h, x) {
                Wide::FloatRegister => {
                    // A zero divisor of either sign raises the divide-by-zero
                    // exception when armed: X0X ea3c665 crashed on a float
                    // divide on a physical FM-1 with EMU_CON bit 2 set.
                    if x & 15 == 3 && f32::from_bits(cpu.r[c]) == 0.0 && cpu.divide_by_zero(pc)? {
                        return Ok(Some((pc + 4, "float_register")));
                    }
                    let value =
                        crate::float::result(x, &cpu.r).map_err(|reason| Fault::Access {
                            pc,
                            fault: crate::bus::AccessFault {
                                address: pc,
                                size: 4,
                                operation: "floating-point",
                                reason,
                            },
                        })?;
                    let Some(value) = value else {
                        return Ok(None);
                    };
                    cpu.r[d] = value.value;
                    if let Some(flags) = value.flags {
                        cpu.sr[5] = (cpu.sr[5] & !15) | flags;
                    }
                    op = "float_register";
                }
                Wide::BranchRegisterMask => {
                    let test =
                        (cpu.r[n] & cpu.r[((h >> 4) & 15) as usize] != 0) == (h & 0x100 != 0);
                    if test {
                        next = next.wrapping_add((signed(x, 16) * 2) as u32);
                    }
                    op = "branch_register_mask";
                }
                Wide::MemoryShift => {
                    let address = cpu.r[d].wrapping_add(memory_offset(x));
                    let value = cpu.read(address, 4)?;
                    let shift = c + ((h & 1) as usize) * 16;
                    cpu.write(
                        address,
                        match x & 3 {
                            0 => value << shift,
                            2 => value >> shift,
                            _ => ((value as i32) >> shift) as u32,
                        },
                    )?;
                    op = "memory_shift";
                }
                Wide::MemoryArithmeticRegister => {
                    let addr = cpu.r[d].wrapping_add(memory_offset(x));
                    let value = cpu.read(addr, 4)?;
                    let operand = cpu.r[c];
                    cpu.write(
                        addr,
                        if x & 2 != 0 {
                            value.wrapping_sub(operand)
                        } else {
                            value.wrapping_add(operand)
                        },
                    )?;
                    op = if x & 2 != 0 {
                        "memory_subtract_register"
                    } else {
                        "memory_add_register"
                    };
                }
                Wide::HalfwordExtended => {
                    let store = x & 1 != 0;
                    // Loads and stores have a signed ten-bit displacement. Bit 2
                    // selects value sign extension of loads and the upper half
                    // for stores (vendor objdump: ED53 0F2B is h[r2+-6] = r0 and
                    // ED55 0F2B is h[r2+506] = r0.h). Stock LVGL uses ED5B to load
                    // at r8-4 and write back r8.
                    let offset =
                        (signed(h & 3, 2) << 8) | (((x >> 8) & 15) << 4) as i32 | (x & 14) as i32;
                    let addr = cpu.r[s].wrapping_add(offset as u32);
                    let updated = if h & 8 != 0 { Some(addr) } else { None };
                    if store && h & 4 != 0 {
                        store_upper_half(cpu, addr, d)?;
                        if let Some(value) = updated {
                            cpu.r[s] = value;
                        }
                    } else {
                        mem = Some((d, s, addr, 2, store, !store && h & 4 != 0, updated));
                    }
                    op = "halfword_extended";
                }
                Wide::HalfwordPostincrement => {
                    // EDD0-EDD7: a signed ten-bit increment with h bits 0-1 on
                    // top; bit 2 is a signed load or a store of the upper half
                    // (vendor objdump: EDD4 1231 is h[r3++=32] = r1.h).
                    let store = x & 1 != 0;
                    let increment =
                        signed(((h & 3) << 8) | (((x >> 8) & 15) << 4) | (x & 14), 10) as u32;
                    let address = cpu.r[s];
                    if store && h & 4 != 0 {
                        store_upper_half(cpu, address, d)?;
                        cpu.r[s] = address.wrapping_add(increment);
                    } else {
                        mem = Some((
                            d,
                            s,
                            address,
                            2,
                            store,
                            !store && h & 4 != 0,
                            Some(address.wrapping_add(increment)),
                        ));
                    }
                    op = "halfword_postincrement";
                }
                Wide::BytePostincrementStore => {
                    // EED2/EED3: a signed nine-bit increment, h bit 0 its sign.
                    let increment = byte_offset(h, x);
                    let address = cpu.r[s];
                    mem = Some((
                        d,
                        s,
                        address,
                        1,
                        true,
                        false,
                        Some(address.wrapping_add(increment)),
                    ));
                    op = "byte_postincrement_store";
                }
                Wide::BytePostincrementLoad => {
                    let off = byte_offset(h, x);
                    let addr = cpu.r[s];
                    mem = Some((
                        d,
                        s,
                        addr,
                        1,
                        false,
                        h & 4 != 0,
                        Some(addr.wrapping_add(off)),
                    ));
                    op = "byte_postincrement_load";
                }
                Wide::MultiplyImmediate => {
                    cpu.r[n] = cpu.r[d].wrapping_mul(packed(x));
                    op = "multiply_immediate";
                }
                Wide::BitMask => {
                    let mask = 1u32 << (cpu.r[c] & 31);
                    cpu.r[d] = match x & 3 {
                        0 => cpu.r[s] | mask,
                        1 => cpu.r[s] ^ mask,
                        2 => cpu.r[s] & mask,
                        _ => cpu.r[s] & !mask,
                    };
                    op = "bit_mask";
                }
                Wide::StackPair => {
                    let addr = cpu.sr[14] + (x & 4092);
                    let r = d & 14;
                    if x & 1 != 0 {
                        cpu.write(addr, cpu.r[r])?;
                        cpu.write(addr + 4, cpu.r[r + 1])?;
                    } else {
                        cpu.r[r] = cpu.read(addr, 4)?;
                        cpu.r[r + 1] = cpu.read(addr + 4, 4)?;
                    }
                    op = "stack_pair";
                }
                Wide::SaturateSigned16 => {
                    cpu.r[d] = (cpu.r[c] as i32).clamp(-32768, 32767) as u32;
                    op = "saturate_signed16";
                }
                Wide::PushSpecialMask => {
                    // [--sp] = {sp, ssp, usp, icfg, psr, rets, retx, rete, reti}
                    // for x = 782F (the fatal-exception frame of Felucca-derived
                    // fm1_vec.S, read back by fm1_fault_c from reti upwards):
                    // bit n is sr[n], pushed highest first. The saved sp is its
                    // value before the push; nothing reads it back as a pointer.
                    let sp = cpu.sr[14];
                    for index in (0..15).rev() {
                        if x & (1 << index) != 0 {
                            let value = if index == 14 { sp } else { cpu.sr[index] };
                            cpu.push(value)?;
                        }
                    }
                    op = "push_special_mask";
                }
                Wide::PopSpecialMask => {
                    // The same set popped lowest first; bit 15 pops pc (stock
                    // FM-1 0x02043854: E950 8000 is {pc} = [sp++]).
                    for index in 0..14 {
                        if x & (1 << index) != 0 {
                            cpu.sr[index] = cpu.pop()?;
                        }
                    }
                    if x & 0x8000 != 0 {
                        next = cpu.pop()?;
                    }
                    op = "pop_special_mask";
                }
                Wide::Trigger => {
                    // Debug trigger event (SDK ___trig, followed by a printf in
                    // jl_fft.c): nothing happens without an attached debugger.
                    op = "trigger";
                }
                Wide::ReverseBytes => {
                    cpu.r[d] = cpu.r[c].swap_bytes();
                    op = "reverse_bytes";
                }
                Wide::SubtractPackedImmediate => {
                    cpu.r[n] = cpu.arithmetic(cpu.r[d], packed(x), true, 0);
                    op = "subtract_packed_immediate";
                }
                Wide::ReverseSubtract => {
                    cpu.r[n] = cpu.arithmetic(packed(x), cpu.r[d], true, 0);
                    op = "reverse_subtract";
                }
                Wide::MultiplyExtended => {
                    cpu.r[d] = cpu.r[s].wrapping_mul(cpu.r[c]);
                    op = "multiply_extended";
                }
                Wide::MultiplyWide => {
                    let product = if d & 1 == 0 {
                        cpu.r[s] as u64 * cpu.r[c] as u64
                    } else {
                        (cpu.r[s] as i32 as i64 * cpu.r[c] as i32 as i64) as u64
                    };
                    let pair = d & 14;
                    let result = if h == 0xe1fc {
                        let accumulator = cpu.r[pair] as u64 | ((cpu.r[pair + 1] as u64) << 32);
                        accumulator.wrapping_add(product)
                    } else {
                        product
                    };
                    cpu.r[d & 14] = result as u32;
                    cpu.r[(d & 14) + 1] = (result >> 32) as u32;
                    op = if h == 0xe1fc {
                        "multiply_accumulate_wide"
                    } else {
                        "multiply_wide"
                    };
                }
                Wide::DivideWide => {
                    if cpu.r[c] == 0 && cpu.divide_by_zero(pc)? {
                        return Ok(Some((pc + 4, "divide_wide")));
                    }
                    let dividend = cpu.r[s] as u64 | ((cpu.r[s + 1] as u64) << 32);
                    let d = d & 14;
                    // Measured on an FM-1 (upstream 1875a54): a zero divisor
                    // gives 0 in both forms (when the trap is off), signed
                    // division truncates toward zero and INT64_MIN / -1 wraps.
                    let quotient = if x & 0x1000 == 0 {
                        dividend.checked_div(cpu.r[c] as u64).unwrap_or(0)
                    } else {
                        (dividend as i64)
                            .checked_div(cpu.r[c] as i32 as i64)
                            .unwrap_or(if cpu.r[c] == 0 { 0 } else { dividend as i64 })
                            as u64
                    };
                    cpu.r[d] = quotient as u32;
                    cpu.r[d + 1] = (quotient >> 32) as u32;
                    op = "divide_wide";
                }
                Wide::ShiftWideRegister => {
                    let value = cpu.r[d] as u64 | ((cpu.r[d + 1] as u64) << 32);
                    let shift = cpu.r[c];
                    let result = if x & 2 == 0 {
                        value.checked_shl(shift)
                    } else {
                        value.checked_shr(shift)
                    }
                    .unwrap_or(0);
                    cpu.r[d] = result as u32;
                    cpu.r[d + 1] = (result >> 32) as u32;
                    op = "shift_wide_register";
                }
                Wide::ShiftWideImmediate => {
                    let value = cpu.r[d] as u64 | ((cpu.r[d + 1] as u64) << 32);
                    let shift = ((x >> 8) & 3) * 16 + (x & 15);
                    let result = match (x >> 10) & 3 {
                        0 => value << shift,
                        2 => value >> shift,
                        _ => ((value as i64) >> shift) as u64,
                    };
                    cpu.r[d] = result as u32;
                    cpu.r[d + 1] = (result >> 32) as u32;
                    op = "shift_wide_immediate";
                }
                Wide::Divide => {
                    if cpu.r[c] == 0 && cpu.divide_by_zero(pc)? {
                        return Ok(Some((pc + 4, "divide")));
                    }
                    cpu.r[d] = if x & 1 == 0 {
                        cpu.r[s].checked_div(cpu.r[c])
                    } else {
                        (cpu.r[s] as i32)
                            .checked_div(cpu.r[c] as i32)
                            .map(|v| v as u32)
                    }
                    .ok_or_else(|| Box::new(Fault::Unsupported { pc, word: h as u16 }))?;
                    op = if x & 1 == 0 {
                        "divide_unsigned"
                    } else {
                        "divide_signed"
                    };
                }
                Wide::CountLeadingZeros => {
                    cpu.r[d] = cpu.r[c].leading_zeros();
                    op = "count_leading_zeros";
                }
                Wide::Absolute => {
                    cpu.r[d] = (cpu.r[c] as i32).wrapping_abs() as u32;
                    op = "absolute";
                }
                Wide::Maximum => {
                    cpu.r[d] = if x & 1 == 0 {
                        cpu.r[s].max(cpu.r[c])
                    } else {
                        (cpu.r[s] as i32).max(cpu.r[c] as i32) as u32
                    };
                    op = if x & 1 == 0 {
                        "maximum_unsigned"
                    } else {
                        "maximum_signed"
                    };
                }
                Wide::Minimum => {
                    cpu.r[d] = if x & 1 == 0 {
                        cpu.r[s].min(cpu.r[c])
                    } else {
                        (cpu.r[s] as i32).min(cpu.r[c] as i32) as u32
                    };
                    op = if x & 1 == 0 {
                        "minimum_unsigned"
                    } else {
                        "minimum_signed"
                    };
                }
                Wide::MemoryAdd => {
                    let addr = cpu.r[d] + (h & 31) * 4;
                    cpu.write(
                        addr,
                        cpu.read(addr, 4)?.wrapping_add(signed(x & 4095, 12) as u32),
                    )?;
                    op = "memory_add";
                }
                Wide::DecrementBranch => {
                    cpu.r[n] = cpu.r[n].wrapping_sub(1);
                    if cpu.r[n] != 0 {
                        next = next.wrapping_add((signed(x, 16) * 2) as u32);
                    }
                    op = "decrement_branch";
                }
                Wide::MemoryBit => {
                    let addr = cpu.r[d].wrapping_add(memory_offset(x));
                    let old = cpu.read(addr, 4)?;
                    let mask = 1u32 << (cpu.r[c] & 31);
                    cpu.write(
                        addr,
                        match x & 3 {
                            0 => old | mask,
                            1 => old ^ mask,
                            3 => old & !mask,
                            _ => return Ok(None),
                        },
                    )?;
                    op = "memory_bit";
                }
                Wide::ArithmeticRegister => {
                    let subtract = x & 2 != 0;
                    let extra = if h == 0xe0b8 {
                        ((cpu.sr[5] >> 1) & 1) ^ subtract as u32
                    } else {
                        0
                    };
                    cpu.r[d] = cpu.arithmetic(cpu.r[s], cpu.r[c], subtract, extra);
                    op = if x & 2 == 0 {
                        if h == 0xe0b8 {
                            "add_carry"
                        } else {
                            "add_extended"
                        }
                    } else {
                        if h == 0xe0b8 {
                            "subtract_carry"
                        } else {
                            "subtract_extended"
                        }
                    };
                }
                Wide::ShiftRegisterExtended => {
                    let shift = cpu.r[c];
                    // Quarkslab pi32v2 imm1619: 0 and 1 shift left, 2 logical
                    // and 3 arithmetic right.
                    cpu.r[d] = match x & 3 {
                        0 | 1 => cpu.r[s].checked_shl(shift).unwrap_or(0),
                        2 => cpu.r[s].checked_shr(shift).unwrap_or(0),
                        _ => ((cpu.r[s] as i32) >> shift.min(31)) as u32,
                    };
                    op = "shift_register_extended";
                }
                Wide::BytePreincrement => {
                    let addr = cpu.r[s].wrapping_add(cpu.r[c]);
                    mem = Some((d, s, addr, 1, x & 1 != 0, x & 2 != 0, Some(addr)));
                    op = "byte_preincrement";
                }
                Wide::RotateRightImmediate => {
                    // Vendor r3 compiles (v >> 1) | (v << 31) to "v <> 1".
                    // A zero count field represents 32, also an identity rotation.
                    cpu.r[d] = cpu.r[s].rotate_right(((x >> 8) & 1) * 16 + (x & 15));
                    op = "rotate_right_immediate";
                }
                Wide::ShiftExtended => {
                    let shift = ((x >> 8) & 3) * 16 + (x & 15);
                    let mode = (x >> 10) & 3;
                    cpu.r[d] = match mode {
                        0 => cpu.r[s].checked_shl(shift).unwrap_or(0),
                        2 => cpu.r[s].checked_shr(shift).unwrap_or(0),
                        _ => ((cpu.r[s] as i32) >> shift.min(31)) as u32,
                    };
                    op = "shift_extended";
                }
                Wide::BranchLong => {
                    let value = match h & 0x60 {
                        // Equality/signed forms extend the literal; unsigned ordering
                        // keeps all 12 bits (stock's cache bound is 2111).
                        0 if matches!(h & 15, 0 | 1 | 10..=13) => signed(x & 4095, 12) as u32,
                        0 => x & 4095,
                        0x20 => packed(x),
                        0x40 => cpu.r[c],
                        _ => packed(x),
                    };
                    let lhs = cpu.r[d];
                    let test = if h & 0x60 == 0x40 && x & 128 != 0 {
                        // Vendor r3: x bit 7 makes the register form an
                        // IEEE single compare (iff), the conditions in the
                        // order of the conditional blocks: FF42 7380 is
                        // iff (r7 u>= r3) (X0X's limiter, 0x0201ffa2).
                        let lhs = f32::from_bits(lhs);
                        let rhs = f32::from_bits(value);
                        if !lhs.is_finite() || !rhs.is_finite() {
                            return Err(Box::new(Fault::Access {
                                pc,
                                fault: crate::bus::AccessFault {
                                    address: pc,
                                    size: 6,
                                    operation: "floating-point condition",
                                    reason:
                                        "exceptional floating-point comparison is not implemented",
                                },
                            }));
                        }
                        let kind = [
                            0x81, 0x89, 0x91, 0x99, 0, 0, 0, 0, 0xc1, 0xc9, 0xd1, 0xd9, 0xe1, 0xe9,
                        ][(h & 15) as usize];
                        float_condition(kind, lhs, rhs)
                    } else if h & 0x60 == 0x60 {
                        if h & 1 == 0 {
                            lhs & value == 0
                        } else {
                            lhs & value != 0
                        }
                    } else {
                        match h & 15 {
                            0 => lhs == value,
                            1 => lhs != value,
                            2 => lhs >= value,
                            3 => lhs < value,
                            8 => lhs > value,
                            9 => lhs <= value,
                            10 => (lhs as i32) >= (value as i32),
                            11 => (lhs as i32) < (value as i32),
                            12 => (lhs as i32) > (value as i32),
                            _ => (lhs as i32) <= (value as i32),
                        }
                    };
                    next = pc + 6;
                    if test {
                        next = next.wrapping_add((signed(cpu.read(pc + 4, 2)?, 16) * 2) as u32);
                    }
                    op = "branch_long";
                }
                Wide::LogicThree => {
                    cpu.r[d] = match x & 3 {
                        0 => cpu.r[s] | cpu.r[c],
                        1 => cpu.r[s] ^ cpu.r[c],
                        2 => cpu.r[s] & cpu.r[c],
                        _ => cpu.r[s] & !cpu.r[c],
                    };
                    op = "logic_three";
                }
                Wide::StoreRegisterList => {
                    let mut address = cpu.r[n];
                    for register in 0..16 {
                        if x & (1 << register) != 0 {
                            cpu.write(address, cpu.r[register])?;
                            address = address.wrapping_add(4);
                        }
                    }
                    // [rN++] = {...} (EB3X): the base advances past the list.
                    if h & 0x10 != 0 {
                        cpu.r[n] = address;
                    }
                    op = "store_register_list";
                }
                Wide::LoadRegisterList => {
                    let mut address = cpu.r[n];
                    for register in 0..16 {
                        if x & (1 << register) != 0 {
                            cpu.r[register] = cpu.read(address, 4)?;
                            address = address.wrapping_add(4);
                        }
                    }
                    // {...} = [rN++] (EB1X), as the store.
                    if h & 0x10 != 0 {
                        cpu.r[n] = address;
                    }
                    op = "load_register_list";
                }
                Wide::StackSubword => {
                    let halfword = h & 4 == 0;
                    let offset = if halfword { x & 4094 } else { x & 4095 };
                    mem = Some((
                        d,
                        0,
                        cpu.sr[14].wrapping_add(offset),
                        if halfword { 2 } else { 1 },
                        if halfword {
                            h == 0xe9d8 && x & 1 != 0
                        } else {
                            h == 0xe9de
                        },
                        h & 1 != 0,
                        None,
                    ));
                    op = "stack_subword";
                }
                Wide::StackExtended => {
                    mem = Some((d, 0, cpu.sr[14] + (x & 4092), 4, x & 1 != 0, false, None));
                    op = "stack_extended";
                }
                Wide::MemoryPair => {
                    let offset =
                        (signed(h & 7, 3) << 8) | (((x >> 8) & 15) << 4) as i32 | (x & 12) as i32;
                    let addr = cpu.r[s].wrapping_add(offset as u32);
                    pair_access(cpu, addr, d, x & 1 != 0)?;
                    if x & 2 != 0 {
                        // Pre-increment: the address is written back to the base
                        // (vendor objdump: EC50 8012 is r9_r8 = d[++r1=0]).
                        cpu.r[s] = addr;
                    }
                    op = "memory_pair";
                }
                Wide::PairPostincrement => {
                    // EC58-EC5F with x bit 1 clear: d[rS++=imm], the access at
                    // rS, then rS += a signed eleven-bit immediate (vendor
                    // objdump: EC58 2009 is d[r0++=8] = r3_r2).
                    let offset =
                        (signed(h & 7, 3) << 8) | (((x >> 8) & 15) << 4) as i32 | (x & 12) as i32;
                    let addr = cpu.r[s];
                    pair_access(cpu, addr, d, x & 1 != 0)?;
                    cpu.r[s] = addr.wrapping_add(offset as u32);
                    op = "pair_postincrement";
                }
                Wide::PairRegisterPreincrement => {
                    // EC5C with x bit 1: r9_r8 = d[++r1=r0] (stock 0x0200940e).
                    let addr = cpu.r[s].wrapping_add(cpu.r[c]);
                    pair_access(cpu, addr, d, x & 1 != 0)?;
                    cpu.r[s] = addr;
                    op = "pair_register_preincrement";
                }
                Wide::BitField => {
                    let pos = (x >> 7) & 31;
                    let len = (x >> 2) & 31;
                    let mask = (1u32 << len) - 1;
                    cpu.r[n] = if h & 0x10 == 0 {
                        (cpu.r[n] & !(mask << pos)) | ((cpu.r[d] & mask) << pos)
                    } else if x & 1 == 0 {
                        (cpu.r[d] >> pos) & mask
                    } else {
                        // sextra (vendor objdump: E1B0 B041 is
                        // r0 = sextra(r11, p:0, l:16), Baud Girl 0x02012c5e).
                        signed((cpu.r[d] >> pos) & mask, len.max(1)) as u32
                    };
                    op = "bit_field";
                }
                Wide::BranchEqualFlag => {
                    if cpu.sr[5] & 4 != 0 {
                        next = next.wrapping_add((signed(x, 16) * 2) as u32);
                    }
                    op = "branch_equal_flag";
                }
                Wide::BranchBit => {
                    let test = (cpu.r[n] & (1 << ((x >> 11) & 31)) != 0) == (x & 512 != 0);
                    if test {
                        next = next.wrapping_add((signed(x & 511, 9) * 2) as u32);
                    }
                    op = "branch_bit";
                }
                Wide::MemoryMask => {
                    // EF00-EFFF: bits 7-6 select or, xor, and, and-not; bits 5-0
                    // are a signed word offset (vendor objdump: EF3F 0400 is
                    // [r0+-4] |= 0x80000000, EF40 0400 is [r0+0] ^= 0x80000000).
                    let addr = cpu.r[d].wrapping_add((signed(h & 63, 6) * 4) as u32);
                    let old = cpu.read(addr, 4)?;
                    let value = packed(x);
                    cpu.write(
                        addr,
                        match h & 0xc0 {
                            0 => old | value,
                            0x40 => old ^ value,
                            0x80 => old & value,
                            _ => old & !value,
                        },
                    )?;
                    op = "memory_mask";
                }
                Wide::ConditionalBlock => {
                    let kind = (h >> 4) & 255;
                    let lhs = cpu.r[n];
                    let rhs = if kind & 7 == 1 {
                        cpu.r[c]
                    } else if matches!(kind, 0x93 | 0x9b | 0xc3 | 0xcb) {
                        // These unsigned comparisons carry a literal 12-bit value,
                        // unlike the neighboring packed-immediate encodings. Stock's
                        // battery clamp uses ECB0 0208 to compare against 520.
                        x & 4095
                    } else if matches!(kind, 0x83 | 0x8b | 0xd3 | 0xdb | 0xe3 | 0xeb) {
                        signed(x & 4095, 12) as u32
                    } else {
                        packed(x)
                    };
                    // Vendor r3: x bit 7 changes the register comparisons from
                    // integers to floating point (iff). Stock voice pitch
                    // calculation compares negative floats with this block form.
                    // EA1X with x = 0080 stays the integer bit test != 0.
                    let float = kind & 7 == 1 && x & 128 != 0 && !(kind == 0xa1 && x & 0x7f == 0);
                    let test = if float {
                        let lhs = f32::from_bits(lhs);
                        let rhs = f32::from_bits(rhs);
                        if !lhs.is_finite() || !rhs.is_finite() {
                            return Err(Box::new(Fault::Access {
                                pc,
                                fault: crate::bus::AccessFault {
                                    address: pc,
                                    size: 4,
                                    operation: "floating-point condition",
                                    reason:
                                        "exceptional floating-point comparison is not implemented",
                                },
                            }));
                        }
                        float_condition(kind, lhs, rhs)
                    } else {
                        match kind {
                            0x81..=0x83 => lhs == rhs,
                            0x89..=0x8b => lhs != rhs,
                            0x91..=0x93 => lhs >= rhs,
                            0x99..=0x9b => lhs < rhs,
                            0xa1 => {
                                if x & 128 == 0 {
                                    lhs & rhs == 0
                                } else {
                                    lhs & rhs != 0
                                }
                            }
                            0xa2 => lhs & rhs == 0,
                            0xa3 => lhs & rhs != 0,
                            0xc1..=0xc3 => lhs > rhs,
                            0xc9..=0xcb => lhs <= rhs,
                            0xd1..=0xd3 => (lhs as i32) >= (rhs as i32),
                            0xd9..=0xdb => (lhs as i32) < (rhs as i32),
                            0xe1..=0xe3 => (lhs as i32) > (rhs as i32),
                            _ => (lhs as i32) <= (rhs as i32),
                        }
                    };
                    next = cpu.conditional(test, x)?;
                    op = "conditional_block";
                }
                Wide::MemoryLogic => {
                    let addr = cpu.r[d].wrapping_add(x & 0xfc);
                    let old = cpu.read(addr, 4)?;
                    let value = cpu.r[c];
                    cpu.write(
                        addr,
                        match x & 3 {
                            0 => old | value,
                            1 => old ^ value,
                            2 => old & value,
                            _ => old & !value,
                        },
                    )?;
                    op = "memory_logic";
                }
                Wide::LogicImmediate => {
                    let mode = (h >> 4) & 15;
                    let value = if mode == 6 && x & 0xc00 == 0 {
                        x & 1023
                    } else {
                        packed(x)
                    };
                    cpu.r[n] = match mode {
                        4 => cpu.r[d] | value,
                        5 => cpu.r[d] ^ value,
                        6 => cpu.r[d] & value,
                        _ => cpu.r[d] & !value,
                    };
                    op = "logic_immediate";
                }
                Wide::StoreImmediate => {
                    cpu.write(cpu.r[d].wrapping_add((h & 31) * 4), packed(x))?;
                    op = "store_immediate";
                }
                Wide::AddImmediate => {
                    let value = match (h >> 4) & 15 {
                        0 => x & 4095,
                        1 => (x & 4095) + 4096,
                        2 => (x & 4095) | 0xffffe000,
                        3 => (x & 4095) | 0xfffff000,
                        _ => packed(x),
                    };
                    cpu.r[n] = cpu.arithmetic(cpu.r[d], value, false, 0);
                    op = "add_immediate";
                }
                Wide::AddSpExtended => {
                    // Vendor r3 assembler: aligned signed 13-bit stack adjustment.
                    // Stock allocates/reclaims its 616-byte filesystem frame here.
                    cpu.sr[14] = cpu.sr[14].wrapping_add(signed(x, 13) as u32);
                    op = "add_sp_extended";
                }
                Wide::AddStackExtended => {
                    cpu.r[d] = cpu.sr[14].wrapping_add(x & 4095);
                    op = "add_stack_extended";
                }
                Wide::BranchCompareImmediate => {
                    let kind = (h >> 7) & 63;
                    // The ordered unsigned forms take an unsigned imm10 (SLOOP's
                    // fm1_delay_us: F9F1 81FC is jb r1, #960); equality and the
                    // signed forms sign-extend it (Baud Girl: F874 FC04 is je
                    // against -2).
                    let raw = (((h >> 4) & 7) << 7) | (x >> 9);
                    let immediate = signed(raw, 10) as u32;
                    let v = cpu.r[n];
                    let test = match kind {
                        0x30 => v == immediate,
                        0x31 => v != immediate,
                        0x32 => v >= raw,
                        0x33 => v < raw,
                        0x38 => v > raw,
                        0x39 => v <= raw,
                        0x3a => (v as i32) >= signed(immediate, 10),
                        0x3b => (v as i32) < signed(immediate, 10),
                        0x3c => (v as i32) > signed(immediate, 10),
                        _ => (v as i32) <= signed(immediate, 10),
                    };
                    if test {
                        next = next.wrapping_add((signed(x & 511, 9) * 2) as u32);
                    }
                    op = "branch_compare_immediate";
                }
                Wide::BranchCompareFloat => {
                    let lhs = f32::from_bits(cpu.r[d]);
                    let rhs = f32::from_bits(cpu.r[n]);
                    if !lhs.is_finite() || !rhs.is_finite() {
                        return Err(Box::new(Fault::Access {
                            pc,
                            fault: crate::bus::AccessFault {
                                address: pc,
                                size: 4,
                                operation: "floating-point branch",
                                reason: "exceptional floating-point comparison is not implemented",
                            },
                        }));
                    }
                    // Vendor objdump: E800 ==, E880 !=, E900 >=, E980 <, EC00 >,
                    // EC80 <=, then the measured ED00-EE80 forms.
                    let test = match h & 0xfff0 {
                        0xe800 => lhs == rhs,
                        0xe880 => lhs != rhs,
                        0xe900 | 0xed00 => lhs >= rhs,
                        0xe980 | 0xed80 => lhs < rhs,
                        0xec00 | 0xee00 => lhs > rhs,
                        _ => lhs <= rhs,
                    };
                    if test {
                        next = next.wrapping_add((signed(x & 511, 9) * 2) as u32);
                    }
                    op = "branch_compare_float";
                }
                Wide::BranchCompareRegister => {
                    let lhs = cpu.r[d];
                    let rhs = cpu.r[n];
                    let test = match h & 0xfff0 {
                        0xe800 => lhs == rhs,
                        0xe880 => lhs != rhs,
                        0xe900 => lhs >= rhs,
                        0xe980 => lhs < rhs,
                        0xec00 => lhs > rhs,
                        0xec80 => lhs <= rhs,
                        0xed00 => (lhs as i32) >= (rhs as i32),
                        0xed80 => (lhs as i32) < (rhs as i32),
                        0xee00 => (lhs as i32) > (rhs as i32),
                        _ => (lhs as i32) <= (rhs as i32),
                    };
                    if test {
                        next = next.wrapping_add((signed(x & 511, 9) * 2) as u32);
                    }
                    op = "branch_compare_register";
                }
                Wide::ByteExtended => {
                    let off = signed(((h & 1) << 8) | (((x >> 8) & 15) << 4) | (x & 15), 9) as u32;
                    mem = Some((
                        d,
                        s,
                        cpu.r[s].wrapping_add(off),
                        1,
                        h & 2 != 0,
                        h & 4 != 0,
                        if h & 8 != 0 {
                            Some(cpu.r[s].wrapping_add(off))
                        } else {
                            None
                        },
                    ));
                    op = "byte_extended";
                }
                Wide::WordExtended => {
                    let offset =
                        (signed(h & 7, 3) << 8) | (((x >> 8) & 15) << 4) as i32 | (x & 12) as i32;
                    let addr = cpu.r[s].wrapping_add(offset as u32);
                    let mode = x & 3;
                    mem = Some((
                        d,
                        s,
                        addr,
                        4,
                        mode & 1 != 0,
                        false,
                        if mode & 2 != 0 { Some(addr) } else { None },
                    ));
                    op = "word_extended";
                }
                Wide::WordRegisterPreincrementStore => {
                    let address = cpu.r[s].wrapping_add(cpu.r[c]);
                    cpu.write(address, cpu.r[d])?;
                    cpu.r[s] = address;
                    op = "word_register_preincrement_store";
                }
                Wide::WordRegisterPreincrement => {
                    // Vendor form: rD = [++rS=rC]. Commit the base before the
                    // destination so a load into its own base retains the loaded word.
                    let address = cpu.r[s].wrapping_add(cpu.r[c]);
                    let value = cpu.read(address, 4)?;
                    cpu.r[s] = address;
                    cpu.r[d] = value;
                    op = "word_register_preincrement";
                }
                Wide::HalfwordRegisterPreincrement => {
                    let address = cpu.r[s].wrapping_add(cpu.r[c]);
                    let value = cpu.read(address, 2)?;
                    cpu.r[s] = address;
                    cpu.r[d] = if x & 2 != 0 {
                        signed(value, 16) as u32
                    } else {
                        value
                    };
                    op = "halfword_register_preincrement";
                }
                Wide::HalfwordRegisterPreincrementStore => {
                    // Vendor objdump: EDDC 4651 is h[++r5=r6] = r4 and EDDC 4653
                    // is h[++r5=r6] = r4.h (SLOOP's sequencer event ring).
                    let address = cpu.r[s].wrapping_add(cpu.r[c]);
                    let value = if x & 2 != 0 { cpu.r[d] >> 16 } else { cpu.r[d] };
                    cpu.write_sized(address, value, 2)?;
                    cpu.r[s] = address;
                    op = "halfword_register_preincrement_store";
                }
                Wide::RegisterPostincrement => {
                    // ECDE/EDDE/EEDE [rS++=rC]: the access at rS, then rS += rC.
                    // x bits 0-1 (vendor objdump): word 2 load, 3 store; halfword
                    // 0 load, 1 store, 2 signed load, 3 store of the upper half;
                    // byte 0 load, 1 store, 2 signed load.
                    let size = match h {
                        0xecde => 4,
                        0xedde => 2,
                        _ => 1,
                    };
                    let address = cpu.r[s];
                    let updated = address.wrapping_add(cpu.r[c]);
                    if x & 1 != 0 {
                        let value = if size == 2 && x & 2 != 0 {
                            cpu.r[d] >> 16
                        } else {
                            cpu.r[d]
                        };
                        cpu.write_sized(address, value, size)?;
                        cpu.r[s] = updated;
                    } else {
                        let value = cpu.read(address, size)?;
                        cpu.r[s] = updated;
                        cpu.r[d] = if size != 4 && x & 2 != 0 {
                            signed(value, (size * 8) as u32) as u32
                        } else {
                            value
                        };
                    }
                    op = "register_postincrement";
                }
                Wide::WordPostincrementStore => {
                    let increment = word_increment(h, x);
                    let address = cpu.r[s];
                    mem = Some((
                        d,
                        s,
                        address,
                        4,
                        true,
                        false,
                        Some(address.wrapping_add(increment)),
                    ));
                    op = "word_postincrement_store";
                }
                Wide::WordPostincrementLoad => {
                    let increment = word_increment(h, x);
                    let address = cpu.r[s];
                    mem = Some((
                        d,
                        s,
                        address,
                        4,
                        false,
                        false,
                        Some(address.wrapping_add(increment)),
                    ));
                    op = "word_postincrement_load";
                }
                Wide::MemoryIndexed => {
                    let size = match h {
                        0xecd8 => 4,
                        0xedd8 => 2,
                        _ => 1,
                    };
                    let scaled = x & 8 != 0;
                    let mode = x & 7;
                    let addr = cpu.r[s].wrapping_add(cpu.r[c].wrapping_mul(if scaled {
                        size as u32
                    } else {
                        1
                    }));
                    mem = Some((
                        d,
                        s,
                        addr,
                        size,
                        if size == 4 { mode == 3 } else { mode == 1 },
                        mode == 2 && size != 4,
                        None,
                    ));
                    op = "memory_indexed";
                }
                Wide::HalfAddSubtract => {
                    let (a, b) = (half(cpu.r[s], x & 4 != 0), half(cpu.r[c], x & 2 != 0));
                    let value = if x & 1 == 0 { a + b } else { a - b };
                    cpu.r[d] = set_half(cpu.r[d], x & 8 != 0, value);
                    op = if x & 1 == 0 {
                        "half_add"
                    } else {
                        "half_subtract"
                    };
                }
                Wide::HalfMultiply => {
                    let (a, b) = (half(cpu.r[s], x & 4 != 0), half(cpu.r[c], x & 2 != 0));
                    let value = simd::mul16(a, b, h & 2 != 0);
                    cpu.r[d] = set_half(cpu.r[d], x & 8 != 0, value);
                    op = "half_multiply";
                }
                Wide::HalfMultiplyWord => {
                    let (a, b) = (half(cpu.r[s], x & 4 != 0), half(cpu.r[c], x & 2 != 0));
                    cpu.r[d] = simd::mul32(a, b, h & 2 != 0);
                    op = "half_multiply_word";
                }
                Wide::Pack => {
                    let (a, b) = (half(cpu.r[s], x & 4 != 0), half(cpu.r[c], x & 2 != 0));
                    cpu.r[d] = simd::join(a, b);
                    op = "pack";
                }
                Wide::DualAddSubtract => {
                    let (a0, a1) = simd::lanes(cpu.r[s], x & 2 != 0, x & 1 != 0);
                    let (b0, b1) = simd::lanes(cpu.r[c], h & 8 != 0, h & 4 != 0);
                    let first = if x & 8 == 0 { a0 + b0 } else { a0 - b0 };
                    let second = if x & 4 == 0 { a1 + b1 } else { a1 - b1 };
                    cpu.r[d] = simd::join(simd::sat16(first as i64), simd::sat16(second as i64));
                    op = "dual_add_subtract";
                }
                Wide::DualMultiply => {
                    let (a0, a1) = simd::lanes(cpu.r[s], x & 2 != 0, x & 1 != 0);
                    let (b0, b1) = simd::lanes(cpu.r[c], h & 8 != 0, h & 4 != 0);
                    let x2 = h & 2 != 0;
                    cpu.r[d] = simd::join(simd::mul16(a0, b0, x2), simd::mul16(a1, b1, x2));
                    op = "dual_multiply";
                }
                Wide::Unknown => return Ok(None),
            }
        }
        Extended::Unknown => return Ok(None),
    }
    if let Some((reg, base, address, size, store, sign, updated)) = mem {
        if store {
            cpu.write_sized(address, cpu.r[reg], size)?;
        } else {
            let value = cpu.read(address, size)?;
            cpu.r[reg] = if sign {
                signed(value, (size * 8) as u32) as u32
            } else {
                value
            };
        }
        if let Some(value) = updated {
            cpu.r[base] = value;
        }
    }
    Ok(Some((next, op)))
}
