// SPDX-License-Identifier: GPL-3.0-only
//! The scanning state (link state 1, fact sheet §21.2) and the virtual
//! advertisers it hears: BLE-MIDI controllers, phones, other FM-1s and
//! beacons that only exist on the virtual air.
//!
//! Model, from §21.2 [IR] and [I]:
//! - a scan window opens one slot after the link starts and then every
//!   interval (columns 1 / 15, 625 µs slots); it lasts WINCNTL0/1 µs and
//!   listens on the channel in column 6 [5:0] as it was when the window
//!   opened (C12: a value written in the event ISR is for the next window);
//! - the event interrupt fires at the end of each window;
//! - a received advertising PDU is stored the connection-path way (§21.2
//!   "RX"): the RXTOG buffer, RXAHDR / RXDHDR / RXSTAT / RSSI2 / LASTCHMAP
//!   written, RXTOG moved on, RXBUFnCNTL bit0 set (C2: `scan_cntl`), and
//!   the RX interrupt raised at the end of the packet;
//! - with FORMAT bit8 clear the engine answers an ADV_IND / ADV_SCAN_IND in
//!   T_IFS with the SCAN_REQ in the TXTOG buffer, inserting the
//!   advertiser's AdvA where software left zeros (C1: `scan_adva`); an
//!   advertiser answers a SCAN_REQ for its own address with its SCAN_RSP,
//!   which is stored as any report.
//!
//! Nothing here ran against hardware.

use super::*;

/// The spacing of an advertiser's PDUs on 37 / 38 / 39 within one event.
const ADV_SPACING_US: u64 = 500;

/// The BLE-MIDI service UUID, least significant octet first.
pub const MIDI_UUID: [u8; 16] = [
    0x00, 0xC7, 0xC4, 0x4E, 0xE3, 0x6C, 0x51, 0xA7, 0x33, 0x4B, 0xE8, 0xED, 0x5A, 0x0E, 0xB8, 0x03,
];

/// A device advertising on the virtual air (not the FM-1, not the central).
#[derive(Clone, Debug)]
pub struct Advertiser {
    pub name: String,
    pub address: [u8; 6],
    pub random: bool,
    /// adv::ADV_IND, ADV_NONCONN_IND or ADV_SCAN_IND.
    pub kind: u8,
    pub adv_data: Vec<u8>,
    pub scan_rsp: Option<Vec<u8>>,
    pub interval_ms: u32,
    /// The RSSI word the engine writes for its packets (§3 RSSI2:
    /// [5:0] fine, [13:8] gain index; a bigger value is a weaker signal in
    /// this model).
    pub rssi: u16,
    next: u64,
    index: u8,
    seed: u32,
    /// Packets sent, SCAN_REQs for it heard, SCAN_RSPs sent.
    pub sent: u64,
    pub scan_reqs: u64,
    pub scan_rsps: u64,
}

fn ad(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![data.len() as u8 + 1, kind];
    v.extend_from_slice(data);
    v
}

fn flags_and_midi() -> Vec<u8> {
    let mut v = ad(0x01, &[0x06]);
    v.extend(ad(0x07, &MIDI_UUID));
    v
}

impl Advertiser {
    fn new(
        name: &str,
        address: [u8; 6],
        random: bool,
        kind: u8,
        adv: Vec<u8>,
        rsp: Option<Vec<u8>>,
    ) -> Self {
        let seed = address
            .iter()
            .fold(0x9E37_79B9u32, |s, b| s.rotate_left(5) ^ *b as u32);
        Self {
            name: name.into(),
            address,
            random,
            kind,
            adv_data: adv,
            scan_rsp: rsp,
            interval_ms: 100,
            rssi: 0x1010,
            next: 0,
            index: 0,
            seed,
            sent: 0,
            scan_reqs: 0,
            scan_rsps: 0,
        }
    }

    /// A BLE-MIDI peripheral: flags and the BLE-MIDI UUID in its ADV_IND,
    /// its complete name in the scan response (`name_in_rsp`) or in the
    /// ADV_IND after the UUID when it fits.
    pub fn midi(name: &str, address: [u8; 6], name_in_rsp: bool) -> Self {
        let mut adv = flags_and_midi();
        let named = ad(0x09, name.as_bytes());
        let rsp = if name_in_rsp || adv.len() + named.len() > 31 {
            Some(named)
        } else {
            adv.extend(named);
            Some(Vec::new())
        };
        Self::new(name, address, true, adv::ADV_IND, adv, rsp)
    }

    /// Something else (a tag, a watch): flags and its name, no BLE-MIDI UUID.
    pub fn beacon(name: &str, address: [u8; 6]) -> Self {
        let mut adv = ad(0x01, &[0x06]);
        adv.extend(ad(0x09, name.as_bytes()));
        Self::new(
            name,
            address,
            true,
            adv::ADV_IND,
            adv,
            Some(ad(0xFF, &[0x4C, 0x00, 0x02])),
        )
    }

