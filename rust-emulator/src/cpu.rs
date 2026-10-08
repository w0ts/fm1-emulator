// SPDX-License-Identifier: GPL-3.0-only
// Probe decodings mirror emu.py, checked against vendor objdump and the FM-1.
// Startup-only additions use the pinned Quarkslab pi32v2 reference; see README.
use crate::decode::{Cache as DecodeCache, First, Wide};
use crate::{
    bus::{AccessFault, Bus},
    PROBE_RETURN, RESULT, USER_STACK,
};
use std::{fmt, io::Write};

mod spin_skip;

#[derive(Debug, PartialEq, Eq)]
pub enum Fault {
    Access { pc: u32, fault: AccessFault },
    Unsupported { pc: u32, word: u16 },
    Limit { pc: u32, limit: u64 },
    Preservation,
    Trace(String),
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    #[test]
    fn an_idle_core_does_not_stop_the_other_core_or_double_shared_time() {
        let mut c = Cpu::new(Bus::new(vec![1, 0, 0, 0]).unwrap(), crate::XIP);
        let entry = crate::RAM + 512;
        c.bus.write(0x10014, 6, 4).unwrap();
        c.bus.write(0x10808, u32::MAX, 4).unwrap();
        c.bus.write(0x10800, 9, 4).unwrap();
        c.bus.write(0x01c7fff8, entry, 4).unwrap();
        c.bus.write(0x1eee004, 8, 4).unwrap();
        for _ in 0..30 {
            c.step().unwrap();
        }
        assert_eq!(c.pc, crate::XIP + 2);
        assert!(c.idle);
        assert_eq!(c.secondary.as_ref().unwrap().pc, entry + 60);
        assert!(!c.secondary.as_ref().unwrap().idle);
        assert_eq!(c.bus.read(0x10804, 4).unwrap(), 2);
    }
    #[test]
    fn two_cores_share_elapsed_oscillator_time() {
        let mut c = Cpu::new(Bus::new(vec![0; 128]).unwrap(), crate::XIP);
        c.bus.write(0x10014, 6, 4).unwrap();
        c.bus.write(0x10808, u32::MAX, 4).unwrap();
        c.bus.write(0x10800, 9, 4).unwrap();
        c.bus.write(0x01c7fff8, crate::RAM + 512, 4).unwrap();
        c.bus.write(0x1eee004, 8, 4).unwrap();
        for _ in 0..30 {
            c.step().unwrap();
        }
        assert_eq!(c.steps, 60);
        assert_eq!(c.bus.read(0x10804, 4).unwrap(), 2);
    }
    #[test]
    fn lock_instructions_change_ownership_without_changing_registers() {
        let mut c = Cpu::new(Bus::new(vec![0x41, 0, 0x40, 0]).unwrap(), crate::XIP);
        let before = c.r;
        assert_eq!(c.step().unwrap(), "lockset");
        assert!(c.bus_locked);
        assert_eq!(c.step().unwrap(), "lockclr");
        assert!(!c.bus_locked);
        assert_eq!(c.r, before);
    }

    #[test]
    fn secondary_uses_the_guest_handoff_and_serializes_bus_locks() {
        let mut c = Cpu::new(Bus::new(vec![0; 32]).unwrap(), crate::XIP);
        let entry = crate::RAM + 512;
        c.bus.write(entry, 0x00400041, 4).unwrap(); // lockset; lockclr
        c.bus.write(0x01c7fff8, entry, 4).unwrap();
        c.bus.write(0x1eee004, 8, 4).unwrap();
        c.step().unwrap();
        assert_eq!(c.pc, crate::XIP + 2);
        let secondary = c.secondary.as_ref().unwrap();
        assert_eq!(secondary.pc, entry + 2);
        assert_eq!(secondary.sr[6], 1);
        assert!(secondary.bus_locked);
        c.step().unwrap(); // Secondary owns the bus until LOCKCLR.
        assert_eq!(c.pc, crate::XIP + 2);
        assert!(!c.secondary.as_ref().unwrap().bus_locked);
        c.step().unwrap();
        assert_eq!(c.pc, crate::XIP + 4);
        c.bus.write(0x1eee004, 2, 4).unwrap();
        c.step().unwrap();
        assert!(c.secondary.is_none());
    }

    #[test]
    fn software_interrupt_latches_are_shared_and_masks_are_per_core() {
        use crate::devices::IRQ_CONFIG;
        let mut c = Cpu::new(Bus::new(vec![0; 32]).unwrap(), crate::XIP);
        c.bus
            .write(IRQ_CONFIG + 0x200 + 15 * 4, 3 << 28, 4)
            .unwrap();
        c.bus.write(0x1eef3a0, 128, 4).unwrap();
        assert_eq!(c.bus.pending_irq_for(0x100, 0), None);
        assert_eq!(c.bus.pending_irq_for(0x100, 1), Some(127));
        assert_eq!(c.bus.read(0x1eef38c, 4).unwrap(), 0x80000000);
        c.bus.write(0x1eef3a4, 128, 4).unwrap();
        assert_eq!(c.bus.pending_irq_for(0x100, 1), None);
        c.bus.write(0x1eef1a0, 128, 4).unwrap();
        assert_eq!(c.bus.pending_irq_for(0x100, 1), Some(127));
        assert_eq!(c.bus.read(0x1eef18c, 4).unwrap(), 0);
        c.bus.write(IRQ_CONFIG + 15 * 4, 3 << 28, 4).unwrap();
        assert_eq!(c.bus.read(0x1eef18c, 4).unwrap(), 0x80000000);
        c.bus.write(0x1eef3a4, 128, 4).unwrap();
        assert_eq!(c.bus.pending_irq_for(0x100, 0), None);
    }

    #[test]
    fn paused_secondary_retains_context_until_resume() {
        let mut c = Cpu::new(Bus::new(vec![0; 32]).unwrap(), crate::XIP);
        let entry = crate::RAM + 512;
        c.bus.write(0x01c7fff8, entry, 4).unwrap();
        c.bus.write(0x1eee004, 8, 4).unwrap();
        c.step().unwrap();
        let paused_pc = c.secondary.as_ref().unwrap().pc;
        c.bus.write(0x1eee004, 12, 4).unwrap();
        assert_eq!(c.bus.read(0x1eee004, 4).unwrap() & 0x1c, 16);
        c.step().unwrap();
        c.step().unwrap();
        assert_eq!(c.secondary.as_ref().unwrap().pc, paused_pc);
        c.bus.write(0x1eee004, 24, 4).unwrap();
        c.step().unwrap();
        assert_eq!(c.secondary.as_ref().unwrap().pc, paused_pc + 2);
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Access { pc, fault } => write!(f, "at PC 0x{pc:08x}: {fault}"),
            Self::Unsupported { pc, word } => {
                write!(f, "unsupported instruction 0x{word:04x} at PC 0x{pc:08x}")
            }
            Self::Limit { pc, limit } => write!(f, "instruction limit {limit} at PC 0x{pc:08x}"),
            Self::Preservation => write!(f, "probe did not preserve registers or stack"),
            Self::Trace(message) => write!(f, "trace: {message}"),
        }
    }
}

/// Internal result: the fault is boxed so that results stay register-sized
/// on the per-instruction path; `step` unboxes it.
pub(crate) type Step<T> = Result<T, Box<Fault>>;

