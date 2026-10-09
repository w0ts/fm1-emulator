// SPDX-License-Identifier: GPL-3.0-only
//! Virtual BLE-MIDI peripherals on the virtual air: the devices the FM-1
//! connects to when it is the central (a controller, a phone app in
//! peripheral mode, a Mac with "Advertise" on). Each one advertises the
//! BLE-MIDI service, answers the engine's SCAN_REQ, takes a CONNECT_IND for
//! its address and runs the slave side of the connection with its own clock
//! (transmit window, CSA #1, SN / NESN, instants, supervision), an L2CAP /
//! ATT server with a controller-shaped database, an SMP responder (LE legacy
//! Just Works with bonding), LL encryption, BLE-MIDI notifications and a
//! record of the MIDI the FM-1 writes to it.
//!
//! Built from the Bluetooth Core specification (Vol 3, Vol 6) and the MIDI
//! Association's BLE-MIDI specification only.

mod host;
mod link;

use crate::ble_air::{adv, TICKS_PER_US};
use crate::ble_crypto;
use crate::ble_engine::MIDI_UUID;
use std::collections::VecDeque;

/// One millisecond in oscillator ticks.
const MS: u64 = 1_000 * TICKS_PER_US;
/// The spacing of the advertising PDUs on 37 / 38 / 39.
const ADV_SPACING: u64 = 500 * TICKS_PER_US;

/// How a virtual peripheral behaves (`FM1_BLE_PERIPHERALS`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeripheralConfig {
    pub name: String,
    /// Apple-like (QA1831): the MIDI value and its CCCD answer Insufficient
    /// Authentication until the link is encrypted.
    pub pair: bool,
    /// An SMP Security Request (bonding) at the start of every connection.
    pub secreq: bool,
    /// Mac-like: advertise from a resolvable private address made from the
    /// IRK, distribute the IRK and the identity address when bonding.
    pub rpa: bool,
    /// An L2CAP Connection Parameter Update Request once subscribed.
    pub upd: bool,
    /// Send notes once subscribed.
    pub notes: bool,
    /// First advertising after this much guest time (ms).
    pub appear_ms: u64,
    /// Gone (no advertising, no answers) from this guest time (ms).
    pub vanish_ms: Option<u64>,
    /// The keys and addresses are derived from it (the same in every run).
    pub seed: u64,
    /// This peripheral's "boot": a new RPA per boot (rpa mode).
    pub boot: u64,
    pub adv_interval_ms: u64,
}

impl PeripheralConfig {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            pair: false,
            secreq: false,
            rpa: false,
            upd: false,
            notes: true,
            appear_ms: 0,
            vanish_ms: None,
            seed: name
                .bytes()
                .fold(0x5EED, |s, b| s.rotate_left(7) ^ b as u64),
            boot: 0,
            adv_interval_ms: 30,
        }
    }

    /// `;`-separated `midi:NAME[:pair][:secreq][:rpa][:upd][:nonotes]
    /// [:appear=MS][:vanish=MS][:seed=N][:boot=N][:ms=N]`.
    pub fn parse(spec: &str) -> Result<Vec<Self>, String> {
        let mut out = Vec::new();
        for item in spec.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let mut parts = item.split(':');
            if parts.next() != Some("midi") {
                return Err(format!("{item}: midi:NAME[:options]"));
            }
            let name = parts.next().filter(|n| !n.is_empty());
            let mut c = Self::new(name.ok_or(format!("{item}: no name"))?);
            let num = |v: &str, what: &str| -> Result<u64, String> {
                v.parse().map_err(|_| format!("{item}: {what}: a number"))
            };
            for opt in parts {
                match opt.split_once('=') {
                    None => match opt {
                        "pair" => c.pair = true,
                        "secreq" => c.secreq = true,
                        "rpa" => c.rpa = true,
                        "upd" => c.upd = true,
                        "nonotes" => c.notes = false,
                        _ => return Err(format!("{item}: unknown option {opt}")),
                    },
                    Some(("appear", v)) => c.appear_ms = num(v, "appear")?,
                    Some(("vanish", v)) => c.vanish_ms = Some(num(v, "vanish")?),
                    Some(("seed", v)) => c.seed = num(v, "seed")?,
                    Some(("boot", v)) => c.boot = num(v, "boot")?,
                    Some(("ms", v)) => c.adv_interval_ms = num(v, "ms")?.max(20),
                    Some((k, _)) => return Err(format!("{item}: unknown option {k}")),
                }
            }
            out.push(c);
        }
        Ok(out)
    }
}

