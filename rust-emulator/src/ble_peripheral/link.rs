// SPDX-License-Identifier: GPL-3.0-only
//! The peripheral's (slave's) link layer: the transmit window after the
//! CONNECT_IND, the anchors it re-synchronises to, CSA #1, SN / NESN with
//! retransmission, instants, the LL control procedures it answers, LL
//! encryption, supervision (Core Vol 6 Part B 4.5, 5.1; Part E).

use super::*;
use crate::ble_air::{airtime_ticks, csa1, ll, used_channels, ConnectParams, UNIT_1250_TICKS};
use crate::ble_crypto::Ccm;

/// How far from the expected time a master packet is still heard (window
/// widening plus a margin; the engine model's timing is exact).
const TOLERANCE: u64 = 40 * TICKS_PER_US;
const T_IFS: u64 = crate::ble_air::T_IFS_US * TICKS_PER_US;

/// LL_START_ENC_REQ / RSP, ENC_RSP opcodes (Core Vol 6 Part B 2.4.2).
const ENC_REQ: u8 = 0x03;
const ENC_RSP: u8 = 0x04;
const START_ENC_REQ: u8 = 0x05;
const START_ENC_RSP: u8 = 0x06;
/// LL_PAUSE_ENC_REQ / RSP: a new key on an encrypted link (5.1.3.2).
const PAUSE_ENC_REQ: u8 = 0x0A;
const PAUSE_ENC_RSP: u8 = 0x0B;
const PHY_RSP: u8 = 0x17;

pub(super) struct SlaveLink {
    pub params: ConnectParams,
    used: Vec<u8>,
    /// The master (InitA) and whether its address is random.
    pub peer: [u8; 6],
    pub peer_random: bool,
    win_start: u64,
    synced: bool,
    anchor: u64,
    event: u16,
    channel: u8,
    last_unmapped: u8,
    in_event: bool,
    event_close: u64,
    sn: u8,
    nesn: u8,
    in_flight: Option<Vec<u8>>,
    queue: VecDeque<Vec<u8>>,
    last_rx: u64,
    /// (instant, new parameters) / (instant, new map).
    update: Option<(u16, ConnectParams)>,
    map: Option<(u16, u64)>,
    /// The next event's window after an update (start, length).
    window: Option<(u64, u64)>,
    enc_tx: Option<Ccm>,
    enc_rx: Option<Ccm>,
    pending: Option<(Ccm, Ccm, bool)>,
    version_sent: bool,
    terminate: Option<u8>,
}

impl SlaveLink {
    pub fn new(_start: u64, end: u64, cind: &[u8]) -> Option<Self> {
        let params = ConnectParams::from_lldata(&cind[14..36])?;
        if params.interval < 6 || params.win_size == 0 || !(5..=16).contains(&params.hop) {
            return None;
        }
        let mut peer = [0; 6];
        peer.copy_from_slice(&cind[2..8]);
        let win_start = end + UNIT_1250_TICKS + params.win_offset as u64 * UNIT_1250_TICKS;
        Some(Self {
            used: used_channels(params.channel_map),
            params,
            peer,
            peer_random: cind[0] & 0x40 != 0,
            win_start,
            synced: false,
            anchor: win_start,
            event: 0,
            channel: 0,
            last_unmapped: 0,
            in_event: false,
            event_close: 0,
            sn: 0,
            nesn: 0,
            in_flight: None,
            queue: VecDeque::new(),
            last_rx: end,
            update: None,
            map: None,
            window: None,
            enc_tx: None,
            enc_rx: None,
            pending: None,
            version_sent: false,
            terminate: None,
        })
    }

    pub fn established(&self) -> bool {
        self.synced
    }

    pub fn encrypted(&self) -> bool {
        self.enc_tx.is_some() && self.enc_rx.is_some()
    }

    fn interval(&self) -> u64 {
        self.params.interval as u64 * UNIT_1250_TICKS
    }

    /// When the link ends unless the master is heard.
    pub fn deadline(&self) -> u64 {
        if self.synced {
            self.last_rx + self.params.timeout as u64 * 10 * MS
        } else {
            self.win_start
                + 6 * self.interval()
                + self.params.win_size as u64 * UNIT_1250_TICKS
                + TOLERANCE
        }
    }