pub fn signed(value: u32, bits: u32) -> i32 {
    ((value << (32 - bits)) as i32) >> (32 - bits)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Repeat {
    start: u32,
    end: u32,
    iterations: u32,
    register: Option<(usize, u32)>,
}

/// Each register of `current` that differs from `before`, else the one of
/// `other`: a branch-free select over whole register files (vectorised),
/// where a register-by-register `if` mispredicts and spills.
#[inline(always)]
fn keep_changed(current: &[u32; 16], before: &[u32; 16], other: &[u32; 16]) -> [u32; 16] {
    std::array::from_fn(|i| {
        let changed = (current[i] != before[i]) as u32;
        let mask = changed.wrapping_neg();
        (current[i] & mask) | (other[i] & !mask)
    })
}

pub struct Cpu {
    decode: DecodeCache,
    blocks: crate::blocks::Cache,
    block_cursor: crate::blocks::Cursor,
    pub bus: Bus,
    pub r: [u32; 16],
    pub sr: [u32; 16],
    pub pc: u32,
    pub steps: u64,
    /// Instructions issued by each core (halted slots included); `steps` is
    /// their sum.
    pub core_steps: [u64; 2],
    /// `run_steps` jumps over spans in which every running core is halted
    /// by `idle` (true, the default); false steps each halted slot. The
    /// guest-visible state is the same either way.
    pub idle_skip: bool,
    /// Instruction slots in which a core was halted by `idle`, stepped or
    /// skipped, over both cores.
    pub idle_slots: u64,
    /// Of `idle_slots`, those jumped over without stepping.
    pub idle_skipped: u64,
    pub interrupts_enabled: bool,
    pub irq_entries: u64,
    /// CPU exceptions (vector 1) entered.
    pub exception_entries: u64,
    /// The PC of an instruction that raised a CPU exception in this step.
    exception: Option<u32>,
    /// Core and PC of the last CPU exception entered (diagnostics).
    last_exception: (usize, u32),
    in_interrupt: bool,
    predicate_skip: Option<(u32, u32, u32)>,
    irq_predicate: Option<(u32, u32, u32)>,
    repeat: Option<Repeat>,
    irq_repeat: Option<Repeat>,
    bus_locked: bool,
    idle: bool,
    idle_wake_delay: u8,
    secondary: Option<Core>,
    /// Opt-in, unmeasured: let an interrupt of a higher priority than the
    /// running handler's preempt it once the handler re-enabled interrupts.
    /// In this mode an entry clears the global enable (the handler's `sti`
    /// re-enables it), as the fork's pre-ICFG model did; the SLOOP and
    /// Melodee USB audio builds rely on preemption. Off by default: the
    /// physical FM-1 measurements behind the interrupt model do not nest.
    pub nested_irqs: bool,
    /// Preempted handlers, innermost last (`nested_irqs`).
    irq_nest: Vec<IrqFrame>,
    /// With `idle_skip`, also jump over spans in which a core busy-waits in
    /// a recorded spin loop (spin.rs). The guest-visible state is the same
    /// either way.
    pub spin_skip: bool,
    /// Instruction slots of each core in spin loops jumped over (also
    /// counted in `core_steps`).
    pub spin_skipped: [u64; 2],
    /// Print the loops accepted and rejected (diagnostics).
    pub spin_log: bool,
    spin: [spin_skip::SpinCore; 2],
    /// Calls of `step` until the next look for spin loops.
    spin_countdown: u32,
    /// While set, the executing instruction's data accesses are appended to
    /// `access_log` (the spin-loop recorder).
    logging: bool,
    access_log: std::cell::RefCell<Vec<crate::spin::Access>>,
}

/// What a nested interrupt entry keeps of the handler it preempts: its
/// ICFG and the block state of the code that handler interrupted.
struct IrqFrame {
    icfg: u32,
    predicate: Option<(u32, u32, u32)>,
    repeat: Option<Repeat>,
}

// Per-core context. Memory and devices remain on the one shared bus.
struct Core {
    block_cursor: crate::blocks::Cursor,
    r: [u32; 16],
    sr: [u32; 16],
    pc: u32,
    interrupts_enabled: bool,
    in_interrupt: bool,
    predicate_skip: Option<(u32, u32, u32)>,
    irq_predicate: Option<(u32, u32, u32)>,
    repeat: Option<Repeat>,
    irq_repeat: Option<Repeat>,
    bus_locked: bool,
    idle: bool,
    idle_wake_delay: u8,
    irq_nest: Vec<IrqFrame>,
}
impl Core {
    fn reset(pc: u32) -> Self {
        let mut sr = [0; 16];
        sr[6] = 1;
        Self {
            block_cursor: Default::default(),
            r: [0; 16],
            sr,
            pc,
            interrupts_enabled: false,
            // ROM's secondary handoff is in supervisor/interrupt context.
            in_interrupt: true,
            predicate_skip: None,
            irq_predicate: None,
            repeat: None,
            irq_repeat: None,
            bus_locked: false,
            idle: false,
            idle_wake_delay: 0,
            irq_nest: Vec::new(),
        }
    }
    fn swap(&mut self, cpu: &mut Cpu) {
        use std::mem::swap;
        swap(&mut self.block_cursor, &mut cpu.block_cursor);
        swap(&mut self.r, &mut cpu.r);
        swap(&mut self.sr, &mut cpu.sr);
        swap(&mut self.pc, &mut cpu.pc);
        swap(&mut self.interrupts_enabled, &mut cpu.interrupts_enabled);
        swap(&mut self.in_interrupt, &mut cpu.in_interrupt);
        swap(&mut self.predicate_skip, &mut cpu.predicate_skip);
        swap(&mut self.irq_predicate, &mut cpu.irq_predicate);
        swap(&mut self.repeat, &mut cpu.repeat);
        swap(&mut self.irq_repeat, &mut cpu.irq_repeat);
        swap(&mut self.bus_locked, &mut cpu.bus_locked);
        swap(&mut self.idle, &mut cpu.idle);
        swap(&mut self.idle_wake_delay, &mut cpu.idle_wake_delay);
        swap(&mut self.irq_nest, &mut cpu.irq_nest);
    }
}

impl Cpu {
    pub(crate) fn arithmetic(&mut self, left: u32, right: u32, subtract: bool, extra: u32) -> u32 {
        let (wide, signed_result, carry) = if subtract {
            let amount = right as u64 + extra as u64;
            (
                (left as u64).wrapping_sub(amount),
                left as i32 as i64 - right as i32 as i64 - extra as i64,
                left as u64 >= amount,
            )
        } else {
            let wide = left as u64 + right as u64 + extra as u64;
            (
                wide,
                left as i32 as i64 + right as i32 as i64 + extra as i64,
                wide > u32::MAX as u64,
            )
        };
        let result = wide as u32;
        let overflow = !(i32::MIN as i64..=i32::MAX as i64).contains(&signed_result);
        self.sr[5] = (self.sr[5] & !15)
            | overflow as u32
            | ((carry as u32) << 1)
            | (((result == 0) as u32) << 2)
            | ((result >> 31) << 3);
        result
    }
    pub fn new(bus: Bus, entry: u32) -> Self {
        Self {
            decode: DecodeCache::new(),
            blocks: crate::blocks::Cache::new(),
            bus,
            block_cursor: Default::default(),
            r: [0; 16],
            sr: [0; 16],
            pc: entry,
            steps: 0,
            core_steps: [0; 2],
            idle_skip: true,
            idle_slots: 0,
            idle_skipped: 0,
            interrupts_enabled: false,
            irq_entries: 0,
            exception_entries: 0,
            exception: None,
            last_exception: (0, 0),
            in_interrupt: false,
            predicate_skip: None,
            irq_predicate: None,
            repeat: None,
            irq_repeat: None,
            bus_locked: false,
            idle: false,
            idle_wake_delay: 0,
            secondary: None,
            nested_irqs: false,
            irq_nest: Vec::new(),
            spin_skip: true,
            spin_skipped: [0; 2],
            spin_log: false,
            spin: Default::default(),
            spin_countdown: spin_skip::POLL_STEPS,
            logging: false,
            access_log: Default::default(),
        }
    }

    // Guest accesses by the instruction at `self.pc`: SRAM and the XIP view
    // inline, the rest (devices, faults) out of line, so that no large
    // bus result is kept on the stack of the instruction functions.
    #[inline(always)]
    pub(crate) fn read(&self, address: u32, size: usize) -> Step<u32> {
        if self.logging {
            return self.read_logged(address, size);
        }
        match self.bus.read_fast(address, size) {
            Some(value) => Ok(value),
            None => self.read_slow(address, size),
        }
    }

    #[inline(never)]
    fn read_slow(&self, address: u32, size: usize) -> Step<u32> {
        self.bus
            .read(address, size)
            .map_err(|fault| Box::new(Fault::Access { pc: self.pc, fault }))
    }

    #[cold]
    #[inline(never)]
    fn read_logged(&self, address: u32, size: usize) -> Step<u32> {
        let value = self.read_slow(address, size)?;
        self.log_access(address, size, false, value);
        Ok(value)
    }

    #[inline(always)]
    pub(crate) fn fetch(&self, address: u32) -> Step<u32> {
        match self.bus.read_fast(address, 2) {
            Some(value) => Ok(value),
            None => self.fetch_slow(address),
        }
    }

    #[inline(never)]
    fn fetch_slow(&self, address: u32) -> Step<u32> {
        self.bus
            .fetch(address)
            .map(u32::from)
            .map_err(|fault| Box::new(Fault::Access { pc: self.pc, fault }))
    }

    #[inline(always)]
    pub(crate) fn write(&mut self, address: u32, value: u32) -> Step<()> {
        self.write_sized(address, value, 4)
    }

    #[inline(always)]
    pub(crate) fn write_sized(&mut self, address: u32, value: u32, size: usize) -> Step<()> {
        if self.logging {
            self.log_access(address, size, true, value);
        }
        if self.bus.write_fast(address, value, size) {
            return Ok(());
        }
        self.write_slow(address, value, size)
    }

    #[inline(never)]
    fn write_slow(&mut self, address: u32, value: u32, size: usize) -> Step<()> {
        let pc = self.pc;
        self.bus
            .write(address, value, size)
            .map_err(|fault| Box::new(Fault::Access { pc, fault }))
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, value: u32) -> Step<()> {
        let address = self.sr[14].wrapping_sub(4);
        self.write(address, value)?;
        self.sr[14] = address;
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn pop(&mut self) -> Step<u32> {
        let value = self.read(self.sr[14], 4)?;
        self.sr[14] = self.sr[14].wrapping_add(4);
        Ok(value)
    }

    /// Whether the primary core is halted in `idle`, waiting for an interrupt.
    pub fn halted(&self) -> bool {
        self.idle
    }

    /// Whether the primary core is running an interrupt handler.
    pub fn in_interrupt(&self) -> bool {
        self.in_interrupt
    }

    /// The secondary core's PC, once started (diagnostics).
    pub fn secondary_pc(&self) -> Option<u32> {
        self.secondary.as_ref().map(|core| core.pc)
    }

    /// Whether the secondary core, once started, is halted in `idle`.
    pub fn secondary_halted(&self) -> Option<bool> {
        self.secondary.as_ref().map(|core| core.idle)
    }

    /// Run at `mhz` instructions per microsecond of guest time instead of
    /// the firmware's system clock (`Bus::set_instruction_clock`).
    pub fn set_cpu_mhz(&mut self, mhz: u32) -> Result<(), String> {
        if !(1..=1000).contains(&mhz) {
            return Err(format!("CPU clock {mhz} MHz: use 1..1000"));
        }
        self.bus.set_instruction_clock(Some(mhz * 1_000_000));
        Ok(())
    }

    /// Execute through upstream's prepared basic blocks and their native
    /// code (`blocks`, `jit`) instead of decoding each instruction.
    /// Guest-visible behaviour is the same either way. Off by default:
    /// measured on Felucca, SLOOP and stock firmware, the lookups cost more
    /// than the prepared forms save while each native call still runs one
    /// instruction (PERFORMANCE.md).
    pub fn set_block_cache(&mut self, enabled: bool) {
        self.blocks.enabled = enabled;
    }

    /// Instructions `run_steps` runs between checks for halted cores.
    const HALT_CHECK_INTERVAL: u64 = 64;

    /// `count` calls of `step`, stopping at the first fault. Spans in which
    /// every running core is halted by `idle` and no interrupt can enter
    /// are jumped over (unless `idle_skip` is off): time, the step counters
    /// and the devices end up exactly as if each halted slot had stepped.
    pub fn run_steps(&mut self, count: u64) -> Result<(), Fault> {
        self.bus.devices_changed();
        let mut done = 0;
        while done < count {
            if self.may_skip() || self.secondary.as_ref().is_some_and(|core| core.idle) {
                let skipped = self.skip_idle_calls(count - done);
                if skipped > 0 {
                    done += skipped;
                    continue;
                }
            }
            // Checking for a halt only every few instructions keeps this
            // loop as tight as a plain one.
            let batch = (count - done).min(Self::HALT_CHECK_INTERVAL);
            for _ in 0..batch {
                self.step_cores().map_err(|fault| *fault)?;
            }
            done += batch;
        }
        Ok(())
    }

    /// While every core that would step is halted by `idle`, or busy-waits
    /// in a recorded spin loop (spin.rs, `spin_skip`), and no interrupt can
    /// enter, account up to `max_calls` calls of `step` at once, stopping
    /// before the call that reaches the next device event. Returns the
    /// calls skipped (0: step normally).
    pub fn skip_idle_calls(&mut self, max_calls: u64) -> u64 {
        if !self.idle_skip {
            return 0;
        }
        // Mirror `step`: which cores issue on the next call. A call that
        // starts or stops the secondary core is stepped normally.
        let control = self.bus.core_control(1);
        let secondary = self.secondary.as_ref();
        if (control & 2 != 0 && secondary.is_some()) || (secondary.is_none() && control & 10 == 8) {
            return 0;
        }
        let secondary_running = control & 0x18 == 8 && secondary.is_some();
        let secondary_alone = secondary_running
            && (self.bus.core_control(0) & 16 != 0
                || secondary.is_some_and(|core| core.bus_locked));
        let primary_steps = !secondary_alone;
        let secondary_steps = secondary_alone || (secondary_running && !self.bus_locked);
        let primary_quiet = self.idle
            && self.idle_wake_delay == 0
            && self.predicate_skip.is_none()
            && self.repeat.is_none();
        let secondary_quiet = secondary.is_some_and(|core| {
            core.idle
                && core.idle_wake_delay == 0
                && core.predicate_skip.is_none()
                && core.repeat.is_none()
        });
        // A core that is not halted may be in a spin loop (checked below).
        let primary_spins = primary_steps && !primary_quiet && self.spin_hint(0);
        let secondary_spins = secondary_steps && !secondary_quiet && self.spin_hint(1);
        if (primary_steps && !primary_quiet && !primary_spins)
            || (secondary_steps && !secondary_quiet && !secondary_spins)
        {
            return 0;
        }
        let spins = primary_spins || secondary_spins;
        if spins && self.bus.mmio_counting {
            return 0;
        }
        // A halted core that can take a pending interrupt wakes next slot; a
        // spinning one is not skipped while any source is pending for it.
        if self.bus.any_irq_pending() {
            if primary_steps
                && (primary_spins || (self.interrupts_enabled && !self.in_interrupt))
                && self
                    .bus
                    .pending_irq_for(self.sr[11], self.sr[6] as usize)
                    .is_some()
            {
                return 0;
            }
            if secondary_steps
                && secondary.is_some_and(|core| {
                    (secondary_spins || (core.interrupts_enabled && !core.in_interrupt))
                        && self
                            .bus
                            .pending_irq_for(core.sr[11], core.sr[6] as usize)
                            .is_some()
                })
            {
                return 0;
            }
        }
        // A spin loop that writes a timer leaves the next tick exact, only
        // to recompute the device event times: do that now.
        if spins && self.bus.reschedule().is_err() {
            return 0;
        }
        // Exactly one issuing core carries guest time per call.
        let calls = self.bus.issues_before_event().min(max_calls);
        if calls == 0 {
            return 0;
        }
        // Where each spinning core is, and what the span does to it.
        let outcomes = if spins {
            let Some(outcomes) =
                self.spin_outcomes(primary_spins, secondary_spins, secondary_alone, calls)
            else {
                return 0;
            };
            Some(outcomes)
        } else {
            None
        };
        self.bus.skip_issues(calls);
        let primary_halted = primary_steps && !primary_spins;
        let secondary_halted = secondary_steps && !secondary_spins;
        let slots = calls * (u64::from(primary_steps) + u64::from(secondary_steps));
        let halted_slots = calls * (u64::from(primary_halted) + u64::from(secondary_halted));
        self.steps += slots;
        self.core_steps[0] += if primary_steps { calls } else { 0 };
        self.core_steps[1] += if secondary_steps { calls } else { 0 };
        self.idle_slots += halted_slots;
        self.idle_skipped += halted_slots;
        if let Some(outcomes) = outcomes {
            self.spin_apply(outcomes, calls);
        }
        calls
    }

    pub fn step(&mut self) -> Result<&'static str, Fault> {
        self.bus.devices_changed();
        self.step_cores().map_err(|fault| *fault)
    }

    /// `step` for a caller that has not changed device state through the
    /// bus's public fields (`devices`, `lcd`, `audio`, ...) since its last
    /// call of `step`, `step_next` or `run_steps`: the interrupt check then
    /// need not look at every source again while none is pending.
    pub fn step_next(&mut self) -> Result<&'static str, Fault> {
        self.step_cores().map_err(|fault| *fault)
    }

    #[inline(always)]
    fn step_cores(&mut self) -> Step<&'static str> {
        self.spin_countdown -= 1;
        if self.spin_countdown == 0 {
            return self.step_cores_polling();
        }
        self.step_cores_with::<false>()
    }

    /// A call of `step` that looks for spin loops first, and records a
    /// core's instructions while `spin_poll` has it recording.
    #[cold]
    #[inline(never)]
    fn step_cores_polling(&mut self) -> Step<&'static str> {
        self.spin_poll();
        self.step_cores_with::<true>()
    }

    #[inline(always)]
    fn step_cores_with<const RECORD: bool>(&mut self) -> Step<&'static str> {
        let control = self.bus.core_control(1);
        if control & 2 != 0 {
            self.secondary = None;
        }
        if self.secondary.is_none() && control & 10 == 8 {
            // SPL's RAM handoff vector, written by the unchanged stock guest.
            let entry = self.read(0x01c7fff8, 4)?;
            self.bus
                .fetch(entry)
                .map_err(|fault| Fault::Access { pc: self.pc, fault })?;
            self.secondary = Some(Core::reset(entry));
        }
        let secondary_running = control & 0x18 == 8 && self.secondary.is_some();
        if secondary_running
            && (self.bus.core_control(0) & 16 != 0
                || self.secondary.as_ref().is_some_and(|core| core.bus_locked))
        {
            return self.step_secondary::<RECORD>(true);
        }
        let op = if RECORD && self.spin_recording(0) {
            self.step_core_recording(0, true)?
        } else {
            self.step_core(true)?
        };
        self.core_steps[0] += 1;
        if !self.bus_locked && secondary_running {
            self.step_secondary::<RECORD>(false)?;
        }
        Ok(op)
    }

    fn step_secondary<const RECORD: bool>(&mut self, advance_time: bool) -> Step<&'static str> {
        let mut secondary = self.secondary.take().unwrap();
        secondary.swap(self);
        let result = if RECORD && self.spin_recording(1) {
            self.step_core_recording(1, advance_time)
        } else {
            self.step_core(advance_time)
        };
        secondary.swap(self);
        self.secondary = Some(secondary);
        self.core_steps[1] += 1;
        result
    }

    fn step_core(&mut self, advance_time: bool) -> Step<&'static str> {
        if let Some((_, at, end)) = self.predicate_skip {
            if self.pc == at {
                self.pc = end;
                self.predicate_skip = None;
            }
        }
        let pc = self.pc;
        if self.bus.mmio_counting {
            self.bus.pc_hint.set(pc);
            self.bus.step_hint.set(self.steps);
        }
        let op = if self.idle {
            // Keep shared hardware time and the other core running while this
            // core waits. IRQ entry resumes at the instruction after IDLE.
            self.idle_slots += 1;
            "idle_wait"
        } else {
            let h = self.fetch(pc)?;
            if h >> 13 == 6 || h & 0xf800 == 0xf000 {
                self.execute_bundle(pc, h)?
            } else if self.blocks.enabled {
                self.execute_prepared(pc, h)?
            } else {
                self.execute(h)?
            }
        };
        if let Some(at) = self.exception.take() {
            self.enter_exception(at)?;
        }
        // FM-1_988: conditional bundles finish and skip their unselected
        // arm before a pending interrupt can enter.
        if let Some((start, at, end)) = self.predicate_skip {
            if self.pc == at {
                self.pc = end;
                self.predicate_skip = None;
            } else if (start..at).contains(&pc)
                && !(start..=at).contains(&self.pc)
                && !op.starts_with("call")
            {
                // The arm left its block (`if (c) { ...; rts }`, a goto
                // out): it never reaches its end, so nothing is skipped
                // and interrupts must not stay deferred.
                self.predicate_skip = None;
            }
        }
        if let Some(mut repeat) = self.repeat {
            if self.pc == repeat.end {
                if let Some((register, count)) = repeat.register {
                    // Hardware writes back its captured counter after each
                    // iteration, even when the body overwrote that register.
                    self.r[register] = count - 1;
                    repeat.register = Some((register, count - 1));
                }
                if repeat.iterations > 1 {
                    self.pc = repeat.start;
                    repeat.iterations -= 1;
                    self.repeat = Some(repeat);
                } else {
                    self.repeat = None;
                }
            }
        }
        self.steps += 1;
        let ticks = if advance_time {
            self.bus.instruction_ticks()
        } else {
            0
        };
        if ticks != 0 {
            self.bus
                .advance_time(ticks)
                .map_err(|fault| Fault::Access { pc, fault })?;
        }
        self.idle_wake_delay = self.idle_wake_delay.saturating_sub(1);
        self.dispatch_interrupt()?;
        Ok(op)
    }

    /// A parallel bundle at `pc` (first word `h`): the following slot,
    /// then the primary slot, both reading the incoming registers.
    #[inline(never)]
    fn execute_bundle(&mut self, pc: u32, h: u32) -> Step<&'static str> {
        let length = if h >> 13 == 6 { 2 } else { 4 };
        let normalized = if length == 2 { h & 0x1fff } else { h & !0x1000 };
        self.pc = pc + length;
        let following = self.read(self.pc, 2)?;
        let before = self.r;
        let specials_before = self.sr;
        self.execute(following)?;
        let following_registers = self.r;
        let following_specials = self.sr;
        let continuation = self.pc;
        self.r = before;
        self.sr = specials_before;
        self.pc = pc;
        let op = self.execute(normalized)?;
        // Both slots read the incoming registers. Compiler bundles have
        // distinct destinations; retain writes from the following slot
        // where the primary slot did not change that register.
        self.r = keep_changed(&self.r, &before, &following_registers);
        self.sr = keep_changed(&self.sr, &specials_before, &following_specials);
        self.pc = continuation;
        Ok(op)
    }

    /// The instruction at `pc` through the block cache and JIT
    /// (`set_block_cache`), or the interpreter where they do not apply.
    #[inline(never)]
    fn execute_prepared(&mut self, pc: u32, h: u32) -> Step<&'static str> {
        let prepared = if self
            .blocks
            .can_execute(&self.decode, self.block_cursor, pc, h as u16)
        {
            self.blocks.instruction(
                &self.bus,
                &mut self.decode,
                &mut self.block_cursor,
                pc,
                h as u16,
            )
        } else {
            None
        };
        let Some(prepared) = prepared else {
            return self.execute(h);
        };
        let instruction = prepared.instruction;
        if let Some(native) = prepared.native {
            native.run(&mut self.r, &mut self.sr);
            self.pc = pc.wrapping_add(instruction.length as u32);
            Ok(instruction.name)
        } else {
            instruction.execute(self)
        }
    }

    pub(crate) fn conditional(&mut self, test: bool, counts: u32) -> Step<u32> {
        use crate::decode::skip_length;
        let mut cursor = self.pc + 4;
        let mut then_end = cursor;
        let then_count = (counts >> 14) + 1;
        let else_count = (counts >> 12) & 3;
        for i in 0..then_count + else_count {
            let h = self.read(cursor, 2)?;
            cursor += skip_length(h);
            // A parallel pair counts as one conditional instruction bundle.
            if h >> 13 == 6 || h & 0xf800 == 0xf000 {
                cursor += skip_length(self.read(cursor, 2)?);
            }
            if i + 1 == then_count {
                then_end = cursor;
            }
        }
        if test {
            self.predicate_skip = Some((self.pc + 4, then_end, cursor));
            Ok(self.pc + 4)
        } else {
            Ok(then_end)
        }
    }

    pub(crate) fn decoded_wide(&mut self, h: u32, x: u32) -> Wide {
        self.decode.wide(h, x)
    }

    /// Execute the instruction word `h` at `self.pc`. The extended forms (most
    /// of the compiler's code) go straight to `extended::execute`.
    #[inline(always)]
    fn execute(&mut self, h: u32) -> Step<&'static str> {
        let kind = self.decode.first(h);
        if let First::Extended(extended) = kind {
            let pc = self.pc;
            let Some((destination, name)) = crate::extended::execute(self, h, pc, extended)? else {
                return Err(Fault::Unsupported { pc, word: h as u16 }.into());
            };
            self.pc = destination;
            return Ok(name);
        }
        self.execute_base(h, kind)
    }

    #[inline(never)]
    fn execute_base(&mut self, h: u32, kind: First) -> Step<&'static str> {
        let pc = self.pc;
        let a = (h & 7) as usize;
        let b = ((h >> 4) & 7) as usize;
        let mut next = pc.wrapping_add(2);
        let op;
        match kind {
            First::MoveImmediate32 => {
                let value = self.read(pc + 2, 2)? | (self.read(pc + 4, 2)? << 16);
                let n = (h & 15) as usize;
                if h & 0xfff0 == 0xffc0 {
                    self.r[n] = value;
                    op = "mov_imm32";
                } else if matches!(n, 0 | 12 | 13 | 14) {
                    self.sr[n] = value;
                    op = "stack_imm32";
                } else {
                    return Err(Fault::Unsupported { pc, word: h as u16 }.into());
                }
                next = pc + 6;
            }
            First::RepeatRegister => {
                if self.repeat.is_some() {
                    return Err(Fault::Unsupported { pc, word: h as u16 }.into());
                }
                let register = (h & 15) as usize;
                let length = (((h >> 4) & 15) + 1) * 2;
                let count = self.r[register];
                if count == 0 {
                    next = pc + 2 + length;
                } else {
                    // Measured on FM-1: 31 executes 31 times; 32 executes once
                    // leaving 31; 63 executes 32 times leaving 31. Compiler loops
                    // reissue REP until the remaining register count reaches zero.
                    let iterations = if count < 32 { count } else { (count & 31) + 1 };
                    self.repeat = Some(Repeat {
                        start: pc + 2,
                        end: pc + 2 + length,
                        iterations,
                        register: Some((register, count)),
                    });
                }
                op = "repeat_register";
            }
            First::RepeatImmediate => {
                if self.repeat.is_some() {
                    return Err(Fault::Unsupported { pc, word: h as u16 }.into());
                }
                let length = (((h >> 4) & 15) + 1) * 2;
                let count = ((h >> 8) & 31) + 1;
                self.repeat = Some(Repeat {
                    start: pc + 2,
                    end: pc + 2 + length,
                    iterations: count,
                    register: None,
                });
                op = "repeat_immediate";
            }
            First::MovSpecial => {
                let extra = self.read(pc + 2, 2)?;
                let reg = ((extra >> 12) & 15) as usize;
                let special = ((extra >> 8) & 15) as usize;
                // Deliberately exclude PC writes and unrecognized reserved encodings.
                if special == 15 || !matches!(extra & 255, 0 | 128) {
                    return Err(Fault::Unsupported { pc, word: h as u16 }.into());
                }
                if extra & 255 == 128 {
                    self.sr[special] = self.r[reg];
                    if special == 11 {
                        self.interrupts_enabled = self.sr[11] & 0x200 != 0;
                    }
                } else {
                    self.r[reg] = self.sr[special];
                }
                next = pc + 4;
                op = "mov_special";
            }
            First::MovMask => {
                let extra = self.read(pc + 2, 2)?;
                let mode = (extra >> 10) & 3;
                if mode == 0 && extra & 0x0f00 > 0x0300 {
                    return Err(Fault::Unsupported { pc, word: h as u16 }.into());
                }
                self.r[((extra >> 12) & 15) as usize] = crate::extended::packed(extra);
                next = pc + 4;
                op = "mov_mask";
            }
            First::MovImm16 => {
                self.r[(h & 15) as usize] = signed(self.read(pc + 2, 2)?, 16) as u32;
                next = pc + 4;
                op = "mov_imm16";
            }
            First::MovImm8 => {
                self.r[a] = (((h >> 3) & 7) << 5) | ((h >> 8) & 31);
                op = "mov_imm8";
            }
            First::MovNegative => {
                self.r[a] = 0xffff_ffe0 | ((h >> 8) & 31);
                op = "mov_negative";
            }
            First::MovReg => {
                self.r[(h & 15) as usize] = self.r[((h >> 4) & 15) as usize];
                op = "mov_reg";
            }
            First::Arithmetic => {
                let c = (((h >> 7) & 3) * 2 + ((h >> 3) & 1)) as usize;
                if h & 0xfe00 == 0x1e00 {
                    self.r[a] = self.arithmetic(self.r[b], self.r[c], true, 0);
                    op = "sub";
                } else {
                    self.r[a] = self.arithmetic(self.r[b], self.r[c], false, 0);
                    op = "add";
                }
            }
            First::AddImm8 => {
                let imm = signed((((h >> 3) & 7) << 5) | ((h >> 8) & 31), 8);
                self.r[a] = self.arithmetic(self.r[a], imm as u32, false, 0);
                op = "add_imm8";
            }
            First::AddSp => {
                let imm = (signed((h >> 5) & 7, 3) << 7) | (((h >> 8) & 31) << 2) as i32;
                self.sr[14] = self.sr[14].wrapping_add(imm as u32);
                op = "add_sp";
            }
            First::AddSmall => {
                self.r[a] = self.arithmetic(self.r[b], (h >> 8) & 31, false, 0);
                op = "add_small";
            }
            First::Logic => match h & 0xff88 {
                0x1900 => {
                    self.r[a] |= self.r[b];
                    op = "or";
                }
                0x1908 => {
                    self.r[a] ^= self.r[b];
                    op = "xor";
                }
                0x1980 => {
                    self.r[a] &= self.r[b];
                    op = "and";
                }
                _ => {
                    self.r[a] = !self.r[b];
                    op = "not";
                }
            },
            First::Asr => {
                self.r[a] = ((self.r[b] as i32) >> ((h >> 8) & 31)) as u32;
                op = "asr";
            }
            First::ShiftImmediate => {
                let shift = (h >> 8) & 31;
                if h & 0x80 != 0 {
                    self.r[a] = self.r[b] >> shift;
                    op = "lsr";
                } else {
                    self.r[a] = self.r[b] << shift;
                    op = "lsl";
                }
            }
            First::MemoryWord => {
                let address = self.r[b].wrapping_add((signed((h >> 8) & 31, 5) * 4) as u32);
                if h & 0x80 != 0 {
                    self.write(address, self.r[a])?;
                    op = "store32";
                } else {
                    self.r[a] = self.read(address, 4)?;
                    op = "load32";
                }
            }
            First::StackMask => {
                let mask = self.read(pc + 2, 2)?;
                next = pc + 4;
                if h & 4 == 0 {
                    if h & 1 != 0 {
                        self.push(self.sr[3])?;
                    }
                    for n in (0..16).rev() {
                        if mask & (1 << n) != 0 {
                            self.push(self.r[n])?;
                        }
                    }
                    op = "push_mask";
                } else {
                    for n in 0..16 {
                        if mask & (1 << n) != 0 {
                            self.r[n] = self.pop()?;
                        }
                    }
                    if h & 1 != 0 {
                        next = self.pop()?;
                    }
                    op = "pop_mask";
                }
            }
            First::PushRegs => {
                let boundary = (h & 15) as usize;
                let range = if boundary < 4 {
                    boundary..=3
                } else {
                    4..=boundary
                };
                for n in range.rev() {
                    self.push(self.r[n])?;
                }
                op = "push_regs";
            }
            First::PopPc => {
                next = self.pop()?;
                op = "pop_pc";
            }
            First::PushRets => {
                self.push(self.sr[3])?;
                op = "push_rets";
            }
            First::PushSpecial => {
                // Mask bit n is sr[n] (reti, rete, retx, rets, sr4, psr); the
                // lowest register ends at the lowest address, as the measured
                // 04e1/04e8/04e9 interrupt frames.
                for n in (0..6).rev() {
                    if h & (1 << n) != 0 {
                        self.push(self.sr[n])?;
                    }
                }
                op = "push_special";
            }
            First::PopSpecial => {
                for n in 0..6 {
                    if h & (1 << n) != 0 {
                        self.sr[n] = self.pop()?;
                    }
                }
                op = "pop_special";
            }
            First::PopRegs => {
                let boundary = (h & 15) as usize;
                let range = if boundary < 4 {
                    boundary..=3
                } else {
                    4..=boundary
                };
                for n in range {
                    self.r[n] = self.pop()?;
                }
                op = "pop_regs";
            }
            First::PushRetsRegs => {
                self.push(self.sr[3])?;
                for n in (4..=(h & 15) as usize).rev() {
                    self.push(self.r[n])?;
                }
                op = "push_rets_regs";
            }
            First::PopRetsRegs => {
                for n in 4..=(h & 15) as usize {
                    self.r[n] = self.pop()?;
                }
                self.sr[3] = self.pop()?;
                op = "pop_rets_regs";
            }
            First::PopPcRegs => {
                for n in 4..=(h & 15) as usize {
                    self.r[n] = self.pop()?;
                }
                next = self.pop()?;
                op = "pop_pc_regs";
            }
            First::MoveStackPointer => {
                match h {
                    0x1440 => self.sr[14] = self.sr[12],
                    0x1441 => self.sr[14] = self.sr[13],
                    0x1442 => self.sr[12] = self.sr[14],
                    _ => self.sr[13] = self.sr[14],
                }
                op = "move_stack_pointer";
            }
            First::CallRel32 => {
                // Vendor startup uses a signed byte displacement after a 6-byte call.
                let displacement = self.read(pc + 2, 2)? | (self.read(pc + 4, 2)? << 16);
                self.sr[3] = pc + 6;
                next = (pc + 6).wrapping_add(displacement);
                op = "call_rel32";
            }
            First::Relative22 => {
                let displacement = signed(((h & 63) << 16) | self.read(pc + 2, 2)?, 22) * 2;
                if h & 0xffc0 == 0xea80 {
                    self.sr[3] = pc + 4;
                    op = "call_rel22";
                } else {
                    op = "goto_rel22";
                }
                next = (pc + 4).wrapping_add(displacement as u32);
            }
            First::CallRel9 => {
                // Vendor assembler's short relative call (signed 9-bit byte offset).
                let displacement = signed((((h >> 4) & 7) << 6) | (((h >> 8) & 31) << 1), 9);
                self.sr[3] = pc + 2;
                next = (pc + 2).wrapping_add(displacement as u32);
                op = "call_rel9";
            }
            First::GotoRel12 => {
                let displacement = signed(
                    ((h & 3) << 10) | (((h >> 4) & 15) << 6) | (((h >> 8) & 31) << 1),
                    12,
                );
                next = (pc + 2).wrapping_add(displacement as u32);
                op = "goto_rel12";
            }
            First::BranchZero => {
                let displacement = signed((((h >> 4) & 7) << 6) | (((h >> 8) & 31) << 1), 9);
                let nonzero = h & 0x80 != 0;
                if (self.r[a] != 0) == nonzero {
                    next = (pc + 2).wrapping_add(displacement as u32);
                }
                op = if nonzero {
                    "branch_nonzero"
                } else {
                    "branch_zero"
                };
            }
            First::TestsetByte => {
                let address = self.r[(h & 15) as usize];
                let old = self.read(address, 1)?;
                self.write_sized(address, 0xff, 1)?;
                // FM-1_982 physical probe: the old byte's low nibble is copied
                // into the four PSR condition bits, not a comparison result.
                self.sr[5] = (self.sr[5] & !15) | (old & 15);
                op = "testset_byte";
            }
            First::Return => {
                next = self.sr[3];
                op = "return";
            }
            First::CallReg => {
                self.sr[3] = pc + 2;
                next = self.r[(h & 15) as usize];
                op = "call_reg";
            }
            First::Rti if !self.irq_nest.is_empty() => {
                // Back into the preempted handler (nested_irqs): same stack,
                // still in interrupt context, its ICFG and block state.
                let outer = self.irq_nest.pop().unwrap();
                next = self.sr[0];
                self.predicate_skip = self.irq_predicate.take();
                self.repeat = self.irq_repeat.take();
                self.irq_predicate = outer.predicate;
                self.irq_repeat = outer.repeat;
                self.sr[11] = outer.icfg;
                self.interrupts_enabled = outer.icfg & 0x200 != 0;
                op = "rti";
            }
            First::Rti if self.in_interrupt => {
                next = self.sr[0];
                self.sr[13] = self.sr[14];
                self.sr[14] = self.sr[12];
                self.in_interrupt = false;
                self.predicate_skip = self.irq_predicate.take();
                self.repeat = self.irq_repeat.take();
                self.interrupts_enabled = true;
                self.sr[11] = (self.sr[11] & !255) | 0x600;
                op = "rti";
            }
            First::Cli => {
                self.interrupts_enabled = false;
                self.sr[11] &= !0x200;
                op = "cli";
            }
            First::BusLock => {
                // CPU bus ownership latch. With one executing core acquisition
                // cannot contend; this does not replace the guest's memory locks.
                self.bus_locked = h == 0x0041;
                op = if self.bus_locked {
                    "lockset"
                } else {
                    "lockclr"
                };
            }
            First::Sti => {
                self.interrupts_enabled = true;
                self.sr[11] |= 0x200;
                op = "sti";
            }
            First::Idle => {
                self.idle = true;
                op = "idle";
            }
            First::SyncOrNop => {
                op = if h == 0x0020 { "csync" } else { "nop" };
            }
            First::Rti => return Err(Fault::Unsupported { pc, word: h as u16 }.into()),
            First::Extended(extended) => {
                let Some((destination, name)) = crate::extended::execute(self, h, pc, extended)?
                else {
                    return Err(Fault::Unsupported { pc, word: h as u16 }.into());
                };
                next = destination;
                op = name;
            }
        }
        self.pc = next;
        Ok(op)
    }

    /// A divide by zero at `pc`. With EMU_CON bit 2 set (vendor debug.c
    /// debug_init sets BIT(2); emu_msg bit 2 is "div0_err") it latches
    /// EMU_MSG bit 2 and returns true: the instruction leaves its
    /// destination unwritten and vector 1 is entered after it.
    pub(crate) fn divide_by_zero(&mut self, pc: u32) -> Step<bool> {
        let core = self.sr[6] as usize;
        if self.bus.emu_con(core) & 4 == 0 {
            return Ok(false);
        }
        self.bus.raise_emu_msg(core, 4);
        self.exception = Some(pc);
        Ok(true)
    }

    /// Enter the CPU exception (source 1, vendor IRQ_EXCEPTION_IDX) raised
    /// by the instruction at `at`. It is synchronous: unlike an interrupt
    /// it does not wait for a handler, a repeat or a conditional block to
    /// end (X0X's float divide-by-zero crash on hardware was inside its
    /// audio interrupt). reti holds the faulting instruction, which the
    /// SDK's exception report prints as the crash address; whether the
    /// hardware stores it or the next PC is unmeasured. ICFG takes the
    /// FM-1_989 interrupt-entry layout.
    fn enter_exception(&mut self, at: u32) -> Step<()> {
        const SOURCE: usize = 1;
        let core = self.sr[6] as usize;
        let Some(priority) = self.bus.devices.irq_priority_for(SOURCE, self.sr[11], core) else {
            return Err(Box::new(Fault::Access {
                pc: at,
                fault: AccessFault {
                    address: at,
                    size: 4,
                    operation: "divide-by-zero exception",
                    reason: "exception vector 1 is not enabled; undelivered entry is unmeasured",
                },
            }));
        };
        let handler = self.read(0x01c7_fe00 + SOURCE as u32 * 4, 4)?;
        self.bus
            .fetch(handler)
            .map_err(|fault| Fault::Access { pc: at, fault })?;
        self.sr[0] = at;
        if !self.in_interrupt {
            self.sr[12] = self.sr[14];
            self.sr[14] = self.sr[13];
        }
        self.pc = handler;
        self.sr[11] = (self.sr[11] & !0x077f04ff)
            | ((SOURCE as u32) << 16)
            | (priority << 24)
            | (1 << priority);
        self.in_interrupt = true;
        self.idle = false;
        self.irq_predicate = self.predicate_skip.take();
        self.irq_repeat = self.repeat.take();
        self.exception_entries += 1;
        self.last_exception = (core, at);
        Ok(())
    }

    /// Core and PC of the instruction that raised the last CPU exception.
    pub fn last_exception(&self) -> (usize, u32) {
        self.last_exception
    }

    fn dispatch_interrupt(&mut self) -> Step<()> {
        if !self.interrupts_enabled
            || (self.in_interrupt && !self.nested_irqs)
            || self.repeat.is_some()
            || self.predicate_skip.is_some()
        {
            return Ok(());
        }
        // Nothing to deliver while no source is pending (the common case).
        if !self.bus.irq_possible() {
            return Ok(());
        }
        if let Some(source) = self.bus.pending_irq_for(self.sr[11], self.sr[6] as usize) {
            if self.idle {
                self.idle = false;
                // FM-1_996: wake executes four CSYNC instructions before
                // TIMER3 enters; immediate CLI after IDLE prevents entry.
                // This measured wake latency is in nominal issue slots.
                self.idle_wake_delay = 4;
                return Ok(());
            }
            if self.idle_wake_delay != 0 {
                return Ok(());
            }
            let priority = self
                .bus
                .devices
                .irq_priority_for(source, self.sr[11], self.sr[6] as usize)
                .unwrap();
            // nested_irqs: only a higher priority than the running
            // handler's (ICFG bits 24-26) preempts it.
            let nesting = self.in_interrupt;
            if nesting && priority <= (self.sr[11] >> 24) & 7 {
                return Ok(());
            }
            let handler = self.read(0x01c7_fe00 + source as u32 * 4, 4)?;
            self.bus
                .fetch(handler)
                .map_err(|fault| Fault::Access { pc: self.pc, fault })?;
            let active = if nesting {
                self.irq_nest.push(IrqFrame {
                    icfg: self.sr[11],
                    predicate: self.irq_predicate.take(),
                    repeat: self.irq_repeat.take(),
                });
                self.sr[11] & 255
            } else {
                self.sr[12] = self.sr[14];
                self.sr[14] = self.sr[13];
                0
            };
            self.sr[0] = self.pc;
            self.pc = handler;
            // FM-1_989: ICFG records source/priority above the active
            // priority bitmap. Entry preserves the global enable and clears
            // thread mode; INTPRI is a guest mask, not the active priority.
            self.sr[11] = (self.sr[11] & !0x077f04ff)
                | ((source as u32) << 16)
                | (priority << 24)
                | (1 << priority)
                | active;
            if self.nested_irqs {
                self.interrupts_enabled = false;
                self.sr[11] &= !0x200;
            }
            self.in_interrupt = true;
            self.idle = false;
            self.irq_predicate = self.predicate_skip.take();
            self.irq_repeat = self.repeat.take();
            self.irq_entries += 1;
        }
        Ok(())
    }

    pub fn run(
        &mut self,
        stop: Option<u32>,
        limit: u64,
        mut trace: Option<&mut dyn Write>,
    ) -> Result<(), Fault> {
        while stop != Some(self.pc) {
            if self.steps >= limit {
                return Err(Fault::Limit { pc: self.pc, limit });
            }
            let pc = self.pc;
            let op = self.step()?;
            if let Some(writer) = trace.as_mut() {
                writeln!(
                    writer,
                    "{{\"pc\":{pc},\"op\":\"{op}\",\"next_pc\":{},\"registers\":{:?},\"sp\":{}}}",
                    self.pc, self.r, self.sr[14]
                )
                .map_err(|error| Fault::Trace(error.to_string()))?;
            }
        }
        Ok(())
    }

    pub fn probe(&mut self, limit: u64, trace: Option<&mut dyn Write>) -> Result<[u32; 12], Fault> {
        self.r = std::array::from_fn(|n| 0x1020_3040 + n as u32 * 0x0101_0101);
        self.r[0] = RESULT;
        let before = self.r;
        self.sr[14] = USER_STACK;
        self.sr[3] = PROBE_RETURN;
        self.run(Some(PROBE_RETURN), limit, trace)?;
        if self.r[1..] != before[1..] || self.sr[14] != USER_STACK {
            return Err(Fault::Preservation);
        }
        let mut values = [0; 12];
        for (i, value) in values.iter_mut().enumerate() {
            *value = self
                .read(RESULT + i as u32 * 4, 4)
                .map_err(|fault| *fault)?;
        }
        Ok(values)
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    fn compare(a: &mut Cpu, b: &mut Cpu) {
        assert_eq!(a.step(), b.step());
        assert_eq!(a.r, b.r);
        assert_eq!(a.sr, b.sr);
        assert_eq!(a.pc, b.pc);
        assert_eq!(a.steps, b.steps);
        assert_eq!(a.irq_entries, b.irq_entries);
        assert_eq!(a.bus.system.watchdog_feeds, b.bus.system.watchdog_feeds);
        for offset in (0..128).step_by(4) {
            assert_eq!(
                a.bus.read(crate::RAM + offset, 4),
                b.bus.read(crate::RAM + offset, 4)
            );
        }
    }
    fn pair(words: &[u16]) -> (Cpu, Cpu) {
        let bytes: Vec<_> = words.iter().flat_map(|h| h.to_le_bytes()).collect();
        let mut a = Cpu::new(Bus::new(bytes.clone()).unwrap(), crate::XIP);
        a.set_block_cache(true);
        let b = Cpu::new(Bus::new(bytes).unwrap(), crate::XIP);
        (a, b)
    }
    #[test]
    fn prepared_instructions_match_the_reference_with_aliases_flags_and_faults() {
        let mut seed = 0x981093u32;
        let mut words = Vec::new();
        let decode = DecodeCache::new();
        // Cover all short register/operand combinations in the prepared families.
        for h in 0..=65535u32 {
            if matches!(
                decode.first(h),
                First::Arithmetic
                    | First::Logic
                    | First::MovReg
                    | First::Extended(crate::decode::Extended::Multiply)
                    | First::Extended(crate::decode::Extended::Extend)
            ) {
                words.push(h as u16);
            }
        }
        words.extend([
            0x2040, 0x20c0, 0x8008, 0xa000, 0xa080, 0xa088, 0x6000, 0x6080, 0x8004, 0x8001, 0x4000,
            0x4080,
        ]);
        for h in words {
            let (mut a, mut b) = pair(&[h, 0x4321, 0x8765]);
            for n in 0..16 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                a.r[n] = seed;
                a.sr[n] = seed.rotate_left(13);
            }
            b.r = a.r;
            b.sr = a.sr;
            compare(&mut a, &mut b);
            // Revisit the cached instruction with different dynamic operands.
            for _ in 0..40 {
                a.pc = crate::XIP;
                b.pc = crate::XIP;
                a.r = a.r.map(|v| !v);
                b.r = a.r;
                compare(&mut a, &mut b);
            }
        }
        for h in [
            0xe1e4, 0xe0b4, 0xe190, 0xe1c0, 0xe1c4, 0xffc8, 0xffee, 0xe04f,
        ] {
            for x in [0, 0x4003, 0x5430, 0x5432, 0x5433, 0x87ff, 0xf000, 0xffff] {
                let (mut a, mut b) = pair(&[h, x, 0x1234]);
                a.r = std::array::from_fn(|n| 0x80000001u32.wrapping_mul(n as u32 + 1));
                a.sr[5] = 0xabcdfff0;
                b.r = a.r;
                b.sr = a.sr;
                for _ in 0..40 {
                    a.pc = crate::XIP;
                    b.pc = crate::XIP;
                    compare(&mut a, &mut b);
                }
            }
        }
    }
    #[test]
    fn cached_blocks_preserve_timer_interrupts_and_repeat_boundaries() {
        let (mut a, mut b) = pair(&[0x2540, 0x0300, 0x1631, 0x2040, 0x8004, 0x0400]);
        for c in [&mut a, &mut b] {
            c.sr[11] = 0x100;
            c.sr[13] = crate::SYSTEM_STACK;
            c.sr[14] = crate::USER_STACK;
            c.interrupts_enabled = true;
            c.bus.write(crate::RAM, 0x0081, 2).unwrap(); // RTI.
            c.bus
                .write(crate::devices::IRQ_CONFIG + 7 * 4, 1 << 28, 4)
                .unwrap();
            c.bus.write(0x01c7fe00 + 63 * 4, crate::RAM, 4).unwrap();
            c.bus.write(crate::devices::TIMER5 + 8, 1, 4).unwrap();
            c.bus.write(crate::devices::TIMER5, 0x4009, 4).unwrap();
        }
        for _ in 0..100 {
            compare(&mut a, &mut b);
        }
        assert!(a.irq_entries > 0);
    }
    #[test]
    fn decoding_ahead_does_not_raise_a_future_fetch_fault() {
        let (mut a, mut b) = pair(&[0x2040]);
        compare(&mut a, &mut b);
        assert_eq!(a.pc, crate::XIP + 2);
        compare(&mut a, &mut b);
    }
}

