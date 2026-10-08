// SPDX-License-Identifier: GPL-3.0-only
//! The engine model against a test "driver" that programs it the way the
//! fact sheet's §6/§7 sequences do, and the virtual central on the air.

use super::*;
use crate::ble_air::ConnectParams;
use crate::ble_central::{CentralConfig, Step};
use crate::{RAM, RAM_SIZE};

const BASE: u32 = RAM + 0x1_0000;
const BLOCK: u32 = 0x40;
const ADV_ADDRESS: [u8; 6] = [0x44, 0x02, 0x85, 0xF4, 0x06, 0xD9];

struct Rig {
    engine: Engine,
    ram: Vec<u8>,
}

impl Rig {
    fn w(&mut self) -> Window<'_> {
        Window {
            ram: &mut self.ram,
            base: BASE,
            end: BASE + 0x1000,
        }
    }
    fn set(&mut self, field: u32, value: u16) {
        Block(BLOCK).set(&mut self.w(), field, value).unwrap();
    }
    fn get(&mut self, field: u32) -> u16 {
        Block(BLOCK).get(&self.w(), field).unwrap()
    }
    fn set8(&mut self, field: u32, value: u8) {
        Block(BLOCK).set8(&mut self.w(), field, value).unwrap();
    }
    fn get8(&mut self, field: u32) -> u8 {
        Block(BLOCK).get8(&self.w(), field).unwrap()
    }
    fn column(&mut self, column: u32, value: u32) {
        self.engine.write(0x28020, value, 4).unwrap();
        self.engine.write(0x2801C, column << 10 | 5, 4).unwrap();
    }
    /// Run the engine for `ticks`, stopping at each of its events.
    fn run(&mut self, ticks: u64) {
        let end = self.engine.now + ticks;
        while self.engine.now < end {
            let step = self
                .engine
                .ticks_to_event()
                .unwrap_or(u64::MAX)
                .min(end - self.engine.now);
            let ram = &mut self.ram;
            self.engine.advance(step as u32, ram).unwrap();
        }
    }
    fn central(&self) -> &Central {
        self.engine.central().unwrap()
    }

    /// The §6 advertising set-up of stock V15 (values from its dump).
    fn advertising(steps: Vec<Step>, config: EngineConfig) -> Self {
        let mut rig = Rig {
            engine: Engine {
                config,
                ..Engine::default()
            },
            ram: vec![0; RAM_SIZE],
        };
        rig.engine.set_ram_window(Some(BASE), Some(BASE + 0x1000));
        rig.w().set_u16(0, 0x8000 | BLOCK as u16).unwrap();
        rig.set(field::ADVIDX, 0xC4E2);
        rig.set(field::FORMAT, 0x000C);
        rig.set(field::OPTCNTL, 0x0228);
        rig.set(field::BDADDR0, 0xBED6);
        rig.set(field::BDADDR1, 0x8E89);
        rig.set(field::CRCWORD0, 0x5555);
        rig.set(field::CRCWORD1, 0x0055);
        for (i, (tx, rx)) in [(0x800u16, 0x400u16), (0x900, 0x600)]
            .into_iter()
            .enumerate()
        {
            let i = i as u32;
            rig.set(field::TXPTR0 + 2 * i, tx);
            rig.set(field::RXPTR0 + 2 * i, rx);
        }
        rig.set(field::RXMAXBUF, 255);
        rig.set(field::WINCNTL0, 50);
        for i in 0..3 {
            let half = u16::from_le_bytes([ADV_ADDRESS[2 * i], ADV_ADDRESS[2 * i + 1]]);
            rig.set(field::LOCALADR + 2 * i as u32, half);
        }
        let mut adv = ADV_ADDRESS.to_vec();
        adv.extend([2, 1, 6]);
        rig.w().set_bytes(0x800, &adv).unwrap();
        rig.set(field::TXAHDR0, 0);
        rig.set(field::TXDHDR0, (adv.len() as u16) << 8);
        let mut rsp = ADV_ADDRESS.to_vec();
        rsp.extend([5, 9, b'F', b'M', b'-', b'1']);
        rig.w().set_bytes(0x900, &rsp).unwrap();
        rig.set(field::TXAHDR0 + 2, 4);
        rig.set(field::TXDHDR0 + 2, (rsp.len() as u16) << 8);
        rig.channel_tables(0x1F_FFFF_FFFF);
        rig.engine
            .attach_central(Central::new(CentralConfig::default(), steps));
        rig.engine.write(0x28000, 1, 4).unwrap();
        rig.column(1, 0xA0);
        rig.column(15, 0x8000);
        rig.column(8, 0);
        rig.column(6, 0x41A5);
        rig.column(2, 0x2000);
        rig.engine.write(0x28028, 0x101, 4).unwrap();
        rig.column(0, 0);
        rig.column(14, 0x8000);
        rig
    }

    /// CHMAP0/1/2 and FRQ_IDX1 for a channel map (§6 layout).
    fn channel_tables(&mut self, map: u64) {
        let used = crate::ble_air::used_channels(map);
        self.set(field::CHMAP0, map as u16);
        self.set(field::CHMAP1, (map >> 16) as u16);
        self.set(
            field::CHMAP2,
            ((map >> 32) & 31) as u16 | (used.len() as u16) << 5,
        );
        for (i, c) in used.iter().enumerate() {
            self.set8(field::FRQ_IDX1 + i as u32, *c);
        }
    }

    /// What the vendor's RX ISR does with a CONNECT_IND (§7), as a driver.
    fn accept_connection(&mut self) {
        let b = (self.get(field::RXTOG) ^ 1) as u32 & 1;
        let pointer = self.get(field::RXPTR0 + 2 * b) as u32;
        let ll = self.w().bytes(pointer + 12, 22).unwrap();
        let p = ConnectParams::from_lldata(&ll).unwrap();
        self.set(field::BDADDR0, p.access_address as u16);
        self.set(field::BDADDR1, (p.access_address >> 16) as u16);
        self.set(field::CRCWORD0, p.crc_init as u16);
        self.set(field::CRCWORD1, (p.crc_init >> 16) as u16);
        self.set(field::RXTOG, 0);
        self.set8(field::TXBUF0CNTL, 1);
        self.set8(field::TXBUF0CNTL + 1, 1);
        self.column(5, 0);
        self.column(4, 0);
        self.column(2, 7 << 12);
        self.column(8, 0);
        self.column(3, 0);
        self.set(field::WINCNTL0, p.win_size as u16 * 1250 + 1250);
        self.column(1, 2 * p.interval as u32);
        self.column(15, 0x8000);
        self.channel_tables(p.channel_map);
        self.column(6, 0x8000 | (p.hop as u32) << 8 | p.hop as u32);
        self.set(field::TXDHDR0, 1);
        self.set(field::TXDHDR0 + 2, 1 ^ 5);
    }
}