    pub fn expired(&self, now: u64) -> Option<String> {
        (now >= self.deadline()).then(|| {
            if self.synced {
                "supervision timeout".into()
            } else {
                "never established: no master packet in six windows".into()
            }
        })
    }

    pub fn send(&mut self, pdu: Vec<u8>) {
        self.queue.push_back(pdu);
    }

    /// Queue an L2CAP basic frame in 27-octet fragments.
    pub fn send_l2cap(&mut self, cid: u16, payload: &[u8]) {
        let mut f = (payload.len() as u16).to_le_bytes().to_vec();
        f.extend(cid.to_le_bytes());
        f.extend_from_slice(payload);
        for (i, chunk) in f.chunks(27).enumerate() {
            let mut p = vec![if i == 0 { 2 } else { 1 }, chunk.len() as u8];
            p.extend_from_slice(chunk);
            self.queue.push_back(p);
        }
    }

    /// Step the event count to `event + 1`: CSA #1, instants.
    fn next_event(&mut self) -> (u64, u64) {
        let old = self.interval();
        let event = self.event.wrapping_add(1);
        let (start, len) = match self.update {
            Some((instant, p)) if instant == event => {
                self.params.interval = p.interval;
                self.params.timeout = p.timeout;
                self.update = None;
                let s = self.anchor + old + p.win_offset as u64 * UNIT_1250_TICKS;
                (s, p.win_size as u64 * UNIT_1250_TICKS)
            }
            _ => (self.anchor + old, 0),
        };
        if let Some((instant, map)) = self.map {
            if instant == event {
                self.params.channel_map = map;
                self.used = used_channels(map);
                self.map = None;
            }
        }
        (start, len)
    }

    /// Is a master packet at `now` on `channel` one we listen to? Moves the
    /// event schedule on (missed events consume their channels).
    fn listen(&mut self, now: u64, channel: u8, report: &mut Report) -> bool {
        if !self.synced {
            if now + TOLERANCE < self.win_start {
                return false;
            }
            let iv = self.interval();
            let k = (now + TOLERANCE - self.win_start) / iv;
            let ws = self.win_start + k * iv;
            if k > 6 || now > ws + self.params.win_size as u64 * UNIT_1250_TICKS + TOLERANCE {
                return false;
            }
            let mut last = 0;
            let mut ch = 0;
            for _ in 0..=k {
                (last, ch) = csa1(last, self.params.hop, &self.used);
            }
            if ch != channel {
                return false;
            }
            self.synced = true;
            self.anchor = now;
            self.in_event = false;
            self.event = k as u16;
            self.last_unmapped = last;
            self.channel = ch;
            if report.first_in_window.is_none() {
                report.first_in_window = Some(k == 0);
                report.first_offset_us = (now as i64 - self.win_start as i64) / TICKS_PER_US as i64;
                report.first_event = k as u16;
            }
            report.established += 1;
            return true;
        }
        if self.in_event && now <= self.event_close && channel == self.channel {
            return true;
        }
        for _ in 0..4096 {
            let (start, len) = self.window.unwrap_or_else(|| {
                let w = self.next_event();
                self.window = Some(w);
                w
            });
            if now + TOLERANCE < start {
                return false;
            }
            let (unmapped, ch) = csa1(self.last_unmapped, self.params.hop, &self.used);
            self.window = None;
            self.event = self.event.wrapping_add(1);
            self.last_unmapped = unmapped;
            self.channel = ch;
            if now <= start + len + TOLERANCE {
                self.anchor = now;
                self.in_event = false;
                return ch == channel;
            }
            self.anchor = start;
        }
        false
    }

