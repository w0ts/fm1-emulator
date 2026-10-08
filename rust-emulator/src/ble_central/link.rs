// SPDX-License-Identifier: GPL-3.0-only
//! The central's (master's) side of a connection: its own anchor clock,
//! CSA #1, SN/NESN, LL control procedures and L2CAP framing (Core Vol 6
//! Part B 4.5, 5; Vol 3 Part A).

use super::*;
use crate::ble_air::{csa1, ll, used_channels};
use std::collections::VecDeque;

/// Payload bytes per LL data PDU without the data length extension.
const LL_PAYLOAD: usize = 27;

#[derive(Clone, Debug)]
pub(super) enum Update {
    Connection(ConnectParams),
    Map(u64),
}

pub(super) struct Link {
    pub params: ConnectParams,
    used: Vec<u8>,
    /// When the central transmits next (the anchor of event `event`).
    anchor: u64,
    /// The anchor of the event in progress (`anchor` is parked meanwhile).
    event_anchor: u64,
    pub event: u16,
    last_unmapped: u8,
    /// The event continues (MD): same channel, no new anchor.
    continuing: bool,
    pairs: u32,
    channel: u8,
    sn: u8,
    nesn: u8,
    queue: VecDeque<Vec<u8>>,
    in_flight: Option<Vec<u8>>,
    pub update: Option<(u16, Update)>,
    pub established: bool,
    last_rx: u64,
    missed: u32,
    reassembly: Vec<u8>,
    pub peer_version: Option<(u8, u16, u16)>,
    pub peer_features: Option<Result<u64, String>>,
    version_sent: bool,
    /// Close after this event: the peer's LL_TERMINATE_IND is acknowledged
    /// in the event after it arrived.
    terminating: Option<(u16, String)>,
}

impl Link {
    pub fn new(params: ConnectParams, first_anchor: u64) -> Self {
        Self {
            params,
            used: used_channels(params.channel_map),
            anchor: first_anchor,
            event_anchor: first_anchor,
            event: 0,
            last_unmapped: 0,
            continuing: false,
            pairs: 0,
            channel: 0,
            sn: 0,
            nesn: 0,
            queue: VecDeque::new(),
            in_flight: None,
            update: None,
            established: false,
            last_rx: first_anchor,
            missed: 0,
            reassembly: Vec::new(),
            peer_version: None,
            peer_features: None,
            version_sent: false,
            terminating: None,
        }
    }
    /// Nothing queued or waiting for an acknowledgement.
    pub fn idle(&self) -> bool {
        self.queue.is_empty() && self.in_flight.as_ref().is_none_or(|p| p[1] == 0)
    }
    fn interval(&self) -> u64 {
        self.params.interval as u64 * UNIT_1250_TICKS
    }
}

impl Central {
    pub fn next_tx_tick(&self) -> Option<u64> {
        self.link.as_ref().map(|l| l.anchor)
    }

    /// The master's packet of this connection event.
    pub fn transmit(&mut self, now: u64) -> Option<CentralTx> {
        self.now = now;
        let max_pairs = self.config.max_pairs_per_event;
        let link = self.link.as_mut()?;
        if !link.continuing {
            let (unmapped, channel) = csa1(link.last_unmapped, link.params.hop, &link.used);
            link.last_unmapped = unmapped;
            link.channel = channel;
            link.event_anchor = link.anchor;
            link.pairs = 0;
        }
        let channel = link.channel;
        let more_allowed = link.pairs + 1 < max_pairs;
        let pdu = match &mut link.in_flight {
            // A retransmission keeps SN and payload; NESN is current.
            Some(pdu) => {
                pdu[0] = (pdu[0] & !4) | link.nesn << 2;
                pdu.clone()
            }
            None => {
                let mut pdu = link.queue.pop_front().unwrap_or_else(|| vec![1, 0]);
                let md = more_allowed && !link.queue.is_empty();
                pdu[0] = (pdu[0] & 3) | link.nesn << 2 | link.sn << 3 | (md as u8) << 4;
                link.in_flight = Some(pdu.clone());
                pdu
            }
        };
        // Park the anchor until the event ends.
        link.anchor = u64::MAX;
        Some(CentralTx {
            channel,
            access_address: link.params.access_address,
            crc_init: link.params.crc_init,
            event: link.event,
            pdu,
        })
    }

