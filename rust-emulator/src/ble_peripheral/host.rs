// SPDX-License-Identifier: GPL-3.0-only
//! The peripheral's host: L2CAP basic frames, the LE signalling channel,
//! an ATT server over a controller-shaped GATT database (the BLE-MIDI
//! service is not the first), the SMP responder (LE legacy pairing, Just
//! Works or, for an auth peripheral, passkey entry with its user typing;
//! bonding) and the BLE-MIDI data path (Core Vol 3 Parts A, F, G, H; the
//! BLE-MIDI specification).

use super::*;
use crate::ble_crypto::{c1, s1, PairingContext};

const CID_ATT: u16 = 4;
const CID_SIG: u16 = 5;
const CID_SMP: u16 = 6;
/// Our ATT MTU (smaller than the FM-1's 247, so the exchange matters).
const OUR_MTU: u16 = 185;

/// The MIDI I/O characteristic 7772E5DB-3868-4112-A1A9-F2669D106BF3, LSB first.
pub const MIDI_IO_UUID: [u8; 16] = [
    0xF3, 0x6B, 0x10, 0x9D, 0x66, 0xF2, 0xA9, 0xA1, 0x12, 0x41, 0x68, 0x38, 0xDB, 0xE5, 0x72, 0x77,
];
const BASE_UUID: [u8; 16] = [
    0xFB, 0x34, 0x9B, 0x5F, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// The database's handles.
const H_SC_CCC: u16 = 9;
const H_BATT_CCC: u16 = 13;
pub const H_MIDI_IO: u16 = 16;
pub const H_MIDI_CCC: u16 = 17;
const H_LAST: u16 = 18;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SmpState {
    #[default]
    Idle,
    Confirm,
    /// Passkey entry: Mconfirm taken, waiting for its user to type the
    /// passkey the initiator shows (`Peripheral::type_passkey`).
    Passkey,
    Random,
    Encrypt,
    Keys,
}

#[derive(Default)]
struct Smp {
    state: SmpState,
    ctx: PairingContext,
    key_size: u8,
    mconfirm: [u8; 16],
    srand: [u8; 16],
    stk: Option<[u8; 16]>,
    ours: u8,
    expect: u8,
    /// The TK: 0 (Just Works) or the passkey typed (passkey entry).
    tk: [u8; 16],
    /// The method is passkey entry (the keys will be authenticated).
    passkey: bool,
}

#[derive(Default)]
pub(super) struct Host {
    rx: Vec<u8>,
    need: usize,
    mtu: u16,
    cccd: [u16; 3],
    started: bool,
    smp: Smp,
    notes_next: u64,
    note_on: bool,
    sig_id: u8,
    upd_sent: bool,
    /// The link's key is authenticated (now; and the one being started).
    link_auth: bool,
    pending_auth: bool,
}

struct Attr {
    handle: u16,
    uuid: Vec<u8>,
    value: Vec<u8>,
}

fn u16le(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

/// A UUID (2 or 16 octets, LSB first) in its 128-bit form.
fn full(uuid: &[u8]) -> Option<[u8; 16]> {
    match uuid.len() {
        2 => {
            let mut u = BASE_UUID;
            u[12] = uuid[0];
            u[13] = uuid[1];
            Some(u)
        }
        16 => uuid.try_into().ok(),
        _ => None,
    }
}

fn is16(a: &Attr, v: u16) -> bool {
    a.uuid.len() == 2 && a.uuid == u16le(v)
}

/// The MIDI messages in a BLE-MIDI packet (header, then timestamped
/// messages with running status; real time and SysEx as single messages).
pub fn decode_midi(p: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if p.len() < 2 || p[0] & 0x80 == 0 {
        return out;
    }
    let len_of = |s: u8| match s {
        0x80..=0xBF | 0xE0..=0xEF | 0xF2 => 2,
        0xC0..=0xDF | 0xF1 | 0xF3 => 1,
        _ => 0,
    };
    let (mut i, mut running) = (1, 0u8);
    while i < p.len() {
        if p[i] & 0x80 != 0 {
            i += 1; // a timestamp octet
            if i >= p.len() {
                break;
            }
            if p[i] & 0x80 != 0 {
                let s = p[i];
                i += 1;
                if s >= 0xF8 {
                    out.push(vec![s]);
                    continue;
                }
                if s == 0xF0 {
                    let mut m = vec![s];
                    while i < p.len() && p[i] != 0xF7 {
                        if p[i] < 0x80 {
                            m.push(p[i]);
                        }
                        i += 1;
                    }
                    m.push(0xF7);
                    i += 1;
                    out.push(m);
                    continue;
                }
                running = s;
                if len_of(s) == 0 {
                    out.push(vec![s]);
                    continue;
                }
            }
        }
        if running == 0 {
            i += 1;
            continue;
        }
        let n = len_of(running);
        if i + n > p.len() {
            break;
        }
        let mut m = vec![running];
        m.extend_from_slice(&p[i..i + n]);
        out.push(m);
        i += n;
    }
    out
}

impl Peripheral {
    fn db(&self) -> Vec<Attr> {
        let decl = |props: u8, h: u16, uuid: &[u8]| {
            let mut v = vec![props];
            v.extend(u16le(h));
            v.extend_from_slice(uuid);
            v
        };
        let a = |handle: u16, uuid: &[u8], value: Vec<u8>| Attr {
            handle,
            uuid: uuid.to_vec(),
            value,
        };
        let ccc = |i: usize| u16le(self.host.cccd[i]).to_vec();
        vec![
            a(1, &u16le(0x2800), u16le(0x1800).to_vec()),
            a(2, &u16le(0x2803), decl(0x02, 3, &u16le(0x2A00))),
            a(3, &u16le(0x2A00), self.config.name.as_bytes().to_vec()),
            a(4, &u16le(0x2803), decl(0x02, 5, &u16le(0x2A01))),
            a(5, &u16le(0x2A01), vec![0, 0]),
            a(6, &u16le(0x2800), u16le(0x1801).to_vec()),
            a(7, &u16le(0x2803), decl(0x20, 8, &u16le(0x2A05))),
            a(8, &u16le(0x2A05), vec![1, 0, 0xFF, 0xFF]),
            a(9, &u16le(0x2902), ccc(0)),
            a(10, &u16le(0x2800), u16le(0x180F).to_vec()),
            a(11, &u16le(0x2803), decl(0x12, 12, &u16le(0x2A19))),
            a(12, &u16le(0x2A19), vec![100]),
            a(13, &u16le(0x2902), ccc(1)),
            a(14, &u16le(0x2800), MIDI_UUID.to_vec()),
            a(15, &u16le(0x2803), decl(0x16, 16, &MIDI_IO_UUID)),
            a(16, &MIDI_IO_UUID, Vec::new()),
            a(17, &u16le(0x2902), ccc(2)),
            a(18, &u16le(0x2901), b"MIDI".to_vec()),
        ]
    }

    fn encrypted(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.encrypted())
    }

    /// The MIDI value and its CCCD: encrypted (pair), with an authenticated
    /// key (auth); else Insufficient Authentication.
    fn needs_encryption(&self, h: u16) -> bool {
        let secure = self.encrypted() && (!self.config.auth || self.host.link_auth);
        self.config.pair && (h == H_MIDI_IO || h == H_MIDI_CCC) && !secure
    }

    fn send(&mut self, cid: u16, payload: &[u8]) {
        if let Some(l) = self.link.as_mut() {
            l.send_l2cap(cid, payload);
        }
    }

    pub(super) fn on_l2cap_fragment(&mut self, now: u64, start: bool, p: &[u8]) {
        let h = &mut self.host;
        if start {
            if p.len() < 4 {
                return;
            }
            h.need = u16::from_le_bytes([p[0], p[1]]) as usize + 4;
            h.rx = p.to_vec();
        } else if h.need > 0 {
            h.rx.extend_from_slice(p);
        }
        if h.need == 0 || h.rx.len() < h.need {
            return;
        }
        let frame = std::mem::take(&mut h.rx);
        h.need = 0;
        let cid = u16::from_le_bytes([frame[2], frame[3]]);
        let body = &frame[4..];
        match cid {
            CID_ATT if !body.is_empty() => self.att(now, body),
            CID_SIG if body.len() >= 4 => self.signalling(body),
            CID_SMP if !body.is_empty() => self.smp(body),
            _ => {}
        }
    }

    /// Once per master packet: the Security Request, the notes.
    pub(super) fn host_event(&mut self, now: u64) {
        if !self.host.started {
            self.host.started = true;
            if self.config.secreq {
                self.send(CID_SMP, &[0x0B, 0x01]);
            }
        }
        let ready = self.host.cccd[2] & 1 != 0 && !self.needs_encryption(H_MIDI_IO);
        if !ready || !self.config.notes || now < self.host.notes_next {
            return;
        }
        let ms = (now / MS) as u16 & 0x1FFF;
        let on = !self.host.note_on;
        self.host.note_on = on;
        self.host.notes_next = now + 200 * MS;
        let mut n = vec![0x1B];
        n.extend(u16le(H_MIDI_IO));
        n.extend([
            0x80 | (ms >> 7) as u8,
            0x80 | (ms & 0x7F) as u8,
            if on { 0x90 } else { 0x80 },
            60,
            if on { 100 } else { 0 },
        ]);
        self.send(CID_ATT, &n);
        self.report.notifications += 1;
    }

    fn att_error(&mut self, op: u8, h: u16, err: u8) {
        if err == 0x05 {
            self.report.auth_errors += 1;
        }
        let mut e = vec![0x01, op];
        e.extend(u16le(h));
        e.push(err);
        self.send(CID_ATT, &e);
    }

    fn mtu(&self) -> usize {
        if self.host.mtu == 0 {
            23
        } else {
            self.host.mtu as usize
        }
    }

    fn group_end(db: &[Attr], h: u16) -> u16 {
        db.iter()
            .find(|a| a.handle > h && is16(a, 0x2800))
            .map_or(H_LAST, |a| a.handle - 1)
    }

    fn att(&mut self, now: u64, p: &[u8]) {
        let op = p[0];
        let u = |i: usize| {
            p.get(i..i + 2)
                .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
        };
        let db = self.db();
        let mtu = self.mtu();
        let range = |p: &[u8]| -> Option<(u16, u16)> {
            let (s, e) = (u(1), u(3));
            (p.len() >= 5 && s != 0 && s <= e).then_some((s, e))
        };
        match op {
            0x02 if p.len() == 3 => {
                self.host.mtu = u(1).clamp(23, OUR_MTU);
                self.report.mtu = self.host.mtu;
                let mut r = vec![0x03];
                r.extend(u16le(OUR_MTU));
                self.send(CID_ATT, &r);
            }
            0x04 => {
                let Some((s, e)) = range(p) else {
                    return self.att_error(op, u(1), 0x01);
                };
                let mut r = vec![0x05, 0];
                for a in db.iter().filter(|a| a.handle >= s && a.handle <= e) {
                    let fmt = if a.uuid.len() == 2 { 1 } else { 2 };
                    if r[1] == 0 {
                        r[1] = fmt;
                    } else if r[1] != fmt || r.len() + 2 + a.uuid.len() > mtu {
                        break;
                    }
                    r.extend(u16le(a.handle));
                    r.extend(&a.uuid);
                }
                if r[1] == 0 {
                    self.att_error(op, s, 0x0A);
                } else {
                    self.send(CID_ATT, &r);
                }
            }
            0x06 if p.len() >= 7 => {
                let Some((s, e)) = range(p) else {
                    return self.att_error(op, u(1), 0x01);
                };
                let (t, v) = (u(5), &p[7..]);
                let mut r = vec![0x07];
                for a in db.iter().filter(|a| a.handle >= s && a.handle <= e) {
                    if is16(a, t) && a.value == v && r.len() + 4 <= mtu {
                        let end = if t == 0x2800 {
                            Self::group_end(&db, a.handle)
                        } else {
                            a.handle
                        };
                        r.extend(u16le(a.handle));
                        r.extend(u16le(end));
                    }
                }
                if r.len() == 1 {
                    self.att_error(op, s, 0x0A);
                } else {
                    self.send(CID_ATT, &r);
                }
            }
            0x08 | 0x10 if p.len() == 7 || p.len() == 21 => {
                let Some((s, e)) = range(p) else {
                    return self.att_error(op, u(1), 0x01);
                };
                let want = full(&p[5..]);
                let group = op == 0x10;
                if group && want != full(&u16le(0x2800)) {
                    return self.att_error(op, s, 0x10);
                }
                let mut r = vec![op + 1, 0];
                for a in db.iter().filter(|a| a.handle >= s && a.handle <= e) {
                    if full(&a.uuid) != want {
                        continue;
                    }
                    if self.needs_encryption(a.handle) {
                        if r[1] == 0 {
                            return self.att_error(op, a.handle, 0x05);
                        }
                        break;
                    }
                    let head = if group { 4 } else { 2 };
                    let v = &a.value[..a.value.len().min(mtu - 2 - head)];
                    let el = (head + v.len()) as u8;
                    if r[1] == 0 {
                        r[1] = el;
                    } else if r[1] != el || r.len() + el as usize > mtu {
                        break;
                    }
                    r.extend(u16le(a.handle));
                    if group {
                        r.extend(u16le(Self::group_end(&db, a.handle)));
                    }
                    r.extend_from_slice(v);
                }
                if r[1] == 0 {
                    self.att_error(op, s, 0x0A);
                } else {
                    self.send(CID_ATT, &r);
                }
            }
            0x0A | 0x0C if p.len() >= 3 => {
                let h = u(1);
                let off = if op == 0x0C { u(3) as usize } else { 0 };
                let Some(a) = db.iter().find(|a| a.handle == h) else {
                    return self.att_error(op, h, 0x01);
                };
                if h == 8 {
                    return self.att_error(op, h, 0x02);
                }
                if self.needs_encryption(h) {
                    return self.att_error(op, h, 0x05);
                }
                if off > a.value.len() {
                    return self.att_error(op, h, 0x07);
                }
                let mut r = vec![op + 1];
                r.extend_from_slice(&a.value[off..a.value.len().min(off + mtu - 1)]);
                self.send(CID_ATT, &r);
            }
            0x12 | 0x52 if p.len() >= 3 => {
                let h = u(1);
                let err = self.att_write(now, h, &p[3..]);
                if op == 0x12 {
                    match err {
                        0 => self.send(CID_ATT, &[0x13]),
                        e => self.att_error(op, h, e),
                    }
                } else if err == 0x05 {
                    self.report.auth_errors += 1;
                }
            }
            0x1E => {}
            0x02 | 0x06 | 0x08 | 0x0A | 0x0C | 0x10 | 0x12 => self.att_error(op, 0, 0x04),
            _ if op & 0x40 == 0 && op & 1 == 0 => self.att_error(op, 0, 0x06),
            _ => {}
        }
    }

    fn att_write(&mut self, now: u64, h: u16, v: &[u8]) -> u8 {
        if h == 0 || h > H_LAST {
            return 0x01;
        }
        if self.needs_encryption(h) {
            return 0x05;
        }
        match h {
            H_MIDI_IO => {
                self.report.midi_writes += 1;
                for m in decode_midi(v) {
                    if self.report.midi.len() < 16 {
                        let hex: Vec<_> = m.iter().map(|b| format!("{b:02x}")).collect();
                        self.report.midi.push(hex.join(""));
                    }
                }
                0
            }
            H_SC_CCC | H_BATT_CCC | H_MIDI_CCC => {
                if v.len() != 2 {
                    return 0x0D;
                }
                let i = match h {
                    H_SC_CCC => 0,
                    H_BATT_CCC => 1,
                    _ => 2,
                };
                self.host.cccd[i] = u16::from_le_bytes([v[0], v[1]]);
                if h == H_MIDI_CCC && v[0] & 1 != 0 {
                    self.report.subscribed = true;
                    self.host.notes_next = now + 50 * MS;
                    if self.config.upd && !self.host.upd_sent {
                        self.host.upd_sent = true;
                        self.host.sig_id = self.host.sig_id.wrapping_add(1).max(1);
                        let mut r = vec![0x12, self.host.sig_id, 8, 0];
                        for v in [12u16, 12, 0, 200] {
                            r.extend(u16le(v));
                        }
                        self.send(CID_SIG, &r);
                    }
                }
                0
            }
            _ => 0x03,
        }
    }

    fn signalling(&mut self, p: &[u8]) {
        match p[0] {
            0x13 if p.len() >= 6 => {
                self.report.param_result = Some(u16::from_le_bytes([p[4], p[5]]))
            }
            0x01 => self.report.param_result = Some(0xFFFF),
            0x12 => self.send(CID_SIG, &[0x01, p[1], 2, 0, 0, 0]),
            _ => {}
        }
    }

    fn smp_fail(&mut self, why: u8) {
        self.host.smp.state = SmpState::Idle;
        self.send(CID_SMP, &[0x05, why]);
    }

    fn smp(&mut self, p: &[u8]) {
        let op = p[0];
        let lens = [0usize, 7, 7, 17, 17, 2, 17, 11, 17, 8, 17, 2];
        if (op as usize) < lens.len() && p.len() != lens[op as usize] {
            return self.smp_fail(0x0A);
        }
        match op {
            0x01 => {
                if p[4] < 7 {
                    return self.smp_fail(0x06);
                }
                let Some(link) = self.link.as_ref() else {
                    return;
                };
                let ours = p[6] & (1 | if self.config.rpa { 2 } else { 0 });
                // An auth peripheral is KeyboardDisplay and mirrors the MITM
                // flag; passkey entry with its user typing when MITM is asked
                // and the initiator can display (DisplayOnly / DisplayYesNo:
                // Core Vol 3 Part H 2.3.5.1, Table 2.8), else Just Works.
                let auth = self.config.auth;
                let (io, auth_req) = if auth {
                    (0x04, 0x01 | (p[3] & 0x04))
                } else {
                    (0x03, 0x01)
                };
                let passkey = auth && p[3] & 0x04 != 0 && matches!(p[1], 0x00 | 0x01);
                let pres = [0x02, io, 0x00, auth_req, 16, p[5] & 3, ours];
                let mut ctx = PairingContext {
                    pres,
                    iat: link.peer_random as u8,
                    ia: link.peer,
                    rat: 1,
                    ra: self.address,
                    ..Default::default()
                };
                ctx.preq.copy_from_slice(&p[..7]);
                self.host.smp = Smp {
                    state: SmpState::Confirm,
                    ctx,
                    key_size: p[4].min(16),
                    ours,
                    expect: p[5] & 3,
                    passkey,
                    ..Default::default()
                };
                self.send(CID_SMP, &pres);
            }
            0x03 if self.host.smp.state == SmpState::Confirm => {
                self.host.smp.mconfirm.copy_from_slice(&p[1..17]);
                if self.host.smp.passkey {
                    self.host.smp.state = SmpState::Passkey;
                    self.report.passkey_waits += 1;
                    return;
                }
                self.smp_sconfirm();
            }
            0x04 if self.host.smp.state == SmpState::Random => {
                let mut mrand = [0u8; 16];
                mrand.copy_from_slice(&p[1..17]);
                let tk = self.host.smp.tk;
                if c1(&tk, &mrand, &self.host.smp.ctx) != self.host.smp.mconfirm {
                    return self.smp_fail(0x04);
                }
                let srand = self.host.smp.srand;
                let mut stk = s1(&tk, &srand, &mrand);
                for b in stk.iter_mut().skip(self.host.smp.key_size as usize) {
                    *b = 0;
                }
                self.host.smp.stk = Some(stk);
                self.host.smp.state = SmpState::Encrypt;
                let mut r = vec![0x04];
                r.extend(srand);
                self.send(CID_SMP, &r);
            }
            0x05 => self.host.smp.state = SmpState::Idle,
            0x06..=0x0A if self.host.smp.state == SmpState::Keys => {
                match op {
                    0x07 => self.host.smp.expect &= !1,
                    0x09 => {
                        let mut a = [0u8; 6];
                        a.copy_from_slice(&p[2..8]);
                        self.report.peer_identity = Some(a);
                        self.host.smp.expect &= !2;
                    }
                    _ => {}
                }
                if self.host.smp.expect == 0 {
                    self.smp_done();
                }
            }
            _ if op <= 0x0B => {}
            _ => self.smp_fail(0x07),
        }
    }

    /// Sconfirm = c1(TK, Srand, ...) to the initiator (Core Vol 3 Part H
    /// 2.3.5.5).
    fn smp_sconfirm(&mut self) {
        let srand = self.random16();
        self.host.smp.srand = srand;
        let mut r = vec![0x03];
        r.extend(c1(&self.host.smp.tk, &srand, &self.host.smp.ctx));
        self.host.smp.state = SmpState::Random;
        self.send(CID_SMP, &r);
    }

    /// A passkey pairing waits for its user.
    pub fn wants_passkey(&self) -> bool {
        self.link.is_some() && self.host.smp.state == SmpState::Passkey
    }

    /// Its user types the passkey the initiator shows (0..999999; with
    /// `typo`, one more): TK = the passkey (2.3.5.3), Sconfirm goes.
    pub fn type_passkey(&mut self, shown: u32) {
        if !self.wants_passkey() {
            return;
        }
        let typed = if self.config.typo {
            (shown + 1) % 1_000_000
        } else {
            shown % 1_000_000
        };
        self.report.passkey = Some(typed);
        self.host.smp.tk = [0; 16];
        self.host.smp.tk[..4].copy_from_slice(&typed.to_le_bytes());
        self.smp_sconfirm();
    }

    /// Both key sets went: bonded. An auth peripheral refuses a Just Works
    /// bond afterwards with a Pairing Failed (Unspecified Reason), as the
    /// iPhone did on the FM-1 (the bond not taken, Insufficient
    /// Authentication on).
    fn smp_done(&mut self) {
        self.host.smp.state = SmpState::Idle;
        self.report.paired = true;
        if self.host.smp.passkey {
            self.report.auth_pairings += 1;
        } else if self.config.auth {
            self.report.late_fails += 1;
            self.send(CID_SMP, &[0x05, 0x08]);
        }
    }

    /// The keys it distributes in this pairing: its bond, or for an auth
    /// peripheral's Just Works pairing an unauthenticated one.
    fn distributed(&self) -> Keys {
        if self.config.auth && !self.host.smp.passkey {
            self.keys.unauthenticated()
        } else {
            self.keys.clone()
        }
    }

    /// The LTK for an LL_ENC_REQ (and whether it is the bond's): the STK of
    /// a pairing in progress (EDIV 0, Rand 0), else ours by EDIV / Rand.
    pub(super) fn ltk_for(&self, rand: &[u8; 8], ediv: u16) -> Option<([u8; 16], bool)> {
        if ediv == 0 && *rand == [0; 8] {
            return self.host.smp.stk.map(|k| (k, false));
        }
        if ediv == self.keys.ediv && *rand == self.keys.rand {
            return Some((self.keys.ltk, true));
        }
        let jw = self.keys.unauthenticated();
        (self.config.auth && ediv == jw.ediv && *rand == jw.rand).then_some((jw.ltk, true))
    }

    /// An LL_ENC_REQ: whether the key it asks for is authenticated (the STK
    /// of a passkey pairing, an auth peripheral's passkey bond).
    pub(super) fn enc_request(&mut self, rand: &[u8; 8], ediv: u16) {
        self.host.pending_auth = if ediv == 0 && *rand == [0; 8] {
            self.host.smp.passkey
        } else {
            self.config.auth && ediv == self.keys.ediv && *rand == self.keys.rand
        };
    }

    /// The link is encrypted both ways: a pairing's key distribution
    /// (the responder's keys first, Core Vol 3 Part H 3.6.1).
    pub(super) fn on_encrypted(&mut self) {
        self.host.link_auth = self.host.pending_auth;
        self.report.authenticated = self.host.link_auth;
        if self.host.smp.state != SmpState::Encrypt {
            return;
        }
        self.host.smp.state = SmpState::Keys;
        let ours = self.host.smp.ours;
        let k = self.distributed();
        if ours & 1 != 0 {
            let mut e = vec![0x06];
            e.extend(k.ltk);
            self.send(CID_SMP, &e);
            let mut m = vec![0x07];
            m.extend(u16le(k.ediv));
            m.extend(k.rand);
            self.send(CID_SMP, &m);
        }
        if ours & 2 != 0 {
            let mut i = vec![0x08];
            i.extend(k.irk);
            self.send(CID_SMP, &i);
            let mut a = vec![0x09, 1];
            a.extend(k.identity);
            self.send(CID_SMP, &a);
        }
        if self.host.smp.expect == 0 {
            self.smp_done();
        }
    }
}