    fn next_pdu(&mut self) -> Vec<u8> {
        if let Some(p) = &mut self.in_flight {
            p[0] = (p[0] & !4) | self.nesn << 2;
            return p.clone();
        }
        let mut pdu = self.queue.pop_front().unwrap_or_else(|| vec![1, 0]);
        let is_start_enc_req = pdu[0] & 3 == 3 && pdu.get(2) == Some(&START_ENC_REQ);
        let is_pause_rsp = pdu[0] & 3 == 3 && pdu.get(2) == Some(&PAUSE_ENC_RSP);
        if let (Some(c), true) = (self.enc_tx.as_mut(), pdu[1] > 0) {
            let body = c.encrypt(pdu[0], &pdu[2..]);
            pdu.truncate(2);
            pdu[1] = body.len() as u8;
            pdu.extend(body);
        }
        if is_start_enc_req {
            // From here the master's packets come encrypted.
            if let Some((_, rx, _)) = &self.pending {
                self.enc_rx = Some(rx.clone());
            }
        }
        if is_pause_rsp {
            // Ours went encrypted; the master's LL_PAUSE_ENC_RSP comes plain.
            self.enc_rx = None;
        }
        let md = !self.queue.is_empty();
        pdu[0] = (pdu[0] & 3) | self.nesn << 2 | self.sn << 3 | (md as u8) << 4;
        self.in_flight = Some(pdu.clone());
        pdu
    }
}

/// Fresh data from the master, after decryption.
pub(super) enum Fresh {
    Control(Vec<u8>),
    Data(bool, Vec<u8>),
}

impl Peripheral {
    /// A packet from the FM-1 (the master) at `now`: heard if this
    /// peripheral listens then, there, with that access address. Returns
    /// the reply (sent T_IFS after the packet's end).
    pub fn on_master_pdu(
        &mut self,
        now: u64,
        channel: u8,
        access_address: u32,
        crc_init: u32,
        pdu: &[u8],
    ) -> Option<Vec<u8>> {
        if self.gone || pdu.len() < 2 {
            return None;
        }
        let report = &mut self.report;
        let link = self.link.as_mut()?;
        if link.params.access_address != access_address || link.params.crc_init != crc_init {
            return None;
        }
        if !link.listen(now, channel, report) {
            return None;
        }
        if !link.in_event {
            report.events += 1;
        }
        link.last_rx = now;
        let (nesn, sn) = ((pdu[0] >> 2) & 1, (pdu[0] >> 3) & 1);
        if nesn != link.sn {
            link.in_flight = None;
            link.sn ^= 1;
        }
        let mut fresh = None;
        let mut mic_failed = false;
        if sn == link.nesn {
            link.nesn ^= 1;
            if pdu[1] > 0 {
                let payload = match link.enc_rx.as_mut() {
                    Some(c) => c.decrypt(pdu[0], &pdu[2..]),
                    None => Some(pdu[2..].to_vec()),
                };
                match payload {
                    Some(payload) => {
                        fresh = Some(match pdu[0] & 3 {
                            3 => Fresh::Control(payload),
                            l => Fresh::Data(l == 2, payload),
                        })
                    }
                    None => mic_failed = true,
                }
            }
        }
        if mic_failed {
            self.close(now, "MIC failure".into());
            return None;
        }
        match fresh {
            Some(Fresh::Control(p)) if !p.is_empty() => self.on_ll_control(&p),
            Some(Fresh::Data(start, p)) => self.on_l2cap_fragment(now, start, &p),
            _ => {}
        }
        self.host_event(now);
        let link = self.link.as_mut()?;
        let reply = link.next_pdu();
        let end = now + airtime_ticks(pdu.len() - 2);
        let reply_end = end + T_IFS + airtime_ticks(reply.len() - 2);
        link.in_event = true;
        link.event_close = reply_end + T_IFS + TOLERANCE;
        if let Some(reason) = link.terminate {
            // Our reply acknowledges the LL_TERMINATE_IND.
            self.close(now, format!("terminated by the master (0x{reason:02x})"));
        }
        Some(reply)
    }

    fn ll_seen(&mut self, op: u8) {
        let name = match op {
            ENC_REQ => "LL_ENC_REQ",
            START_ENC_RSP => "LL_START_ENC_RSP",
            PAUSE_ENC_REQ => "LL_PAUSE_ENC_REQ",
            PAUSE_ENC_RSP => "LL_PAUSE_ENC_RSP",
            0x18 => "LL_PHY_UPDATE_IND",
            _ => crate::ble_air::ll_name(op),
        };
        if !self.report.ll_seen.contains(&name) {
            self.report.ll_seen.push(name);
        }
    }

