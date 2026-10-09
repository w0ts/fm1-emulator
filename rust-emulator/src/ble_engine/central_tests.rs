// SPDX-License-Identifier: GPL-3.0-only
//! The central role (states 3 and 6) against a scripted firmware that
//! programs the engine the way the fact sheet's §21.3 / §21.4 / §8.2 say,
//! and a virtual BLE-MIDI peripheral on the air.

use super::*;
use crate::ble_air::ConnectParams;
use crate::ble_peripheral::{Peripheral, PeripheralConfig};
use crate::{RAM, RAM_SIZE};
use std::collections::VecDeque;

const BASE: u32 = RAM + 0x1_0000;
const BLOCK: u32 = 0x40;
const RXP: [u16; 2] = [0x400, 0x600];
const TXP: [u16; 2] = [0x800, 0x900];
const OWN: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0xC6];

struct Fw {
    engine: Engine,
    ram: Vec<u8>,
    params: ConnectParams,
    /// The master is programmed (the event IRQ after the CONNECT_IND).
    master: bool,
    first_rx: bool,
    rx_next: u32,
    rx: Vec<Vec<u8>>,
    queue: VecDeque<Vec<u8>>,
    loaded: [bool; 2],
    acked: u32,
    channel: u8,
    events: u32,
    /// (instant, new interval) of an update the firmware started.
    update: Option<(u16, u16)>,
}