    /// The set the firmware's DEVICES test listens to: three BLE-MIDI
    /// devices (a controller with its name in the scan response, one with
    /// it in the ADV_IND, another FM-1 named as ours are), a phone app
    /// behind a resolvable private address, a beacon, and a BLE-MIDI
    /// advertiser that is not connectable.
    pub fn defaults() -> Vec<Self> {
        let mut v = vec![
            Self::midi("KeyStep 37", [0x37, 0x37, 0x4B, 0x65, 0x79, 0xC4], true),
            Self::midi("WIDI", [0x11, 0x0D, 0x1D, 0x10, 0x20, 0xD1], false),
            Self::midi("FM-1 B00B", [0x0B, 0xB0, 0x42, 0x42, 0x42, 0xC9], true),
            Self::midi("iPad AUM", [0x01, 0x02, 0x03, 0x04, 0x05, 0x4A], true),
            Self::beacon("Tile", [0x7E, 0x11, 0x00, 0x00, 0x00, 0xE0]),
            Self::midi("NonConn MIDI", [0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0xCC], true),
        ];
        v[0].rssi = 0x0805; // strong
        v[1].rssi = 0x1810; // middle
        v[2].rssi = 0x2A30; // weak
        v[3].rssi = 0x1208;
        v[3].interval_ms = 30;
        v[5].kind = adv::ADV_NONCONN_IND;
        v[5].scan_rsp = None;
        for (i, a) in v.iter_mut().enumerate() {
            a.next = (3 + 7 * i as u64) * 1_000 * TICKS_PER_US;
        }
        v
    }

    /// `default`, or `;`-separated specs `midi:NAME[:adv][:ms=N][:rssi=0xNNNN]`
    /// / `beacon:NAME`; addresses are made from the position in the list.
    pub fn parse(spec: &str) -> Result<Vec<Self>, String> {
        if spec == "default" {
            return Ok(Self::defaults());
        }
        let mut out = Vec::new();
        for (i, item) in spec
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .enumerate()
        {
            let mut parts = item.split(':');
            let kind = parts.next().unwrap_or("");
            let name = parts.next().ok_or(format!("{item}: no name"))?;
            let address = [0x10 + i as u8, 0x20, 0x30, 0x40, 0x50, 0xC0 | i as u8];
            let mut a = match kind {
                "midi" => Self::midi(name, address, true),
                "beacon" => Self::beacon(name, address),
                _ => return Err(format!("{item}: midi or beacon")),
            };
            for opt in parts {
                if opt == "adv" {
                    a = Self::midi(name, address, false);
                } else if let Some(v) = opt.strip_prefix("ms=") {
                    a.interval_ms = v.parse().map_err(|_| format!("{item}: ms"))?;
                } else if let Some(v) = opt.strip_prefix("rssi=") {
                    a.rssi = u16::from_str_radix(v.trim_start_matches("0x"), 16)
                        .map_err(|_| format!("{item}: rssi"))?;
                } else {
                    return Err(format!("{item}: unknown option {opt}"));
                }
            }
            a.next = (3 + 7 * i as u64) * 1_000 * TICKS_PER_US;
            out.push(a);
        }
        Ok(out)
    }

    fn pdu(&self, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut p = vec![kind | (self.random as u8) << 6, 6 + data.len() as u8];
        p.extend_from_slice(&self.address);
        p.extend_from_slice(data);
        p
    }

    pub(super) fn next_tick(&self) -> u64 {
        self.next
    }
}

/// An open or coming scan window of a link in state 1.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ScanWin {
    pub start: u64,
    pub end: u64,
    pub channel: u8,
    pub open: bool,
}

/// Who put an advertising PDU on the air.
#[derive(Clone, Copy, Debug)]
pub(super) enum AirSource {
    Advertiser(usize),
    Peripheral(usize),
}

/// A packet the scanner received, stored when it ends.
#[derive(Clone, Debug)]
pub(super) struct PendingRx {
    pub at: u64,
    pub link: usize,
    pub pdu: Vec<u8>,
    pub channel: u8,
    pub rssi: u16,
}

impl Engine {
    /// Column 14 started a link in state 1: its first window one slot
    /// later (as advertising's first event, §6 step 12).
    pub(super) fn start_scan(&mut self, link: usize) {
        let start = self.now + SLOT;
        self.links[link].scan = Some(ScanWin {
            start,
            ..ScanWin::default()
        });
        self.links[link].action = Some((start, Action::ScanOpen));
        self.note(format!("link {link} scanning started"));
    }