    fn on_ll_control(&mut self, p: &[u8]) {
        let op = p[0];
        self.ll_seen(op);
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let u16_at = |i: usize| {
            p.get(i..i + 2)
                .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
        };
        match op {
            ll::VERSION_IND if !link.version_sent => {
                link.version_sent = true;
                link.send(vec![3, 6, ll::VERSION_IND, 9, 0xFF, 0xFF, 0x01, 0x00]);
            }
            ll::VERSION_IND => {}
            ll::FEATURE_REQ | ll::SLAVE_FEATURE_REQ => {
                let mut r = vec![3, 9, ll::FEATURE_RSP, 0x21];
                r.extend([0; 7]);
                link.send(r);
            }
            ll::LENGTH_REQ => link.send(vec![
                3,
                9,
                ll::LENGTH_RSP,
                251,
                0,
                0x48,
                0x08,
                251,
                0,
                0x48,
                0x08,
            ]),
            ll::PING_REQ => link.send(vec![3, 1, ll::PING_RSP]),
            ll::CONNECTION_UPDATE_IND if p.len() >= 12 => {
                let mut n = link.params;
                n.win_size = p[1];
                n.win_offset = u16_at(2);
                n.interval = u16_at(4);
                n.latency = u16_at(6);
                n.timeout = u16_at(8);
                link.update = Some((u16_at(10), n));
                self.report.conn_updates += 1;
            }
            ll::CHANNEL_MAP_IND if p.len() >= 8 => {
                let mut m = [0u8; 8];
                m[..5].copy_from_slice(&p[1..6]);
                link.map = Some((u16_at(6), u64::from_le_bytes(m) & 0x1F_FFFF_FFFF));
                self.report.chmap_updates += 1;
            }
            ll::TERMINATE_IND => link.terminate = Some(p.get(1).copied().unwrap_or(0)),
            ENC_REQ if p.len() >= 23 => {
                let mut rand = [0u8; 8];
                rand.copy_from_slice(&p[1..9]);
                let ediv = u16_at(9);
                let mut skdm = [0u8; 8];
                skdm.copy_from_slice(&p[11..19]);
                self.enc_request(&rand, ediv);
                let ltk = self.ltk_for(&rand, ediv);
                let mut fresh = [0u8; 16];
                fill(&mut self.rng, &mut fresh);
                let link = self.link.as_mut().unwrap();
                let Some((ltk, bonded)) = ltk else {
                    link.send(vec![3, 2, ll::REJECT_IND, 0x06]);
                    return;
                };
                let mut skds = [0u8; 8];
                skds.copy_from_slice(&fresh[..8]);
                let mut iv = [0u8; 8];
                iv[..4].copy_from_slice(&p[19..23]);
                iv[4..].copy_from_slice(&fresh[8..12]);
                let key = ble_crypto::session_key(&ltk, &skdm, &skds);
                let tx = Ccm {
                    key,
                    iv,
                    counter: 0,
                    direction: 0,
                };
                let rx = Ccm {
                    direction: 1,
                    ..tx.clone()
                };
                link.pending = Some((tx, rx, bonded));
                let mut r = vec![3, 13, ENC_RSP];
                r.extend(skds);
                r.extend(&fresh[8..12]);
                link.send(r);
                link.send(vec![3, 1, START_ENC_REQ]);
            }
            // The encryption pause (5.1.3.2): ours encrypted, the master's
            // plain, then both directions plain until its LL_ENC_REQ.
            PAUSE_ENC_REQ if link.encrypted() => {
                link.send(vec![3, 1, PAUSE_ENC_RSP]);
                self.report.pauses += 1;
            }
            PAUSE_ENC_RSP if link.enc_tx.is_some() && link.enc_rx.is_none() => {
                link.enc_tx = None;
                self.host.link_auth = false;
            }
            START_ENC_RSP => {
                if let Some((tx, _, bonded)) = link.pending.take() {
                    link.enc_tx = Some(tx);
                    link.send(vec![3, 1, START_ENC_RSP]);
                    self.report.encrypted = true;
                    if bonded {
                        self.report.ltk_reuse += 1;
                    }
                    self.on_encrypted();
                }
            }
            ll::FEATURE_RSP
            | ll::UNKNOWN_RSP
            | ll::REJECT_IND
            | ll::REJECT_EXT_IND
            | ll::LENGTH_RSP
            | ll::PING_RSP => {}
            ll::PHY_REQ => link.send(vec![3, 3, PHY_RSP, 1, 1]),
            0x18 => {}
            _ => link.send(vec![3, 2, ll::UNKNOWN_RSP, op]),
        }
    }
}