impl Fw {
    fn w(&mut self) -> Window<'_> {
        Window {
            ram: &mut self.ram,
            base: BASE,
            end: BASE + 0x1000,
        }
    }
    fn set(&mut self, f: u32, v: u16) {
        Block(BLOCK).set(&mut self.w(), f, v).unwrap();
    }
    fn get(&mut self, f: u32) -> u16 {
        Block(BLOCK).get(&self.w(), f).unwrap()
    }
    fn set8(&mut self, f: u32, v: u8) {
        Block(BLOCK).set8(&mut self.w(), f, v).unwrap();
    }
    fn get8(&mut self, f: u32) -> u8 {
        Block(BLOCK).get8(&self.w(), f).unwrap()
    }
    fn col(&mut self, c: u32, v: u32) {
        self.engine.write(0x28020, v, 4).unwrap();
        self.engine.write(0x2801C, c << 10 | 5, 4).unwrap();
    }
    fn col_rd(&mut self, c: u32) -> u32 {
        self.engine.write(0x2801C, c << 10 | 2, 4).unwrap();
        self.engine.read(0x28024, 4).unwrap()
    }
    fn addr(&mut self, f: u32, a: &[u8; 6]) {
        for i in 0..3 {
            self.set(
                f + 2 * i as u32,
                u16::from_le_bytes([a[2 * i], a[2 * i + 1]]),
            );
        }
    }

    /// §21.3 "Engine programming" in the vendor's order, link 0.
    fn initiate(config: EngineConfig, peripheral: Peripheral, params: ConnectParams) -> Self {
        let target = peripheral.address;
        let mut fw = Fw {
            engine: Engine {
                config,
                ..Engine::default()
            },
            ram: vec![0; RAM_SIZE],
            params,
            master: false,
            first_rx: false,
            rx_next: 0,
            rx: Vec::new(),
            queue: VecDeque::new(),
            loaded: [false; 2],
            acked: 0,
            channel: 37,
            events: 0,
            update: None,
        };
        fw.engine.peripherals.push(peripheral);
        fw.engine.set_ram_window(Some(BASE), Some(BASE + 0x1000));
        fw.w().set_u16(0, 0x8000 | BLOCK as u16).unwrap();
        for c in 0..17 {
            fw.col(c, 0);
        }
        fw.set(field::RXMAXBUF, 255);
        fw.col(8, 0);
        fw.set(field::WINCNTL0, 37_500);
        fw.set(field::WINCNTL1, 0);
        fw.col(1, 64);
        fw.col(15, 0);
        fw.col(6, 0x2100 | 37);
        fw.addr(field::LOCALADR, &OWN);
        fw.set(field::FORMAT, 0x0008);
        fw.addr(field::WHITELIST0, &target);
        fw.set(field::FILTERCNTL, 1 | 1 << 8 | 0x10);
        fw.addr(field::TARGETADR, &target);
        for b in 0..2u32 {
            fw.set(field::RXPTR0 + 2 * b, RXP[b as usize]);
            fw.set(field::TXPTR0 + 2 * b, TXP[b as usize]);
            let mut p = OWN.to_vec();
            p.extend(target);
            p.extend(params.lldata());
            fw.w().set_bytes(TXP[b as usize] as u32, &p).unwrap();
            fw.set(field::TXAHDR0 + 2 * b, 5 | 1 << 4 | 1 << 5);
            fw.set(field::TXDHDR0 + 2 * b, 34 << 8);
        }
        fw.set(field::TXTOG, 0);
        fw.col(2, 0x3000);
        fw.engine.write(0x28028, 0x101, 4).unwrap();
        for (c, v) in [(7, 0), (0, 0), (14, 0), (0, 0), (14, 0x8000), (9, 1)] {
            fw.col(c, v);
        }
        fw
    }

    /// §21.3 "Switch to master" in the event interrupt after the CONNECT_IND.
    fn program_master(&mut self) {
        let p = self.params;
        let tog = self.get(field::TXTOG);
        self.set(field::TXTOG, tog & !2);
        self.set(field::RXTOG, 0);
        self.set(field::BDADDR0, p.access_address as u16);
        self.set(field::BDADDR1, (p.access_address >> 16) as u16);
        self.set(field::CRCWORD0, p.crc_init as u16);
        self.set(field::CRCWORD1, (p.crc_init >> 16) as u16);
        self.set8(field::TXBUF0CNTL, 1);
        self.set8(field::TXBUF0CNTL + 1, 1);
        self.col(5, 0);
        self.col(4, 0);
        let start = 2 * p.win_offset as u32 + 3;
        for (c, v) in [
            (7, 0),
            (0, 0),
            (14, 0),
            (0, start),
            (14, 0x8000),
            (0, start),
            (14, 0x8000),
        ] {
            self.col(c, v);
        }
        self.col(2, 0x6000);
        self.col(8, 0);
        self.col(3, 0);
        self.set(field::EVTCOUNT, 0);
        let win = p.win_size as u32 * 1250 + 1250;
        self.set(field::WINCNTL0, win as u16);
        self.set(field::WINCNTL1, (win >> 16) as u16);
        self.col(1, 2 * p.interval as u32);
        self.col(15, 0x8000);
        let used = crate::ble_air::used_channels(p.channel_map);
        self.set(field::CHMAP0, p.channel_map as u16);
        self.set(field::CHMAP1, (p.channel_map >> 16) as u16);
        self.set(
            field::CHMAP2,
            ((p.channel_map >> 32) & 31) as u16 | (used.len() as u16) << 5,
        );
        for (i, c) in used.iter().enumerate() {
            self.set8(field::FRQ_IDX1 + i as u32, *c);
        }
        self.col(6, 0x8000 | (p.hop as u32) << 8 | p.hop as u32);
        let t = (self.get(field::TXTOG) & 1) as u32;
        self.set(field::TXAHDR0, 0);
        self.set(field::TXAHDR0 + 2, 0);
        self.set(field::TXDHDR0 + 2 * t, 1);
        self.set(field::TXDHDR0 + 2 * (t ^ 1), 5);
        self.master = true;
    }

    /// The §8.2 TX service: the TXTOG buffer, then the other.
    fn tx_service(&mut self) {
        let t = (self.get(field::TXTOG) & 1) as u32;
        for b in [t, t ^ 1] {
            if self.get8(field::TXBUF0CNTL + b) & 1 == 0 {
                continue;
            }
            if self.loaded[b as usize] {
                self.loaded[b as usize] = false;
                self.acked += 1;
            }
            let Some(p) = self.queue.pop_front() else {
                continue;
            };
            self.w().set_bytes(TXP[b as usize] as u32, &p[2..]).unwrap();
            let keep = self.get(field::TXDHDR0 + 2 * b) & 4;
            self.set(
                field::TXDHDR0 + 2 * b,
                keep | (p[1] as u16) << 8 | (p[0] & 3) as u16,
            );
            self.set8(field::TXBUF0CNTL + b, 0);
            self.loaded[b as usize] = true;
        }
    }

    fn rx_isr(&mut self) {
        if !self.master {
            return; // initiating: the advertising report (not needed here)
        }
        let b = self.rx_next;
        if self.get8(field::RXBUF0CNTL + b) & 1 == 0 {
            return;
        }
        if !self.first_rx {
            self.first_rx = true;
            self.set(field::WINCNTL0, 0);
            self.set(field::WINCNTL1, 0);
        }
        let dh = self.get(field::RXDHDR0 + 2 * b);
        let len = (dh >> 8) as u32;
        let mut pdu = vec![(dh & 0x1F) as u8, len as u8];
        pdu.extend(self.w().bytes(RXP[b as usize] as u32, len).unwrap());
        self.set8(field::RXBUF0CNTL + b, 0);
        self.rx_next ^= 1;
        if len > 0 {
            self.rx.push(pdu);
        }
        self.tx_service();
    }

    fn event_isr(&mut self) {
        if !self.master {
            if self.engine.stats.connect_inds_sent > 0 {
                self.program_master();
            } else {
                self.channel = if self.channel >= 39 {
                    37
                } else {
                    self.channel + 1
                };
                self.col(6, 0x2100 | self.channel as u32);
            }
            return;
        }
        self.events += 1;
        let counter = (self.col_rd(3) as u16).wrapping_sub(1);
        if let Some((instant, iv)) = self.update {
            if counter.wrapping_add(1) == instant {
                self.col(4, 0x8000 | iv as u32); // 2 x (iv / 2)
                self.col(1, 2 * iv as u32);
                self.col(15, 0x8000);
                self.update = None;
            }
        }
    }

    /// Run until `ticks` pass, taking interrupts as the firmware would.
    fn run(&mut self, ticks: u64) {
        let end = self.engine.now + ticks;
        while self.engine.now < end {
            let step = self
                .engine
                .ticks_to_event()
                .unwrap_or(u64::MAX)
                .min(end - self.engine.now);
            self.engine.advance(step as u32, &mut self.ram).unwrap();
            if self.engine.rx_irq() {
                self.engine.write(0x2802C, 0x100, 4).unwrap();
                self.rx_isr();
            }
            if self.engine.event_irq() {
                self.engine.write(0x2802C, 1, 4).unwrap();
                self.event_isr();
            }
        }
    }

    fn send_ll(&mut self, p: &[u8]) {
        let mut v = vec![3, p.len() as u8];
        v.extend_from_slice(p);
        self.queue.push_back(v);
    }

    fn peripheral(&self) -> &Peripheral {
        &self.engine.peripherals[0]
    }
}

