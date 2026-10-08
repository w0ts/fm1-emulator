// SPDX-License-Identifier: GPL-3.0-only
//! The central's ATT client (Core Vol 3 Parts F and G): MTU exchange,
//! discovery in CoreBluetooth's order (all primary services, then each
//! service's characteristics, then each characteristic's descriptors),
//! reads, writes, notifications; plus the minimal ATT server answers a
//! central owes a peripheral that asks.

use std::collections::VecDeque;
use std::fmt;

/// BLE-MIDI service 03B80E5A-EDE8-4B33-A751-6CE34EC4C700, little-endian.
pub const MIDI_SERVICE: [u8; 16] = [
    0x00, 0xC7, 0xC4, 0x4E, 0xE3, 0x6C, 0x51, 0xA7, 0x33, 0x4B, 0xE8, 0xED, 0x5A, 0x0E, 0xB8, 0x03,
];
/// BLE-MIDI I/O characteristic 7772E5DB-3868-4112-A1A9-F2669D106BF3.
pub const MIDI_CHARACTERISTIC: [u8; 16] = [
    0xF3, 0x6B, 0x10, 0x9D, 0x66, 0xF2, 0xA9, 0xA1, 0x12, 0x41, 0x68, 0x38, 0xDB, 0xE5, 0x72, 0x77,
];