fn adv_records(rig: &Rig) -> Vec<&AirRecord> {
    rig.engine
        .air_log()
        .iter()
        .filter(|r| r.access_address == ADV_ACCESS_ADDRESS)
        .collect()
}

#[test]
fn advertising_events_hop_37_38_39_and_answer_scan_requests() {
    let mut rig = Rig::advertising(vec![Step::Scan(u32::MAX)], EngineConfig::default());
    rig.run(150 * 24_000);
    let log = adv_records(&rig);
    let adv: Vec<_> = log
        .iter()
        .filter(|r| r.pdu[0] & 15 == adv::ADV_IND)
        .collect();
    assert_eq!(
        adv.iter().map(|r| r.channel).collect::<Vec<_>>(),
        [37, 38, 39, 37, 38, 39]
    );
    // 1,250 µs between the PDUs of an event (ADVIDX), 100 ms between events.
    assert_eq!(adv[1].tick - adv[0].tick, 1_250 * 24);
    assert_eq!(adv[3].tick - adv[0].tick, 0xA0 * SLOT);
    assert_eq!(adv[0].pdu[..2], [0, 9]);
    assert_eq!(adv[0].pdu[2..8], ADV_ADDRESS);
    let rsp: Vec<_> = log
        .iter()
        .filter(|r| r.pdu[0] & 15 == adv::SCAN_RSP)
        .collect();
    assert_eq!(rsp.len(), 6);
    assert_eq!(rsp[0].pdu[8..], [5, 9, b'F', b'M', b'-', b'1']);
    let req = log.iter().find(|r| r.pdu[0] & 15 == adv::SCAN_REQ).unwrap();
    // SCAN_RSP T_IFS after the SCAN_REQ ends.
    assert_eq!(rsp[0].tick - req.tick, airtime_ticks(12) + T_IFS);
    assert_eq!(rig.engine.stats.event_irqs, 2);
    assert!(rig.engine.event_irq() && !rig.engine.rx_irq());
    rig.engine.write(0x2802C, 1, 4).unwrap();
    assert!(!rig.engine.event_irq());
    assert_eq!(rig.central().scan_responses().len(), 6);
}