/// splitmix64: the peripheral's deterministic randomness.
fn mix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fill(state: &mut u64, out: &mut [u8]) {
    for chunk in out.chunks_mut(8) {
        let v = mix(state).to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}

/// The keys this peripheral distributes when it bonds (least significant
/// octet first), the same in every run for one seed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keys {
    pub ltk: [u8; 16],
    pub ediv: u16,
    pub rand: [u8; 8],
    pub irk: [u8; 16],
    /// Its identity address: random static (top two bits 11).
    pub identity: [u8; 6],
}

impl Keys {
    fn derive(seed: u64) -> Self {
        let mut s = seed ^ 0x4B45_5953;
        let mut k = Self {
            ltk: [0; 16],
            ediv: 0,
            rand: [0; 8],
            irk: [0; 16],
            identity: [0; 6],
        };
        fill(&mut s, &mut k.ltk);
        k.ediv = mix(&mut s) as u16 | 1;
        fill(&mut s, &mut k.rand);
        fill(&mut s, &mut k.irk);
        fill(&mut s, &mut k.identity);
        k.identity[5] |= 0xC0;
        k
    }
}

/// What a peripheral saw, for diagnose's report and the tests.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub adv_pdus: u64,
    pub scan_reqs: u64,
    pub connections: u32,
    pub established: u32,
    /// The first master packet of the first connection: inside the
    /// transmit window, its distance from the window start (µs) and its
    /// event (0 = the first window).
    pub first_in_window: Option<bool>,
    pub first_offset_us: i64,
    pub first_event: u16,
    pub events: u64,
    /// LL control opcodes heard from the master, in first-seen order.
    pub ll_seen: Vec<&'static str>,
    pub mtu: u16,
    pub auth_errors: u32,
    pub paired: bool,
    /// Encryptions started with the bond's LTK (EDIV / Rand), not an STK.
    pub ltk_reuse: u32,
    pub encrypted: bool,
    pub subscribed: bool,
    pub notifications: u64,
    pub midi_writes: u64,
    /// The first MIDI messages the FM-1 wrote, as hex.
    pub midi: Vec<String>,
    pub conn_updates: u32,
    pub chmap_updates: u32,
    /// The answer to our L2CAP Connection Parameter Update Request.
    pub param_result: Option<u16>,
    /// The initiator's identity address (from its Identity Address
    /// Information) when it distributed one.
    pub peer_identity: Option<[u8; 6]>,
    pub ended: Vec<String>,
}

/// An advertising PDU this peripheral puts on the air now.
pub struct AdvOut {
    pub channel: u8,
    pub pdu: Vec<u8>,
    pub scan_rsp: Vec<u8>,
    pub rssi: u16,
}

pub struct Peripheral {
    pub config: PeripheralConfig,
    pub keys: Keys,
    /// The address it advertises from (an RPA in rpa mode, else the identity).
    pub address: [u8; 6],
    pub report: Report,
    next_adv: u64,
    adv_index: u8,
    rng: u64,
    link: Option<link::SlaveLink>,
    host: host::Host,
    gone: bool,
}

impl Peripheral {
    pub fn new(config: PeripheralConfig) -> Self {
        let keys = Keys::derive(config.seed);
        let mut rng = config.seed ^ config.boot.rotate_left(17) ^ 0xB007;
        let address = if config.rpa {
            ble_crypto::rpa(&keys.irk, mix(&mut rng) as u32)
        } else {
            keys.identity
        };
        Self {
            next_adv: config.appear_ms * MS,
            config,
            keys,
            address,
            report: Report::default(),
            adv_index: 0,
            rng,
            link: None,
            host: host::Host::default(),
            gone: false,
        }
    }