fn params() -> ConnectParams {
    ConnectParams {
        access_address: 0x71A4_C35E,
        crc_init: 0x00C3_9A17,
        win_size: 2,
        win_offset: 6,
        interval: 9,
        latency: 0,
        timeout: 200,
        hop: 11,
        ..ConnectParams::default()
    }
}

fn keyboard() -> Peripheral {
    Peripheral::new(PeripheralConfig::new("KeyStep 37"))
}

const MS: u64 = 24_000;

#[test]
fn initiating_sends_the_connect_ind_and_the_master_runs_its_events() {
    let mut fw = Fw::initiate(EngineConfig::default(), keyboard(), params());
    fw.run(400 * MS);
    let s = &fw.engine.stats;
    assert_eq!(s.connect_inds_sent, 1);
    assert!(s.init_windows >= 1);
    assert!(
        fw.master,
        "the event IRQ after the CONNECT_IND programmed the master"
    );
    assert!(
        s.master_events > 20 && s.master_rx >= s.master_events - 1,
        "{s:?}"
    );
    let r = &fw.peripheral().report;
    assert_eq!(r.connections, 1);
    assert_eq!(r.first_in_window, Some(true));
    // 2 x WinOffset + 4 slots from the counter's start, which is the
    // CONNECT_IND's end: 1.25 ms into the 2.5 ms transmit window.
    assert!(
        (1_240..=1_260).contains(&r.first_offset_us),
        "{}",
        r.first_offset_us
    );
    assert!(fw.peripheral().connected());
    // The firmware's LL_VERSION_IND goes out of a TX buffer, is acknowledged
    // (bit0 back to 1), and the peripheral's comes back in an RX buffer.
    fw.send_ll(&[0x0C, 9, 0xFF, 0xFF, 1, 0]);
    fw.tx_service();
    fw.run(100 * MS);
    assert_eq!(fw.acked, 1);
    assert!(
        fw.rx.iter().any(|p| p[0] & 3 == 3 && p[2] == 0x0C),
        "{:?}",
        fw.rx
    );
    assert!(fw.peripheral().report.ll_seen.contains(&"LL_VERSION_IND"));
}

#[test]
fn a_master_connection_update_moves_both_sides_at_the_instant() {
    let mut fw = Fw::initiate(EngineConfig::default(), keyboard(), params());
    fw.run(200 * MS);
    let counter = (fw.col_rd(3) as u16).wrapping_sub(1);
    let instant = counter.wrapping_add(10);
    let iv = 12u16;
    let mut ind = vec![0x00, 1];
    ind.extend((iv / 2).to_le_bytes());
    ind.extend(iv.to_le_bytes());
    ind.extend(0u16.to_le_bytes());
    ind.extend(200u16.to_le_bytes());
    ind.extend(instant.to_le_bytes());
    fw.send_ll(&ind);
    fw.col(5, instant as u32);
    fw.update = Some((instant, iv));
    fw.tx_service();
    let before = fw.engine.stats.master_rx;
    fw.run(600 * MS);
    let r = &fw.peripheral().report;
    assert_eq!(r.conn_updates, 1);
    assert!(fw.peripheral().connected(), "{:?}", r.ended);
    let after = fw.engine.stats.master_rx - before;
    // 600 ms at 15 ms after the instant: about 40 more replies, every event heard.
    assert!((35..=60).contains(&after), "{after}");
    assert!(fw
        .engine
        .notes()
        .iter()
        .any(|n| n.contains("master instant")));
}