#[test]
fn software_scan_response_mode_stores_the_request_and_sends_nothing() {
    let mut config = EngineConfig::default();
    config.apply("scan_rsp=software").unwrap();
    let mut rig = Rig::advertising(vec![Step::Scan(u32::MAX)], config);
    rig.run(10 * 24_000);
    assert!(adv_records(&rig)
        .iter()
        .all(|r| r.pdu[0] & 15 != adv::SCAN_RSP));
    assert!(rig.engine.rx_irq());
    assert_eq!(
        rig.get(field::RXTOG) & 1,
        1,
        "two requests filled buffers 0 and 1, then 0"
    );
    let req = rig.w().bytes(0x400, 12).unwrap();
    assert_eq!(req[6..], ADV_ADDRESS);
    assert_eq!(rig.get(field::RXDHDR0) >> 8, 12);
    assert_eq!(rig.get8(field::RXBUF0CNTL) & 1, 1);
}

fn connected_rig(config: EngineConfig, steps: Vec<Step>) -> Rig {
    let mut script = vec![Step::Scan(1), Step::Connect(Default::default())];
    script.extend(steps);
    let mut rig = Rig::advertising(script, config);
    while rig.engine.links[0].connect_end.is_none() {
        rig.run(1_000);
    }
    let end = rig.engine.links[0].connect_end.unwrap();
    rig.run(end - rig.engine.now);
    assert!(rig.engine.rx_irq(), "CONNECT_IND raises the RX interrupt");
    rig.accept_connection();
    rig
}

#[test]
fn connect_ind_stops_advertising_and_the_first_anchor_follows_the_window() {
    let mut rig = connected_rig(EngineConfig::default(), vec![Step::Wait(3)]);
    let end = rig.engine.links[0].connect_end.unwrap();
    rig.run(200 * 24_000);
    let log: Vec<_> = rig
        .engine
        .air_log()
        .iter()
        .filter(|r| r.access_address != ADV_ACCESS_ADDRESS)
        .collect();
    // transmitWindowStart = CONNECT_IND end + 1.25 ms (WinOffset 0).
    assert_eq!(log[0].tick, end + 2 * SLOT);
    assert_eq!(log[0].sender, Sender::Central);
    assert_eq!(log[1].tick, log[0].tick + airtime_ticks(0) + T_IFS);
    assert!(rig.central().connected());
    assert!(
        adv_records(&rig).iter().all(|r| r.tick < end),
        "no ADV after CONNECT_IND"
    );
    // CSA #1 from hop 7 with all channels: 7, 14, 21, ...
    let channels: Vec<_> = log.iter().step_by(2).take(4).map(|r| r.channel).collect();
    assert_eq!(channels, [7, 14, 21, 28]);
    // Column 3 reads back counter + 1 (§2.3).
    rig.engine.write(0x2801C, 3 << 10 | 2, 4).unwrap();
    // Between events it holds the last event's counter + 1 = events run.
    let events = rig.engine.stats.conn_events as u32;
    assert_eq!(rig.engine.read(0x28024, 4).unwrap(), events);
}

#[test]
fn empty_pdus_alternate_buffers_and_sn_and_loaded_buffers_are_consumed() {
    let mut rig = connected_rig(EngineConfig::default(), vec![Step::Wait(20)]);
    rig.run(40 * 24_000); // events 0 and 1 (30 ms interval)
    let replies: Vec<_> = rig
        .engine
        .air_log()
        .iter()
        .filter(|r| r.sender == Sender::Peripheral && r.event.is_some())
        .map(|r| r.pdu.clone())
        .collect();
    // Event 0: empty, SN 0 (buffer 0). Event 1: acked, so buffer 1, SN 1.
    assert_eq!(replies[0][..2], [0x01 | 4, 0]);
    assert_eq!(replies[1][0] & 0x0B, 0x09);
    assert_eq!(rig.get(field::TXTOG) & 1, 1);
    // RX side: two central PDUs stored, RXTOG toggled twice, header kept.
    assert_eq!(rig.get(field::RXTOG) & 1, 0);
    assert_eq!(rig.get(field::RXDHDR0) & 3, 1);
    assert_eq!(
        rig.get(field::RXSTAT0 + 2) >> 8,
        1,
        "event 1 acknowledged our SN 0"
    );
    // Load buffer 1 with an L2CAP start fragment; the next reply carries it.
    rig.w().set_bytes(0x900, &[1, 0, 4, 0, 0x1B]).unwrap();
    let sn = rig.get(field::TXDHDR0 + 2) & 4;
    rig.set(field::TXDHDR0 + 2, 5 << 8 | 2 | sn);
    rig.set8(field::TXBUF0CNTL + 1, 0);
    rig.set(field::TXTOG, 1);
    // Buffer 1's empty PDU (SN 1) is still unacknowledged, so the next
    // event sends buffer 0 (SN 0); the data goes out in the event after
    // that, and is acknowledged one event later.
    let before = rig.engine.stats.tx_data;
    rig.run(100 * 24_000);
    assert_eq!(rig.engine.stats.tx_data, before + 1);
    assert_eq!(rig.get8(field::TXBUF0CNTL + 1) & 1, 1, "acked: consumed");
    let sent = rig
        .engine
        .air_log()
        .iter()
        .find(|r| r.sender == Sender::Peripheral && r.pdu.len() == 7)
        .unwrap();
    assert_eq!(sent.pdu[0] & 3, 2);
    assert_eq!(sent.pdu[2..], [1, 0, 4, 0, 0x1B]);
}