    /// The peripheral answered within T_IFS.
    pub fn on_peripheral_pdu(&mut self, now: u64, pdu: &[u8]) {
        self.now = now;
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let first = !link.established;
        link.established = true;
        link.last_rx = now;
        link.missed = 0;
        let (nesn, sn) = ((pdu[0] >> 2) & 1, (pdu[0] >> 3) & 1);
        let mut acked_terminate = false;
        if nesn != link.sn {
            if let Some(sent) = link.in_flight.take() {
                acked_terminate = sent[0] & 3 == 3 && sent.get(2) == Some(&ll::TERMINATE_IND);
            }
            link.sn ^= 1;
        }
        let fresh = sn == link.nesn;
        if fresh {
            link.nesn ^= 1;
        }
        if first {
            self.say(format!(
                "connected: first peripheral PDU at event {}",
                self.link.as_ref().unwrap().event
            ));
        }
        if fresh && pdu.len() > 2 {
            match pdu[0] & 3 {
                3 => self.on_ll_control(&pdu[2..]),
                2 | 1 => self.on_l2cap_fragment(pdu[0] & 3 == 2, &pdu[2..]),
                _ => self.say("peripheral sent a PDU with LLID 0".into()),
            }
        }
        if acked_terminate {
            self.close("LL_TERMINATE_IND sent and acknowledged".into());
            return;
        }
        // MD from either side continues the event (Core Vol 6 Part B 4.5.6).
        let max_pairs = self.config.max_pairs_per_event;
        let link = self.link.as_mut().unwrap();
        link.pairs += 1;
        let ours = link
            .in_flight
            .as_ref()
            .map_or(!link.queue.is_empty(), |p| p[0] & 0x10 != 0);
        if (pdu[0] & 0x10 != 0 || ours) && link.pairs < max_pairs {
            link.continuing = true;
            link.anchor =
                now + airtime_ticks(pdu.len() - 2) + crate::ble_air::T_IFS_US * TICKS_PER_US;
        } else {
            self.end_event();
        }
    }

    /// Nobody answered: count it, apply supervision, move on.
    pub fn on_no_response(&mut self, now: u64) {
        self.now = now;
        let Some(link) = self.link.as_mut() else {
            return;
        };
        link.missed += 1;
        let timeout = link.params.timeout as u64 * 10_000 * TICKS_PER_US;
        let (established, missed, silent) = (
            link.established,
            link.missed,
            now.saturating_sub(link.last_rx),
        );
        let timeout_ms = link.params.timeout as u32 * 10;
        if !established && missed >= 6 {
            self.close("not established: no answer in the first 6 events".into());
        } else if established && silent > timeout {
            self.close(format!("supervision timeout ({timeout_ms} ms)"));
        } else {
            self.end_event();
        }
    }

    fn end_event(&mut self) {
        self.events += 1;
        let link = self.link.as_mut().expect("an event of a live link");
        link.continuing = false;
        let previous = link.event_anchor;
        link.event = link.event.wrapping_add(1);
        let mut next = previous + link.interval();
        if let Some((instant, update)) = link.update.clone() {
            if instant == link.event {
                match update {
                    // The window starts the old interval after the last
                    // anchor plus the offset; the new interval from there.
                    Update::Connection(p) => {
                        next += p.win_offset as u64 * UNIT_1250_TICKS;
                        link.params = ConnectParams {
                            channel_map: link.params.channel_map,
                            hop: link.params.hop,
                            ..p
                        };
                    }
                    Update::Map(map) => {
                        link.params.channel_map = map;
                        link.used = used_channels(map);
                    }
                }
                link.update = None;
                let event = link.event;
                self.say(format!("instant {event} applied"));
            }
        }
        let link = self.link.as_mut().unwrap();
        link.anchor = next;
        if link
            .terminating
            .as_ref()
            .is_some_and(|(e, _)| *e < link.event)
        {
            let (_, why) = link.terminating.take().unwrap();
            self.close(why);
            return;
        }
        self.run_script();
        self.flush_gatt();
    }

    pub(super) fn close(&mut self, why: String) {
        self.say(format!("connection closed: {why}"));
        self.link = None;
        self.closed = Some(why);
        self.run_script();
    }

    // ---- LL control --------------------------------------------------

    pub(super) fn send_ll(&mut self, opcode: u8, data: &[u8]) {
        if let Some(link) = self.link.as_mut() {
            let mut pdu = vec![3, 1 + data.len() as u8, opcode];
            pdu.extend(data);
            link.queue.push_back(pdu);
        }
    }