/// A 16- or 128-bit UUID as sent on the air (little-endian).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uuid(pub Vec<u8>);

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b: Vec<u8> = self.0.iter().rev().copied().collect();
        if b.len() != 16 {
            let hex: Vec<_> = b.iter().map(|x| format!("{x:02X}")).collect();
            return write!(f, "0x{}", hex.join(""));
        }
        let h =
            |r: std::ops::Range<usize>| b[r].iter().map(|x| format!("{x:02X}")).collect::<String>();
        write!(
            f,
            "{}-{}-{}-{}-{}",
            h(0..4),
            h(4..6),
            h(6..8),
            h(8..10),
            h(10..16)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub start: u16,
    pub end: u16,
    pub uuid: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Characteristic {
    pub declaration: u16,
    pub properties: u8,
    pub value: u16,
    pub uuid: Uuid,
    /// Last handle belonging to it (before the next declaration).
    pub end: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub handle: u16,
    pub uuid: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub tick: u64,
    pub handle: u16,
    pub value: Vec<u8>,
    pub indication: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Discovery {
    Idle,
    Services(u16),
    Characteristics(usize, u16),
    Descriptors(usize, u16),
}

pub(super) struct Gatt {
    client_mtu: u16,
    pub mtu: u16,
    pub services: Vec<Service>,
    pub characteristics: Vec<Characteristic>,
    pub descriptors: Vec<Descriptor>,
    pub notifications: Vec<Notification>,
    pub outbox: VecDeque<Vec<u8>>,
    /// Opcode of the outstanding request.
    waiting: Option<u8>,
    discovery: Discovery,
    done: Option<Result<String, String>>,
    log: Vec<String>,
}

fn u16_at(p: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([p[i], p[i + 1]])
}

impl Gatt {
    pub fn new(client_mtu: u16) -> Self {
        Self {
            client_mtu,
            mtu: 23,
            services: Vec::new(),
            characteristics: Vec::new(),
            descriptors: Vec::new(),
            notifications: Vec::new(),
            outbox: VecDeque::new(),
            waiting: None,
            discovery: Discovery::Idle,
            done: None,
            log: Vec::new(),
        }
    }

    /// The result of the last request-style step, once.
    pub fn take_done(&mut self) -> Option<Result<String, String>> {
        self.done.take()
    }
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    fn request(&mut self, pdu: Vec<u8>) {
        self.waiting = Some(pdu[0]);
        self.outbox.push_back(pdu);
    }

    pub fn exchange_mtu(&mut self, mtu: u16) {
        self.client_mtu = mtu;
        let mut pdu = vec![0x02];
        pdu.extend(mtu.to_le_bytes());
        self.request(pdu);
    }

    pub fn discover(&mut self) {
        self.services.clear();
        self.characteristics.clear();
        self.descriptors.clear();
        self.discovery = Discovery::Services(1);
        self.next_discovery();
    }

    pub fn read(&mut self, handle: u16) {
        let mut pdu = vec![0x0A];
        pdu.extend(handle.to_le_bytes());
        self.request(pdu);
    }

    pub fn write_request(&mut self, handle: u16, value: &[u8]) {
        let mut pdu = vec![0x12];
        pdu.extend(handle.to_le_bytes());
        pdu.extend(value);
        self.request(pdu);
    }

    pub fn write_command(&mut self, handle: u16, value: &[u8]) {
        let mut pdu = vec![0x52];
        pdu.extend(handle.to_le_bytes());
        pdu.extend(value);
        self.outbox.push_back(pdu);
    }

    fn midi(&self) -> Option<&Characteristic> {
        self.characteristics
            .iter()
            .find(|c| c.uuid.0 == MIDI_CHARACTERISTIC)
    }

    pub fn midi_handle(&self) -> Option<u16> {
        self.midi().map(|c| c.value)
    }

    /// The CCCD of the MIDI characteristic, or of the first characteristic
    /// that can notify; value handle + 1 when no descriptor was found.
    pub fn subscribe_handle(&self) -> Option<u16> {
        let c = self.midi().or_else(|| {
            self.characteristics
                .iter()
                .find(|c| c.properties & 0x10 != 0)
        })?;
        let cccd = self
            .descriptors
            .iter()
            .find(|d| d.handle > c.value && d.handle <= c.end && d.uuid.0 == [0x02, 0x29]);
        Some(cccd.map_or(c.value + 1, |d| d.handle))
    }

    fn next_discovery(&mut self) {
        loop {
            match self.discovery {
                Discovery::Idle => return,
                Discovery::Services(start) => {
                    let mut pdu = vec![0x10];
                    pdu.extend(start.to_le_bytes());
                    pdu.extend([0xFF, 0xFF, 0x00, 0x28]);
                    return self.request(pdu);
                }
                Discovery::Characteristics(i, start) => {
                    let Some(s) = self.services.get(i).cloned() else {
                        self.finish_characteristics();
                        continue;
                    };
                    if start > s.end || start == 0 {
                        self.discovery = Discovery::Characteristics(
                            i + 1,
                            self.services.get(i + 1).map_or(0, |s| s.start),
                        );
                        continue;
                    }
                    let mut pdu = vec![0x08];
                    pdu.extend(start.to_le_bytes());
                    pdu.extend(s.end.to_le_bytes());
                    pdu.extend([0x03, 0x28]);
                    return self.request(pdu);
                }
                Discovery::Descriptors(i, start) => {
                    let Some(c) = self.characteristics.get(i).cloned() else {
                        self.discovery = Discovery::Idle;
                        self.done = Some(Ok(format!(
                            "{} services, {} characteristics, {} descriptors",
                            self.services.len(),
                            self.characteristics.len(),
                            self.descriptors.len()
                        )));
                        return;
                    };
                    let start = start.max(c.value + 1);
                    if start > c.end || c.value == 0xFFFF {
                        self.discovery = Discovery::Descriptors(i + 1, 0);
                        continue;
                    }
                    let mut pdu = vec![0x04];
                    pdu.extend(start.to_le_bytes());
                    pdu.extend(c.end.to_le_bytes());
                    return self.request(pdu);
                }
            }
        }
    }

    /// All characteristics known: each ends before the next declaration
    /// or at its service's end. Then descriptors.
    fn finish_characteristics(&mut self) {
        let n = self.characteristics.len();
        for i in 0..n {
            let declaration = self.characteristics[i].declaration;
            let service_end = self
                .services
                .iter()
                .find(|s| (s.start..=s.end).contains(&declaration))
                .map_or(0xFFFF, |s| s.end);
            let next = self
                .characteristics
                .get(i + 1)
                .map(|c| c.declaration - 1)
                .filter(|&h| h < service_end)
                .unwrap_or(service_end);
            self.characteristics[i].end = next;
        }
        for c in &self.characteristics {
            self.log.push(format!(
                "characteristic {} at 0x{:04x}, value 0x{:04x}, properties 0x{:02x}",
                c.uuid, c.declaration, c.value, c.properties
            ));
        }
        self.discovery = Discovery::Descriptors(0, 0);
    }

    fn discovery_ended(&mut self) {
        match self.discovery {
            Discovery::Services(_) => {
                for s in &self.services {
                    self.log.push(format!(
                        "service {} 0x{:04x}-0x{:04x}",
                        s.uuid, s.start, s.end
                    ));
                }
                let first = self.services.first().map_or(0, |s| s.start);
                self.discovery = Discovery::Characteristics(0, first);
            }
            Discovery::Characteristics(i, _) => {
                let next = self.services.get(i + 1).map_or(0, |s| s.start);
                self.discovery = Discovery::Characteristics(i + 1, next);
            }
            Discovery::Descriptors(i, _) => self.discovery = Discovery::Descriptors(i + 1, 0),
            Discovery::Idle => {}
        }
        self.next_discovery();
    }

    /// An ATT PDU from the peripheral.
    pub fn on_pdu(&mut self, tick: u64, p: &[u8]) {
        let Some(&op) = p.first() else { return };
        let answered = |s: &mut Self, req: u8| {
            if s.waiting == Some(req) {
                s.waiting = None;
                true
            } else {
                s.log.push(format!("unexpected ATT response 0x{op:02x}"));
                false
            }
        };
        match op {
            0x01 if p.len() >= 5 => {
                let (req, handle, code) = (p[1], u16_at(p, 2), p[4]);
                if !answered(self, req) {
                    return;
                }
                if matches!(req, 0x10 | 0x08 | 0x04) && self.discovery != Discovery::Idle {
                    if code != 0x0A {
                        self.log
                            .push(format!("discovery error 0x{code:02x} at 0x{handle:04x}"));
                    }
                    self.discovery_ended();
                } else {
                    self.done = Some(Err(format!(
                        "ATT error 0x{code:02x} for request 0x{req:02x} at 0x{handle:04x}"
                    )));
                }
            }
            0x03 if p.len() >= 3 => {
                if answered(self, 0x02) {
                    let server = u16_at(p, 1);
                    self.mtu = server.min(self.client_mtu).max(23);
                    self.done = Some(Ok(format!("server MTU {server}, ATT MTU {}", self.mtu)));
                }
            }
            0x02 if p.len() >= 3 => {
                let client = u16_at(p, 1);
                self.mtu = client.min(self.client_mtu).max(23);
                let mut rsp = vec![0x03];
                rsp.extend(self.client_mtu.to_le_bytes());
                self.outbox.push_back(rsp);
                self.log.push(format!("peripheral MTU request {client}"));
            }
            0x11 if p.len() >= 2 && answered(self, 0x10) => self.on_services(p),
            0x09 if p.len() >= 2 && answered(self, 0x08) => self.on_characteristics(p),
            0x05 if p.len() >= 2 && answered(self, 0x04) => self.on_descriptors(p),
            0x0B => {
                if answered(self, 0x0A) {
                    self.done = Some(Ok(format!("read {:02x?}", &p[1..])));
                }
            }
            0x13 => {
                if answered(self, 0x12) {
                    self.done = Some(Ok("write response".into()));
                }
            }
            0x1B | 0x1D if p.len() >= 3 => {
                self.notifications.push(Notification {
                    tick,
                    handle: u16_at(p, 1),
                    value: p[3..].to_vec(),
                    indication: op == 0x1D,
                });
                if op == 0x1D {
                    self.outbox.push_back(vec![0x1E]);
                }
            }
            // Requests to our (empty) server.
            _ if op & 0x40 == 0 && op & 1 == 0 => {
                let handle = if p.len() >= 3 { u16_at(p, 1) } else { 0 };
                let code = if matches!(op, 0x04 | 0x06 | 0x08 | 0x10) {
                    0x0A
                } else {
                    0x06
                };
                let mut rsp = vec![0x01, op];
                rsp.extend(handle.to_le_bytes());
                rsp.push(code);
                self.outbox.push_back(rsp);
            }
            _ => self.log.push(format!("ATT 0x{op:02x} ignored")),
        }
    }

    fn on_services(&mut self, p: &[u8]) {
        let len = p[1] as usize;
        if len < 6 {
            return self.discovery_ended();
        }
        let mut last = 0xFFFF;
        for e in p[2..].chunks_exact(len) {
            last = u16_at(e, 2);
            self.services.push(Service {
                start: u16_at(e, 0),
                end: last,
                uuid: Uuid(e[4..].to_vec()),
            });
        }
        if last == 0xFFFF {
            self.discovery_ended();
        } else {
            self.discovery = Discovery::Services(last + 1);
            self.next_discovery();
        }
    }

    fn on_characteristics(&mut self, p: &[u8]) {
        let len = p[1] as usize;
        let Discovery::Characteristics(i, _) = self.discovery else {
            return;
        };
        if len < 7 {
            return self.discovery_ended();
        }
        let mut last = 0;
        for e in p[2..].chunks_exact(len) {
            last = u16_at(e, 0);
            self.characteristics.push(Characteristic {
                declaration: last,
                properties: e[2],
                value: u16_at(e, 3),
                uuid: Uuid(e[5..].to_vec()),
                end: 0xFFFF,
            });
        }
        self.discovery = Discovery::Characteristics(i, last.saturating_add(1));
        self.next_discovery();
    }

    fn on_descriptors(&mut self, p: &[u8]) {
        let width = if p[1] == 1 { 2 } else { 16 };
        let Discovery::Descriptors(i, _) = self.discovery else {
            return;
        };
        let mut last = 0;
        for e in p[2..].chunks_exact(2 + width) {
            last = u16_at(e, 0);
            self.descriptors.push(Descriptor {
                handle: last,
                uuid: Uuid(e[2..].to_vec()),
            });
        }
        self.discovery = Discovery::Descriptors(i, last.saturating_add(1));
        self.next_discovery();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(g: &mut Gatt) -> Vec<u8> {
        g.outbox.pop_front().expect("a PDU was queued")
    }

    #[test]
    fn discovery_walks_services_characteristics_and_descriptors() {
        let mut g = Gatt::new(527);
        g.discover();
        assert_eq!(sent(&mut g), [0x10, 1, 0, 0xFF, 0xFF, 0, 0x28]);
        // One 16-bit service 1-5 and the 128-bit MIDI service 0x10-0x13.
        g.on_pdu(0, &[0x11, 6, 1, 0, 5, 0, 0x00, 0x18]);
        assert_eq!(sent(&mut g), [0x10, 6, 0, 0xFF, 0xFF, 0, 0x28]);
        let mut rsp = vec![0x11, 20, 0x10, 0, 0x13, 0];
        rsp.extend(MIDI_SERVICE);
        g.on_pdu(0, &rsp);
        assert_eq!(sent(&mut g), [0x10, 0x14, 0, 0xFF, 0xFF, 0, 0x28]);
        g.on_pdu(0, &[0x01, 0x10, 0x14, 0, 0x0A]);
        assert_eq!(sent(&mut g), [0x08, 1, 0, 5, 0, 3, 0x28]);
        g.on_pdu(0, &[0x01, 0x08, 1, 0, 0x0A]);
        assert_eq!(sent(&mut g), [0x08, 0x10, 0, 0x13, 0, 3, 0x28]);
        let mut rsp = vec![0x09, 21, 0x11, 0, 0x16, 0x12, 0];
        rsp.extend(MIDI_CHARACTERISTIC);
        g.on_pdu(0, &rsp);
        assert_eq!(sent(&mut g), [0x08, 0x12, 0, 0x13, 0, 3, 0x28]);
        g.on_pdu(0, &[0x01, 0x08, 0x12, 0, 0x0A]);
        assert_eq!(sent(&mut g), [0x04, 0x13, 0, 0x13, 0]);
        g.on_pdu(0, &[0x05, 1, 0x13, 0, 0x02, 0x29]);
        assert_eq!(
            g.take_done(),
            Some(Ok("2 services, 1 characteristics, 1 descriptors".into()))
        );
        assert_eq!(g.midi_handle(), Some(0x12));
        assert_eq!(g.subscribe_handle(), Some(0x13));
        assert_eq!(
            g.services[1].uuid.to_string(),
            "03B80E5A-EDE8-4B33-A751-6CE34EC4C700"
        );
    }

    #[test]
    fn server_requests_get_errors_and_mtu_requests_an_answer() {
        let mut g = Gatt::new(247);
        g.on_pdu(0, &[0x02, 0x00, 0x02]);
        assert_eq!(sent(&mut g), [0x03, 247, 0]);
        assert_eq!(g.mtu, 247);
        g.on_pdu(0, &[0x10, 1, 0, 0xFF, 0xFF, 0, 0x28]);
        assert_eq!(sent(&mut g), [0x01, 0x10, 1, 0, 0x0A]);
        g.on_pdu(0, &[0x52, 1, 0, 9]);
        assert!(g.outbox.is_empty());
        g.on_pdu(5, &[0x1D, 0x22, 0, 7]);
        assert_eq!(sent(&mut g), [0x1E]);
        assert_eq!(g.notifications[0].value, [7]);
    }
}