#[test]
fn unacknowledged_pdus_are_resent_and_old_sn_is_dropped() {
    let mut rig = connected_rig(EngineConfig::default(), vec![Step::Wait(20)]);
    rig.run(10 * 24_000); // event 0 done
    let mut conn = rig.engine.links[0].conn.clone().unwrap();
    let block = Block(BLOCK);
    let channel = conn.channel;
    let tx = |pdu: Vec<u8>| CentralTx {
        channel,
        access_address: ConnectParams::default().access_address,
        crc_init: ConnectParams::default().crc_init,
        event: 1,
        pdu,
    };
    // A central PDU with NESN = our last SN (0): a NAK, and SN = 0 again: old.
    conn.awaiting = true;
    rig.engine.links[0].conn = Some(conn.clone());
    let stored = rig.engine.stats.rx_stored;
    let mut w = Window {
        ram: &mut rig.ram,
        base: BASE,
        end: BASE + 0x1000,
    };
    let note = rig
        .engine
        .conn_receive(0, block, &mut w, &tx(vec![0x01, 0]), 0)
        .unwrap();
    assert_eq!(note, "old SN: retransmission ignored");
    assert_eq!(rig.engine.stats.rx_stored, stored);
    rig.engine.conn_tx(0, block, &mut w).unwrap();
    assert_eq!(rig.engine.stats.tx_retransmit, 1);
    // drop_old_sn=0: the same packet is stored for the driver to drop.
    rig.engine.config.apply("drop_old_sn=0").unwrap();
    conn = rig.engine.links[0].conn.clone().unwrap();
    conn.awaiting = true;
    rig.engine.links[0].conn = Some(conn);
    let note = rig
        .engine
        .conn_receive(0, block, &mut w, &tx(vec![0x01, 0]), 0)
        .unwrap();
    assert!(note.starts_with("old SN: stored anyway"));
}

#[test]
fn a_connection_update_switches_interval_only_at_its_instant() {
    let mut rig = connected_rig(EngineConfig::default(), vec![Step::Wait(40)]);
    rig.run(100 * 24_000);
    let counter = rig.engine.links[0].conn.as_ref().unwrap().counter;
    let instant = counter + 3;
    // Software writes the instant and, one event before it, the interval.
    rig.column(5, instant as u32);
    rig.column(1, 12);
    rig.run(200 * 24_000);
    let stored = |e: u16| {
        rig.engine.air_log().iter().any(|r| {
            r.sender == Sender::Central && r.event == Some(e) && r.note.starts_with("stored")
        })
    };
    // Gated: the old interval holds up to and including the instant's
    // anchor (old interval + offset 0), so the central (which sent no
    // update and keeps 30 ms) is heard there, and not on the 7.5 ms grid
    // the engine uses afterwards.
    assert!((counter..=instant).all(stored));
    assert!(!stored(instant + 1));
    assert_eq!(
        rig.engine.links[0].conn.as_ref().unwrap().interval_slots,
        12
    );
    assert!(rig
        .engine
        .notes()
        .iter()
        .any(|n| n.contains(&format!("instant {instant}"))));
}

#[test]
fn settings_parse_and_reject_unknown_keys() {
    let mut c = EngineConfig::default();
    c.apply("pairs=4, engine_retransmits=0,event_irq_every_event=off,rx_needs_armed=1")
        .unwrap();
    assert_eq!(c.max_pairs_per_event, 4);
    assert!(!c.engine_retransmits && !c.event_irq_every_event && c.rx_needs_armed);
    assert!(c.apply("bogus=1").is_err());
    assert!(c.apply("pairs=0").is_err());
}

#[test]
fn dma_outside_the_window_faults() {
    let mut rig = Rig::advertising(vec![], EngineConfig::default());
    rig.engine.set_ram_window(None, Some(BASE + 0x100));
    let ram = &mut rig.ram;
    let error = (0..400).find_map(|_| rig.engine.advance(24_000, ram).err());
    assert!(error.unwrap().contains("outside the baseband RAM window"));
}
