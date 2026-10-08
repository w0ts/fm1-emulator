// SPDX-License-Identifier: GPL-3.0-only
//! Interrupt-handler timing on CPU 0 (diagnostics): per-source handler
//! durations in emulated instructions and guest time, and for the BLE
//! interrupts (fact sheet §10: 45 event, 29 RX) the delay from the engine
//! raising the request to the handler's entry, with the part of it spent
//! in the audio handler (IRQ 11).
//!
//! Durations are measured in CPU 0 instruction issues: the emulator issues
//! one instruction per core clock cycle, so they are emulated cycles.
//! They are guest behaviour under the emulator's timing model, not
//! hardware measurements.

use crate::cpu::Cpu;
use std::collections::{BTreeMap, VecDeque};

pub const AUDIO_IRQ: usize = 11;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandlerStats {
    pub count: u64,
    pub instructions: u64,
    pub max_instructions: u64,
    pub ticks: u64,
    pub max_ticks: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DelayStats {
    pub count: u64,
    pub total_ticks: u64,
    pub max_ticks: u64,
    /// The longest part of one delay spent in the audio handler.
    pub max_audio_ticks: u64,
    /// Requests whose delay overlapped an audio handler.
    pub delayed_by_audio: u64,
}

#[derive(Default)]
pub struct IsrProfiler {
    entries: u64,
    current: Option<(usize, u64, u64)>,
    pub handlers: BTreeMap<usize, HandlerStats>,
    pub delays: BTreeMap<usize, DelayStats>,
    audio: VecDeque<(u64, u64)>,
}

impl IsrProfiler {
    /// Call after every `Cpu::step`/`step_next`.
    pub fn observe(&mut self, cpu: &Cpu) {
        let tick = cpu.bus.oscillator_ticks();
        let steps = cpu.core_steps[0];
        let entered = cpu.irq_entries != self.entries;
        self.entries = cpu.irq_entries;
        let inside = cpu.in_interrupt();
        if let Some((source, start_steps, start_tick)) = self.current {
            if !inside || entered {
                self.finish(
                    source,
                    steps - start_steps,
                    tick - start_tick,
                    start_tick,
                    tick,
                );
                self.current = None;
            }
        }
        if inside && entered {
            let source = ((cpu.sr[11] >> 16) & 0x7F) as usize;
            self.current = Some((source, steps, tick));
            self.entered(cpu, source, tick);
        }
    }

    fn finish(&mut self, source: usize, instructions: u64, ticks: u64, start: u64, end: u64) {
        let h = self.handlers.entry(source).or_default();
        h.count += 1;
        h.instructions += instructions;
        h.max_instructions = h.max_instructions.max(instructions);
        h.ticks += ticks;
        h.max_ticks = h.max_ticks.max(ticks);
        if source == AUDIO_IRQ {
            self.audio.push_back((start, end));
            if self.audio.len() > 64 {
                self.audio.pop_front();
            }
        }
    }

    fn entered(&mut self, cpu: &Cpu, source: usize, tick: u64) {
        let rx = match source {
            crate::ble_engine::RX_IRQ => true,
            crate::ble_engine::EVENT_IRQ => false,
            _ => return,
        };
        let Some(raised) = cpu.bus.ble_engine().raised_at(rx) else {
            return;
        };
        let delay = tick.saturating_sub(raised);
        let audio: u64 = self
            .audio
            .iter()
            .map(|&(s, e)| e.min(tick).saturating_sub(s.max(raised)))
            .sum();
        let d = self.delays.entry(source).or_default();
        d.count += 1;
        d.total_ticks += delay;
        d.max_ticks = d.max_ticks.max(delay);
        d.max_audio_ticks = d.max_audio_ticks.max(audio);
        d.delayed_by_audio += (audio > 0) as u64;
    }

    /// A text report: one line per handler, then the BLE delays.
    pub fn report(&self) -> String {
        let us = |t: u64| t as f64 / 24.0;
        let mut out = String::from("ISR durations on CPU 0 (instructions = emulated cycles):\n");
        for (source, h) in &self.handlers {
            out += &format!(
                "  IRQ {source:3}: {:8} runs, mean {:7.0} instr ({:7.1} us), max {:7} instr ({:7.1} us)\n",
                h.count,
                h.instructions as f64 / h.count.max(1) as f64,
                us(h.ticks) / h.count.max(1) as f64,
                h.max_instructions,
                us(h.max_ticks)
            );
        }
        for (source, d) in &self.delays {
            out += &format!(
                "  IRQ {source:3} entry delay: {} entries, mean {:.1} us, max {:.1} us; \
                 {} overlapped audio, longest audio share {:.1} us\n",
                d.count,
                us(d.total_ticks) / d.count.max(1) as f64,
                us(d.max_ticks),
                d.delayed_by_audio,
                us(d.max_audio_ticks)
            );
        }
        out
    }
}