#[test]
fn without_the_engines_connect_ind_nothing_connects() {
    let config = EngineConfig {
        init_cind: false,
        ..EngineConfig::default()
    };
    let mut fw = Fw::initiate(config, keyboard(), params());
    fw.run(300 * MS);
    assert_eq!(fw.engine.stats.connect_inds_sent, 0);
    assert!(fw.engine.stats.init_windows > 3);
    assert_eq!(fw.peripheral().report.connections, 0);
    assert!(!fw.master);
}

#[test]
fn stopping_the_master_ends_the_peripherals_link_by_supervision() {
    let mut fw = Fw::initiate(EngineConfig::default(), keyboard(), params());
    fw.run(200 * MS);
    assert!(fw.peripheral().connected());
    fw.col(14, 0);
    fw.run(2_500 * MS);
    let r = &fw.peripheral().report;
    assert_eq!(r.ended, ["supervision timeout"]);
    assert!(r.adv_pdus > 3, "advertising again");
}

#[test]
fn a_first_anchor_after_the_window_is_never_heard() {
    // WinOffset 6 but the counter started 4 slots late (as if the event
    // interrupt came 2.5 ms after the CONNECT_IND): outside the window.
    let config = EngineConfig {
        master_anchor_from_counter: true,
        ..EngineConfig::default()
    };
    let mut fw = Fw::initiate(config, keyboard(), params());
    fw.params.win_offset += 2;
    fw.run(300 * MS);
    let r = &fw.peripheral().report;
    assert_eq!(r.established, 0);
    assert!(
        r.ended.iter().any(|e| e.starts_with("never established")),
        "{r:?}"
    );
}

#[test]
fn a_late_event_interrupt_puts_the_unadjusted_first_anchor_after_the_window() {
    // The FM-1's (blell-dev4): the event interrupt 4.8 ms after the
    // CONNECT_IND; a firmware that writes 2 x WinOffset + 3 regardless
    // misses the transmit window, as the hardware did (0x3E).
    let config = EngineConfig {
        init_evt_delay_us: 4_800,
        ..EngineConfig::default()
    };
    let mut fw = Fw::initiate(config, keyboard(), params());
    fw.run(300 * MS);
    assert!(fw.master);
    let r = &fw.peripheral().report;
    assert_eq!(r.first_in_window, None, "{r:?}");
    assert_eq!(r.established, 0);
    assert!(
        r.ended.iter().any(|e| e.starts_with("never established")),
        "{r:?}"
    );
}

#[test]
fn the_engine_reads_busy_for_a_while_after_a_stop() {
    let config = EngineConfig {
        busy_after_stop_us: 900,
        ..EngineConfig::default()
    };
    let mut fw = Fw::initiate(config, keyboard(), params());
    fw.run(5 * MS);
    assert_eq!(
        fw.engine.read(0x28038, 4).unwrap() & 2,
        0,
        "idle while it runs"
    );
    fw.col(14, 0);
    assert_eq!(fw.engine.read(0x28038, 4).unwrap() & 2, 2, "busy at once");
    fw.engine.advance(800 * 24, &mut fw.ram).unwrap();
    assert_eq!(
        fw.engine.read(0x28038, 4).unwrap() & 2,
        2,
        "busy 0.8 ms later"
    );
    fw.engine.advance(200 * 24, &mut fw.ram).unwrap();
    assert_eq!(
        fw.engine.read(0x28038, 4).unwrap() & 2,
        0,
        "idle after 0.9 ms"
    );
    assert_eq!(fw.engine.stats.busy_stops, 1);
}

#[test]
fn a_lost_connect_ind_leaves_the_peripheral_advertising() {
    let config = EngineConfig {
        cind_lost: 1,
        ..EngineConfig::default()
    };
    let mut fw = Fw::initiate(config, keyboard(), params());
    fw.run(300 * MS);
    assert_eq!(fw.engine.stats.connect_inds_sent, 1);
    assert_eq!(fw.engine.stats.cind_lost, 1);
    let r = &fw.peripheral().report;
    assert_eq!(r.connections, 0, "{r:?}");
    assert!(!fw.peripheral().connected());
    let mut c = EngineConfig::default();
    c.apply("cind_lost=random,seed=8").unwrap();
    assert_eq!(c.cind_lost_n(), 2);
    c.apply("seed=0").unwrap();
    assert_eq!(c.cind_lost_n(), 1);
}