    pub(super) fn send_version(&mut self) {
        if let Some(link) = self.link.as_mut() {
            link.version_sent = true;
        }
        // Version 9 (Core 5.0), company 0xFFFF (no company: test use).
        self.send_ll(ll::VERSION_IND, &[9, 0xFF, 0xFF, 0x01, 0x00]);
    }

    /// The instant: far enough ahead for the queue and a few retries.
    fn instant(&self) -> u16 {
        let link = self.link.as_ref().unwrap();
        link.event.wrapping_add(8 + link.queue.len() as u16)
    }

    pub(super) fn start_update(&mut self, p: LinkParams) {
        if self.link.is_none() {
            return;
        }
        let current = self.link.as_ref().unwrap().params;
        let new = ConnectParams {
            interval: p.interval.unwrap_or(current.interval),
            latency: p.latency.unwrap_or(current.latency),
            timeout: p.timeout.unwrap_or(current.timeout),
            win_size: p.win_size.unwrap_or(1),
            win_offset: p.win_offset.unwrap_or(0),
            ..current
        };
        self.queue_update(new);
    }

    fn queue_update(&mut self, new: ConnectParams) {
        let instant = self.instant();
        let mut data = vec![new.win_size];
        for v in [
            new.win_offset,
            new.interval,
            new.latency,
            new.timeout,
            instant,
        ] {
            data.extend(v.to_le_bytes());
        }
        self.send_ll(ll::CONNECTION_UPDATE_IND, &data);
        self.link.as_mut().unwrap().update = Some((instant, Update::Connection(new)));
        self.say(format!(
            "LL_CONNECTION_UPDATE_IND: interval {} latency {} timeout {} at instant {instant}",
            new.interval, new.latency, new.timeout
        ));
    }

    pub(super) fn start_channel_map(&mut self, map: u64) -> Result<(), String> {
        if used_channels(map).len() < 2 {
            return Err("a channel map needs at least two channels".into());
        }
        let instant = self.instant();
        let mut data = map.to_le_bytes()[..5].to_vec();
        data.extend(instant.to_le_bytes());
        self.send_ll(ll::CHANNEL_MAP_IND, &data);
        self.link.as_mut().unwrap().update = Some((instant, Update::Map(map)));
        Ok(())
    }

    fn on_ll_control(&mut self, p: &[u8]) {
        let op = p[0];
        let name = crate::ble_air::ll_name(op);
        self.say(format!("peripheral {name} {:02x?}", &p[1..]));
        let link = self.link.as_mut().unwrap();
        match op {
            ll::FEATURE_REQ | ll::SLAVE_FEATURE_REQ => {
                let features = self.config.features.to_le_bytes();
                self.send_ll(ll::FEATURE_RSP, &features);
            }
            ll::FEATURE_RSP if p.len() >= 9 => {
                link.peer_features = Some(Ok(u64::from_le_bytes(p[1..9].try_into().unwrap())));
            }
            ll::VERSION_IND if p.len() >= 6 => {
                link.peer_version = Some((
                    p[1],
                    u16::from_le_bytes([p[2], p[3]]),
                    u16::from_le_bytes([p[4], p[5]]),
                ));
                if !link.version_sent {
                    self.send_version();
                }
            }
            ll::CONNECTION_PARAM_REQ if p.len() >= 9 => {
                // The master answers with an update (Core Vol 6 Part B 5.1.7).
                let min = u16::from_le_bytes([p[1], p[2]]);
                let latency = u16::from_le_bytes([p[5], p[6]]);
                let timeout = u16::from_le_bytes([p[7], p[8]]);
                let new = ConnectParams {
                    interval: min,
                    latency,
                    timeout,
                    win_size: 1,
                    win_offset: 0,
                    ..link.params
                };
                self.queue_update(new);
            }
            ll::LENGTH_REQ => self.send_ll(ll::LENGTH_RSP, &[27, 0, 0x48, 0x01, 27, 0, 0x48, 0x01]),
            ll::PING_REQ => self.send_ll(ll::PING_RSP, &[]),
            ll::TERMINATE_IND => {
                let reason = p.get(1).copied().unwrap_or(0);
                let event = link.event.wrapping_add(1);
                link.terminating = Some((
                    event,
                    format!("peripheral LL_TERMINATE_IND, reason 0x{reason:02x}"),
                ));
            }
            ll::UNKNOWN_RSP | ll::REJECT_IND | ll::REJECT_EXT_IND => {
                if link.peer_features.is_none() && p.get(1) == Some(&ll::FEATURE_REQ) {
                    link.peer_features = Some(Err(format!("{name} {:02x?}", &p[1..])));
                }
            }
            ll::FEATURE_RSP | ll::VERSION_IND | ll::CONNECTION_PARAM_REQ => {}
            ll::CONNECTION_UPDATE_IND | ll::CHANNEL_MAP_IND => {
                self.say("a peripheral may not send this master-only PDU".into());
            }
            _ => self.send_ll(ll::UNKNOWN_RSP, &[op]),
        }
    }