    /// A window opens: its length from WINCNTL0/1, its channel from column 6.
    pub(super) fn scan_open(&mut self, link: usize, block: Block, w: &mut Window) -> Res<()> {
        let window = block.window_us(w)? as u64 * TICKS_PER_US;
        let col6 = self.columns[6][link];
        let channel = (col6 & 63) as u8;
        if col6 & 0x2000 == 0 || !(37..=39).contains(&channel) {
            self.note(format!(
                "link {link}: scan window with column 6 = {col6:#06x} (not 0x2100 | 37..39)"
            ));
        }
        let start = self.now;
        let end = start + window.max(TICKS_PER_US);
        self.links[link].scan = Some(ScanWin {
            start,
            end,
            channel,
            open: true,
        });
        if self.link_state(link) == 3 {
            self.stats.init_windows += 1;
        } else {
            self.stats.scan_windows += 1;
        }
        self.links[link].action = Some((end, Action::ScanEnd));
        Ok(())
    }

    /// A window closes: the event interrupt; the next one an interval after
    /// this one opened (columns 1 / 15; scanning writes column 15 bit15 0).
    pub(super) fn scan_end(&mut self, link: usize) {
        let Some(s) = self.links[link].scan else {
            return;
        };
        self.raise(link, false);
        // Initiating ends with the engine's CONNECT_IND when column 9 bit0
        // is set (§21.3 step 8 [I]).
        if self.link_state(link) == 3
            && self.pending_master[link].is_some()
            && self.columns[9][link] & 1 != 0
        {
            self.links[link].scan = None;
            return;
        }
        let next = (s.start + self.interval_slots(link).max(1) * SLOT).max(self.now + 1);
        self.links[link].scan = Some(ScanWin {
            open: false,
            start: next,
            ..s
        });
        self.links[link].action = Some((next, Action::ScanOpen));
    }

    /// The link (if any) listening on `channel` now.
    fn scanner_on(&self, channel: u8) -> Option<usize> {
        (0..LINKS).find(|&l| {
            self.links[l].running
                && matches!(self.link_state(l), 1 | 3)
                && self.links[l].scan.is_some_and(|s| {
                    s.open && s.channel == channel && s.start <= self.now && self.now < s.end
                })
        })
    }

    pub(super) fn next_advertiser_tick(&self) -> Option<u64> {
        self.advertisers.iter().map(Advertiser::next_tick).min()
    }

    pub(super) fn next_pending_tick(&self) -> Option<u64> {
        self.pending_rx.iter().map(|p| p.at).min()
    }

    /// Store every received packet that has ended (RX path of §21.2).
    pub(super) fn store_pending(&mut self, w: &mut Window) -> Res<bool> {
        let now = self.now;
        let due: Vec<_> = self
            .pending_rx
            .iter()
            .filter(|p| p.at <= now)
            .cloned()
            .collect();
        if due.is_empty() {
            return Ok(false);
        }
        self.pending_rx.retain(|p| p.at > now);
        for p in due {
            if !self.links[p.link].running || !matches!(self.link_state(p.link), 1 | 3) {
                continue;
            }
            let Some(block) = w.link(p.link)? else {
                continue;
            };
            let b = (block.get(w, field::RXTOG)? & 1) as u32;
            let len = p.pdu[1] as u16;
            if len > block.get(w, field::RXMAXBUF)? {
                continue;
            }
            let pointer = block.get(w, field::RXPTR0 + 2 * b)? as u32;
            w.set_bytes(pointer, &p.pdu[2..])?;
            block.set(w, field::RXAHDR0 + 2 * b, p.pdu[0] as u16)?;
            block.set(w, field::RXDHDR0 + 2 * b, len << 8)?;
            block.set(w, field::RXSTAT0 + 2 * b, 1)?;
            block.set(w, field::RSSI2, p.rssi)?;
            block.set(w, field::LASTCHMAP, p.channel as u16)?;
            if self.config.scan_cntl {
                let c = block.get8(w, field::RXBUF0CNTL + b)?;
                block.set8(w, field::RXBUF0CNTL + b, c | 1)?;
            }
            let tog = block.get(w, field::RXTOG)?;
            block.set(w, field::RXTOG, tog ^ 1)?;
            self.stats.scan_reports += 1;
            self.raise(p.link, true);
        }
        Ok(true)
    }