    pub fn connected(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.established())
    }

    fn present(&self, now: u64) -> bool {
        !self.gone && now >= self.config.appear_ms * MS
    }

    fn adv_data() -> Vec<u8> {
        let mut v = vec![2, 0x01, 0x06, 17, 0x07];
        v.extend(MIDI_UUID);
        v
    }

    fn pdu(&self, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut p = vec![kind | 0x40, 6 + data.len() as u8];
        p.extend(self.address);
        p.extend_from_slice(data);
        p
    }

    /// When it next has something to do on its own (advertise, appear,
    /// vanish, a supervision timeout).
    pub fn next_tick(&self) -> Option<u64> {
        let vanish = (!self.gone)
            .then_some(self.config.vanish_ms.map(|v| v * MS))
            .flatten();
        let own = if self.gone {
            None
        } else if let Some(l) = &self.link {
            Some(l.deadline())
        } else {
            Some(self.next_adv)
        };
        [vanish, own].into_iter().flatten().min()
    }

    /// Time `now` reached: vanish, a link's end by timeout, or the next
    /// advertising PDU (returned for the engine to put on the air).
    pub fn tick(&mut self, now: u64) -> Option<AdvOut> {
        if !self.gone && self.config.vanish_ms.is_some_and(|v| now >= v * MS) {
            self.gone = true;
            if self.link.take().is_some() {
                self.report.ended.push("vanished".into());
            }
            return None;
        }
        if self.gone {
            return None;
        }
        if let Some(why) = self.link.as_ref().and_then(|l| l.expired(now)) {
            self.close(now, why);
            return None;
        }
        if self.link.is_some() || now < self.next_adv || !self.present(now) {
            return None;
        }
        let channel = 37 + self.adv_index;
        self.adv_index += 1;
        if self.adv_index < 3 {
            self.next_adv = now + ADV_SPACING;
        } else {
            self.adv_index = 0;
            let delay = mix(&mut self.rng) % 10_001 * TICKS_PER_US;
            self.next_adv = now + self.config.adv_interval_ms * MS + delay - 2 * ADV_SPACING;
        }
        self.report.adv_pdus += 1;
        let mut name = vec![self.config.name.len() as u8 + 1, 0x09];
        name.extend(self.config.name.bytes());
        Some(AdvOut {
            channel,
            pdu: self.pdu(adv::ADV_IND, &Self::adv_data()),
            scan_rsp: self.pdu(adv::SCAN_RSP, &name),
            rssi: 0x0C08,
        })
    }

    /// The scanner's SCAN_REQ for us was answered.
    pub fn scan_request(&mut self) {
        self.report.scan_reqs += 1;
    }

    /// A CONNECT_IND for our address, sent at `start`, ending at `end`.
    pub fn connect_ind(&mut self, start: u64, end: u64, cind: &[u8]) {
        if self.link.is_some() || self.gone || cind.len() != 36 || cind[8..14] != self.address {
            return;
        }
        let Some(link) = link::SlaveLink::new(start, end, cind) else {
            self.report
                .ended
                .push("CONNECT_IND with invalid LLData".into());
            return;
        };
        self.report.connections += 1;
        self.host = host::Host::default();
        self.link = Some(link);
    }

    fn close(&mut self, now: u64, why: String) {
        self.link = None;
        self.report.ended.push(why);
        self.adv_index = 0;
        self.next_adv = now + 20 * MS;
    }

    fn random16(&mut self) -> [u8; 16] {
        let mut r = [0; 16];
        fill(&mut self.rng, &mut r);
        r
    }

    /// One line for diagnose (`ble_peripheral name=... key=value ...`).
    pub fn report_line(&self) -> String {
        let r = &self.report;
        let hex = |a: &[u8]| {
            a.iter()
                .rev()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let window = match r.first_in_window {
            Some(true) => "yes",
            Some(false) => "no",
            None => "-",
        };
        format!(
            "ble_peripheral name={} addr={} identity={} adv={} scan_reqs={} conns={} established={} \
             in_window={window} offset_us={} first_event={} events={} ll={} mtu={} auth_errors={} \
             paired={} ltk_reuse={} encrypted={} subscribed={} ntf={} writes={} midi={} updates={} \
             chmaps={} param_result={} peer_identity={} ended={}",
            self.config.name.replace(' ', "_"),
            hex(&self.address),
            hex(&self.keys.identity),
            r.adv_pdus,
            r.scan_reqs,
            r.connections,
            r.established,
            r.first_offset_us,
            r.first_event,
            r.events,
            if r.ll_seen.is_empty() {
                "-".into()
            } else {
                r.ll_seen.join(",")
            },
            r.mtu,
            r.auth_errors,
            r.paired as u8,
            r.ltk_reuse,
            r.encrypted as u8,
            r.subscribed as u8,
            r.notifications,
            r.midi_writes,
            if r.midi.is_empty() {
                "-".into()
            } else {
                r.midi.join(",")
            },
            r.conn_updates,
            r.chmap_updates,
            r.param_result.map_or("-".into(), |v| v.to_string()),
            r.peer_identity.map_or("-".into(), |a| hex(&a)),
            if r.ended.is_empty() {
                "-".into()
            } else {
                r.ended.join("|").replace(' ', "_")
            },
        )
    }
}

#[cfg(test)]
mod tests;