#[cfg(test)]
mod jit_tests {
    use super::*;
    #[test]
    fn native_arithmetic_matches_all_condition_flag_edges_and_operand_aliases() {
        for subtract in [false, true] {
            for destination in [1, 2, 3] {
                let bytes: Vec<_> = [
                    0xe0b4u16,
                    (destination << 12) | 0x0210 | if subtract { 2 } else { 0 },
                ]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect();
                let mut a = Cpu::new(Bus::new(bytes.clone()).unwrap(), crate::XIP);
                a.set_block_cache(true);
                let mut b = Cpu::new(Bus::new(bytes).unwrap(), crate::XIP);
                for _ in 0..64 {
                    a.pc = crate::XIP;
                    a.step().unwrap();
                }
                let calls = a.blocks.native_calls;
                assert!(calls > 0);
                for left in [0, 1, u32::MAX, 0x80000000, 0x7fffffff, 0x80000001] {
                    for right in [0, 1, u32::MAX, 0x80000000, 0x7fffffff, 0x80000001] {
                        a.pc = crate::XIP;
                        b.pc = crate::XIP;
                        a.r[1] = left;
                        a.r[2] = right;
                        a.sr[5] = 0xaabbccf0;
                        b.r = a.r;
                        b.sr = a.sr;
                        assert_eq!(a.step(), b.step());
                        assert_eq!(a.r, b.r);
                        assert_eq!(a.sr, b.sr);
                        assert_eq!(a.pc, b.pc);
                    }
                }
                assert!(a.blocks.native_calls > calls);
            }
        }
    }
    #[test]
    fn hot_native_code_revalidates_sram_operands_and_flash_permissions() {
        let mut c = Cpu::new(Bus::new(vec![0xc0, 0x20, 0, 0]).unwrap(), crate::XIP);
        c.set_block_cache(true);
        for _ in 0..64 {
            c.pc = crate::XIP;
            c.step().unwrap();
        }
        assert!(
            c.blocks.native_calls > 0,
            "the test must execute native code"
        );
        c.bus.write(0x40200, 0, 4).unwrap();
        c.pc = crate::XIP;
        assert!(matches!(c.step(), Err(Fault::Access { .. })));
        c.bus.write(crate::RAM, 0x4003e1e4, 4).unwrap();
        for _ in 0..64 {
            c.pc = crate::RAM;
            c.r[4] = 7;
            c.step().unwrap();
            assert_eq!(c.r[4], 21);
        }
        let calls = c.blocks.native_calls;
        assert!(calls > 32);
        c.bus.write(crate::RAM + 2, 0x4005, 2).unwrap();
        c.pc = crate::RAM;
        c.r[4] = 7;
        c.step().unwrap();
        assert_eq!(c.r[4], 35);
        assert_eq!(
            c.blocks.native_calls, calls,
            "changed code must leave the old translation"
        );
    }
}