    /// The advertiser due now sends its next PDU (37, 38, 39, then its
    /// next event after the interval and a 0-10 ms advDelay).
    pub(super) fn advertiser_sends(&mut self, w: &mut Window) -> Res<()> {
        let now = self.now;
        let Some(i) = self.advertisers.iter().position(|a| a.next <= now) else {
            return Ok(());
        };
        let a = &mut self.advertisers[i];
        let channel = 37 + a.index;
        let pdu = a.pdu(a.kind, &a.adv_data.clone());
        a.sent += 1;
        a.index += 1;
        if a.index < 3 {
            a.next = now + ADV_SPACING_US * TICKS_PER_US;
        } else {
            a.index = 0;
            a.seed = a.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let delay = (a.seed >> 8) as u64 % 10_001;
            a.next = now + (a.interval_ms as u64 * 1_000 + delay) * TICKS_PER_US
                - 2 * ADV_SPACING_US * TICKS_PER_US;
        }
        let (rssi, rsp) = (a.rssi, a.scan_rsp.clone());
        let rsp_pdu = rsp.map(|d| self.advertisers[i].pdu(adv::SCAN_RSP, &d));
        self.air_adv(w, AirSource::Advertiser(i), channel, pdu, rsp_pdu, rssi)
    }

    /// An advertising PDU on `channel` now: a scanning link stores it (and
    /// its engine may send a SCAN_REQ, answered by the SCAN_RSP `rsp`); an
    /// initiating link may answer it with the CONNECT_IND.
    pub(super) fn air_adv(
        &mut self,
        w: &mut Window,
        src: AirSource,
        channel: u8,
        pdu: Vec<u8>,
        rsp_pdu: Option<Vec<u8>>,
        rssi: u16,
    ) -> Res<()> {
        let now = self.now;
        let kind = pdu[0] & 15;
        let mut address = [0u8; 6];
        address.copy_from_slice(&pdu[2..8]);
        let end = now + airtime_ticks(pdu.len() - 2);
        let Some(link) = self.scanner_on(channel) else {
            return Ok(());
        };
        self.pending_rx.push(PendingRx {
            at: end,
            link,
            pdu: pdu.clone(),
            channel,
            rssi,
        });
        let initiating = self.link_state(link) == 3;
        self.log(AirRecord {
            tick: now,
            sender: Sender::Advertiser,
            channel,
            access_address: ADV_ACCESS_ADDRESS,
            event: None,
            pdu: pdu.clone(),
            note: if initiating {
                "heard by the initiator".into()
            } else {
                "heard by the scanner".into()
            },
        });
        if initiating {
            if let Some((at, cind_end, cind)) = self.initiator_hears(link, w, &pdu, end, channel)? {
                if let AirSource::Peripheral(i) = src {
                    self.peripherals[i].connect_ind(at, cind_end, &cind);
                }
            }
            return Ok(());
        }
        let block = w
            .link(link)?
            .ok_or("a scanning link has no control block")?;
        // FORMAT bit8 clear: the engine's SCAN_REQ (§21.2 step 6, backoff).
        if block.get(w, field::FORMAT)? & 0x100 != 0
            || (kind != adv::ADV_IND && kind != adv::ADV_SCAN_IND)
        {
            return Ok(());
        }
        let b = (block.get(w, field::TXTOG)? & 1) as u32;
        let mut req = Self::adv_buffer(block, w, b)?;
        let req_at = end + T_IFS;
        let mut note = "the engine's SCAN_REQ";
        if req.len() != 14 || req[0] & 15 != adv::SCAN_REQ {
            self.note(format!(
                "link {link}: TX buffer {b} holds no SCAN_REQ while scanning: none sent"
            ));
            return Ok(());
        }
        if self.config.scan_adva && req[8..14] == [0; 6] {
            req[8..14].copy_from_slice(&address); // C1: the engine fills AdvA in
            note = "the engine's SCAN_REQ, AdvA filled in";
        }
        let answered = req[8..14] == address && rsp_pdu.is_some();
        self.stats.scan_reqs += 1;
        let req_end = req_at + airtime_ticks(12);
        self.log(AirRecord {
            tick: req_at,
            sender: Sender::Peripheral,
            channel,
            access_address: ADV_ACCESS_ADDRESS,
            event: None,
            pdu: req,
            note: note.into(),
        });
        if !answered {
            return Ok(());
        }
        match src {
            AirSource::Advertiser(i) => {
                self.advertisers[i].scan_reqs += 1;
                self.advertisers[i].scan_rsps += 1;
            }
            AirSource::Peripheral(i) => self.peripherals[i].scan_request(),
        }
        let rsp = rsp_pdu.unwrap_or_default();
        let rsp_at = req_end + T_IFS;
        self.pending_rx.push(PendingRx {
            at: rsp_at + airtime_ticks(rsp.len() - 2),
            link,
            pdu: rsp.clone(),
            channel,
            rssi,
        });
        self.stats.scan_responses_heard += 1;
        self.log(AirRecord {
            tick: rsp_at,
            sender: Sender::Advertiser,
            channel,
            access_address: ADV_ACCESS_ADDRESS,
            event: None,
            pdu: rsp,
            note: "SCAN_RSP".into(),
        });
        Ok(())
    }
}