    // ---- L2CAP ---------------------------------------------------------

    /// Send an L2CAP frame on channel `cid`, fragmented into LL PDUs.
    pub(super) fn send_l2cap(&mut self, cid: u16, payload: &[u8]) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let mut frame = (payload.len() as u16).to_le_bytes().to_vec();
        frame.extend(cid.to_le_bytes());
        frame.extend(payload);
        for (i, chunk) in frame.chunks(LL_PAYLOAD).enumerate() {
            let mut pdu = vec![if i == 0 { 2 } else { 1 }, chunk.len() as u8];
            pdu.extend(chunk);
            link.queue.push_back(pdu);
        }
    }

    fn on_l2cap_fragment(&mut self, start: bool, data: &[u8]) {
        let link = self.link.as_mut().unwrap();
        if start {
            if !link.reassembly.is_empty() {
                self.say("L2CAP start fragment before the previous frame completed".into());
            }
            let link = self.link.as_mut().unwrap();
            link.reassembly = data.to_vec();
        } else if link.reassembly.is_empty() {
            self.say("L2CAP continuation without a start".into());
            return;
        } else {
            link.reassembly.extend(data);
        }
        let link = self.link.as_mut().unwrap();
        if link.reassembly.len() < 4 {
            return;
        }
        let length = u16::from_le_bytes([link.reassembly[0], link.reassembly[1]]) as usize;
        if link.reassembly.len() < 4 + length {
            return;
        }
        let frame = std::mem::take(&mut link.reassembly);
        let cid = u16::from_le_bytes([frame[2], frame[3]]);
        let payload = &frame[4..4 + length];
        match cid {
            4 => self.gatt.on_pdu(self.now, payload),
            5 => self.on_signalling(payload),
            6 => self.on_smp(payload),
            _ => self.say(format!("L2CAP frame on unknown channel {cid}")),
        }
        self.flush_gatt();
    }

    /// LE signalling (Vol 3 Part A 4): accept connection parameter update
    /// requests and carry them out with the shortest interval offered.
    fn on_signalling(&mut self, p: &[u8]) {
        if p.len() < 4 {
            return;
        }
        let (code, id) = (p[0], p[1]);
        if code == 0x12 && p.len() >= 12 {
            let field = |i: usize| u16::from_le_bytes([p[4 + 2 * i], p[5 + 2 * i]]);
            let (min, max, latency, timeout) = (field(0), field(1), field(2), field(3));
            self.say(format!(
                "L2CAP connection parameter update request: interval {min}-{max}, latency {latency}, timeout {timeout}"
            ));
            self.send_l2cap(5, &[0x13, id, 2, 0, 0, 0]);
            let current = self.link.as_ref().unwrap().params;
            let new = ConnectParams {
                interval: min,
                latency,
                timeout,
                win_size: 1,
                win_offset: 0,
                ..current
            };
            if self.link.as_ref().unwrap().update.is_none() {
                self.queue_update(new);
            }
        } else if code & 1 == 0 || code == 0x01 {
            // A request we do not handle: command reject, not understood.
            if code != 0x01 && code != 0x13 {
                self.send_l2cap(5, &[0x01, id, 2, 0, 0, 0]);
            }
        }
    }

    /// SMP (Vol 3 Part H): the virtual central does not pair.
    fn on_smp(&mut self, p: &[u8]) {
        let code = p.first().copied().unwrap_or(0);
        if !self.config.answer_smp {
            self.say(format!(
                "SMP 0x{code:02x} from the peripheral: not answered"
            ));
            return;
        }
        self.say(format!(
            "SMP 0x{code:02x} from the peripheral: pairing not supported"
        ));
        if matches!(code, 0x01 | 0x0B) {
            self.send_l2cap(6, &[0x05, 0x05]);
        }
    }

    /// Move the ATT client's queued PDUs onto the link.
    pub(super) fn flush_gatt(&mut self) {
        for line in self.gatt.take_log() {
            self.say(line);
        }
        while let Some(pdu) = self.gatt.outbox.pop_front() {
            self.send_l2cap(4, &pdu);
        }
    }
}
