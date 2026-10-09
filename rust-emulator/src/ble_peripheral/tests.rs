// SPDX-License-Identifier: GPL-3.0-only
//! The virtual peripheral against a scripted master (the Core's master
//! side, written here independently of the engine model).

use super::host::{decode_midi, H_MIDI_CCC, H_MIDI_IO, MIDI_IO_UUID};
use super::*;
use crate::ble_air::{
    airtime_ticks, csa1, used_channels, ConnectParams, T_IFS_US, UNIT_1250_TICKS,
};
use crate::ble_crypto::{c1, rpa_resolves, s1, session_key, Ccm, PairingContext};

const OWN: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0xC6];

struct Master {
    p: Peripheral,
    params: ConnectParams,
    used: Vec<u8>,
    anchor: u64,
    last: u8,
    sn: u8,
    nesn: u8,
    queue: VecDeque<Vec<u8>>,
    in_flight: Option<Vec<u8>>,
    frames: Vec<(u16, Vec<u8>)>,
    reasm: Vec<u8>,
    ll: Vec<Vec<u8>>,
    tx: Option<Ccm>,
    rx: Option<Ccm>,
    answered: u32,
}

impl Master {
    /// Advertising heard, then a CONNECT_IND; the first packet `delay_us`
    /// into the transmit window.
    fn connect(config: PeripheralConfig, delay_us: u64) -> Self {
        let mut p = Peripheral::new(config);
        let mut t = 0;
        let out = loop {
            t = p.next_tick().unwrap().max(t);
            if let Some(o) = p.tick(t) {
                break o;
            }
        };
        assert_eq!(out.pdu[0] & 15, adv::ADV_IND);
        assert_eq!(&out.pdu[2..8], &p.address);
        let params = ConnectParams {
            interval: 9,
            timeout: 200,
            win_offset: 4,
            hop: 9,
            ..ConnectParams::default()
        };
        let mut cind = vec![0x05 | 0x40 | 0x80, 34];
        cind.extend(OWN);
        cind.extend(p.address);
        cind.extend(params.lldata());
        let start = t + airtime_ticks(out.pdu.len() - 2) + T_IFS_US * TICKS_PER_US;
        let end = start + airtime_ticks(34);
        p.connect_ind(start, end, &cind);
        let anchor =
            end + UNIT_1250_TICKS * (1 + params.win_offset as u64) + delay_us * TICKS_PER_US;
        Self {
            p,
            used: used_channels(params.channel_map),
            params,
            anchor,
            last: 0,
            sn: 0,
            nesn: 0,
            queue: VecDeque::new(),
            in_flight: None,
            frames: Vec::new(),
            reasm: Vec::new(),
            ll: Vec::new(),
            tx: None,
            rx: None,
            answered: 0,
        }
    }

    fn event(&mut self) {
        let (u, ch) = csa1(self.last, self.params.hop, &self.used);
        self.last = u;
        let mut at = self.anchor;
        for _ in 0..4 {
            let pdu = match &mut self.in_flight {
                Some(p) => {
                    p[0] = (p[0] & !4) | self.nesn << 2;
                    p.clone()
                }
                None => {
                    let mut p = self.queue.pop_front().unwrap_or_else(|| vec![1, 0]);
                    if let (Some(c), true) = (self.tx.as_mut(), p[1] > 0) {
                        let body = c.encrypt(p[0], &p[2..]);
                        p.truncate(2);
                        p[1] = body.len() as u8;
                        p.extend(body);
                    }
                    p[0] = (p[0] & 3) | self.nesn << 2 | self.sn << 3;
                    self.in_flight = Some(p.clone());
                    p
                }
            };
            let aa = self.params.access_address;
            let Some(r) = self.p.on_master_pdu(at, ch, aa, self.params.crc_init, &pdu) else {
                break;
            };
            self.answered += 1;
            if (r[0] >> 2) & 1 != self.sn {
                self.in_flight = None;
                self.sn ^= 1;
            }
            if (r[0] >> 3) & 1 == self.nesn {
                self.nesn ^= 1;
                if r[1] > 0 {
                    let mut body = r[2..].to_vec();
                    if let Some(c) = self.rx.as_mut() {
                        body = c.decrypt(r[0], &body).expect("the peripheral's MIC");
                    }
                    self.take(r[0] & 3, body);
                }
            }
            let end = at + airtime_ticks(pdu.len() - 2) + T_IFS_US * TICKS_PER_US;
            at = end + airtime_ticks(r.len() - 2) + T_IFS_US * TICKS_PER_US;
            if r[0] & 0x10 == 0 && self.queue.is_empty() {
                break;
            }
        }
        self.anchor += self.params.interval as u64 * UNIT_1250_TICKS;
    }

    fn take(&mut self, llid: u8, body: Vec<u8>) {
        match llid {
            3 => self.ll.push(body),
            2 => {
                self.reasm = body;
                self.frame_done();
            }
            _ => {
                self.reasm.extend(body);
                self.frame_done();
            }
        }
    }

    fn frame_done(&mut self) {
        if self.reasm.len() >= 4
            && self.reasm.len() == u16::from_le_bytes([self.reasm[0], self.reasm[1]]) as usize + 4
        {
            let cid = u16::from_le_bytes([self.reasm[2], self.reasm[3]]);
            self.frames.push((cid, self.reasm[4..].to_vec()));
            self.reasm.clear();
        }
    }

    fn events(&mut self, n: u32) {
        for _ in 0..n {
            self.event();
        }
    }

    fn ll_send(&mut self, p: &[u8]) {
        let mut v = vec![3, p.len() as u8];
        v.extend_from_slice(p);
        self.queue.push_back(v);
    }

    fn l2cap(&mut self, cid: u16, p: &[u8]) {
        let mut f = (p.len() as u16).to_le_bytes().to_vec();
        f.extend(cid.to_le_bytes());
        f.extend_from_slice(p);
        for (i, c) in f.chunks(27).enumerate() {
            let mut v = vec![if i == 0 { 2 } else { 1 }, c.len() as u8];
            v.extend_from_slice(c);
            self.queue.push_back(v);
        }
    }

    /// The next frame on `cid` (running events until it comes).
    fn frame(&mut self, cid: u16) -> Vec<u8> {
        for _ in 0..40 {
            if let Some(i) = self.frames.iter().position(|f| f.0 == cid) {
                return self.frames.remove(i).1;
            }
            self.event();
        }
        panic!("no frame on cid {cid}");
    }

    fn att(&mut self, req: &[u8]) -> Vec<u8> {
        self.l2cap(4, req);
        self.frame(4)
    }

    fn ll_reply(&mut self, op: u8) -> Vec<u8> {
        for _ in 0..20 {
            if let Some(i) = self.ll.iter().position(|p| p[0] == op) {
                return self.ll.remove(i);
            }
            self.event();
        }
        panic!("no LL 0x{op:02x}");
    }

    /// Start the LL encryption as master with `ltk`, EDIV / Rand.
    fn encrypt(&mut self, ltk: &[u8; 16], ediv: u16, rand: &[u8; 8]) {
        let skdm = [1, 2, 3, 4, 5, 6, 7, 8u8];
        let ivm = [9, 10, 11, 12u8];
        let mut req = vec![0x03];
        req.extend(rand);
        req.extend(ediv.to_le_bytes());
        req.extend(skdm);
        req.extend(ivm);
        self.ll_send(&req);
        let rsp = self.ll_reply(0x04);
        let mut skds = [0u8; 8];
        skds.copy_from_slice(&rsp[1..9]);
        let mut iv = [0u8; 8];
        iv[..4].copy_from_slice(&ivm);
        iv[4..].copy_from_slice(&rsp[9..13]);
        let c = Ccm {
            key: session_key(ltk, &skdm, &skds),
            iv,
            counter: 0,
            direction: 1,
        };
        self.ll_reply(0x05);
        self.rx = Some(Ccm {
            direction: 0,
            ..c.clone()
        });
        self.tx = Some(c);
        self.ll_send(&[0x06]);
        self.ll_reply(0x06);
    }

    fn discover(&mut self) -> (u16, u16) {
        let mut req = vec![0x06, 1, 0, 0xFF, 0xFF, 0x00, 0x28];
        req.extend(MIDI_UUID);
        let r = self.att(&req);
        assert_eq!(r[0], 0x07);
        let (s, e) = (
            u16::from_le_bytes([r[1], r[2]]),
            u16::from_le_bytes([r[3], r[4]]),
        );
        let mut q = vec![0x08];
        q.extend(s.to_le_bytes());
        q.extend(e.to_le_bytes());
        q.extend([0x03, 0x28]);
        let r = self.att(&q);
        assert_eq!((r[0], r[1]), (0x09, 21));
        assert_eq!(&r[7..23], &MIDI_IO_UUID);
        let value = u16::from_le_bytes([r[5], r[6]]);
        let mut f = vec![0x04];
        f.extend((value + 1).to_le_bytes());
        f.extend(e.to_le_bytes());
        let r = self.att(&f);
        assert_eq!((r[0], r[1]), (0x05, 1));
        let cccd = (2..r.len())
            .step_by(4)
            .find(|&i| r[i + 2..i + 4] == [0x02, 0x29])
            .map(|i| u16::from_le_bytes([r[i], r[i + 1]]))
            .unwrap();
        (value, cccd)
    }
}

#[test]
fn plain_connection_discovery_notes_and_midi_in() {
    let mut m = Master::connect(PeripheralConfig::new("KeyStep"), 500);
    m.ll_send(&[0x0C, 9, 0xFF, 0xFF, 1, 0]);
    assert_eq!(m.ll_reply(0x0C)[1], 9);
    let r = &m.p.report;
    assert_eq!(r.first_in_window, Some(true));
    assert!(
        (490..=510).contains(&r.first_offset_us),
        "{}",
        r.first_offset_us
    );
    let r = m.att(&[0x02, 247, 0]);
    assert_eq!(r, [0x03, 185, 0]);
    // The MIDI service is the fourth: the first Read By Group Type answer holds the three 16-bit ones.
    let r = m.att(&[0x10, 1, 0, 0xFF, 0xFF, 0x00, 0x28]);
    assert_eq!((r[0], r[1]), (0x11, 6));
    let (value, cccd) = m.discover();
    assert_eq!((value, cccd), (H_MIDI_IO, H_MIDI_CCC));
    let mut w = vec![0x12];
    w.extend(cccd.to_le_bytes());
    w.extend([1, 0]);
    assert_eq!(m.att(&w), [0x13]);
    m.events(40);
    let notes: Vec<_> = m
        .frames
        .iter()
        .filter(|f| f.0 == 4 && f.1[0] == 0x1B)
        .collect();
    assert!(notes.len() >= 2, "{} notifications", notes.len());
    assert_eq!(decode_midi(&notes[0].1[3..]), [vec![0x90, 60, 100]]);
    let mut cmd = vec![0x52];
    cmd.extend(value.to_le_bytes());
    cmd.extend([0x80, 0x81, 0x90, 0x3C, 0x64, 0x3E, 0x50, 0x82, 0xB0, 7, 100]);
    m.l2cap(4, &cmd);
    m.events(3);
    assert_eq!(m.p.report.midi, ["903c64", "903e50", "b00764"]);
    m.ll_send(&[0x02, 0x13]);
    m.events(3);
    assert_eq!(m.p.report.ended, ["terminated by the master (0x13)"]);
    assert!(m.p.report_line().contains("in_window=yes"));
}

#[test]
fn a_late_first_packet_never_establishes() {
    let mut m = Master::connect(PeripheralConfig::new("Late"), 3_000);
    m.events(10);
    assert_eq!(m.answered, 0);
    let mut t = m.anchor;
    while m.p.tick(t).is_none() && m.p.connected() {
        t += MS;
    }
    assert!(m.p.report.ended[0].starts_with("never established"));
    assert_eq!(m.p.report.first_in_window, None);
}

/// Pair as the FM-1 would (Just Works, bonding, both key sets), then a
/// second boot of the master reconnects with the bond's LTK.
#[test]
fn pairing_required_bonds_and_reconnects_with_the_ltk() {
    let mut cfg = PeripheralConfig::new("iPad AUM");
    cfg.pair = true;
    cfg.rpa = true;
    let mut m = Master::connect(cfg.clone(), 200);
    assert!(rpa_resolves(&m.p.keys.irk, &m.p.address));
    let (_, cccd) = m.discover();
    let mut w = vec![0x12];
    w.extend(cccd.to_le_bytes());
    w.extend([1, 0]);
    let r = m.att(&w);
    assert_eq!(r, [0x01, 0x12, cccd as u8, 0, 0x05]);
    let preq = [0x01, 0x03, 0x00, 0x01, 16, 0x03, 0x03];
    m.l2cap(6, &preq);
    let pres = m.frame(6);
    assert_eq!(pres, [0x02, 0x03, 0x00, 0x01, 16, 0x03, 0x03]);
    let ctx = PairingContext {
        preq,
        pres: pres.clone().try_into().unwrap(),
        iat: 1,
        ia: OWN,
        rat: 1,
        ra: m.p.address,
    };
    let k0 = [0u8; 16];
    let mrand = [7u8; 16];
    let mut c = vec![0x03];
    c.extend(c1(&k0, &mrand, &ctx));
    m.l2cap(6, &c);
    let sconf = m.frame(6);
    let mut r = vec![0x04];
    r.extend(mrand);
    m.l2cap(6, &r);
    let srand = m.frame(6);
    let srand: [u8; 16] = srand[1..17].try_into().unwrap();
    assert_eq!(&sconf[1..17], &c1(&k0, &srand, &ctx));
    let stk = s1(&k0, &srand, &mrand);
    m.encrypt(&stk, 0, &[0; 8]);
    assert!(m.p.report.encrypted);
    let enc = m.frame(6);
    let ident = m.frame(6);
    let irk = m.frame(6);
    let id = m.frame(6);
    assert_eq!((enc[0], ident[0], irk[0], id[0]), (0x06, 0x07, 0x08, 0x09));
    assert_eq!(&irk[1..17], &m.p.keys.irk);
    assert_eq!(&id[2..8], &m.p.keys.identity);
    let ltk: [u8; 16] = enc[1..17].try_into().unwrap();
    let ediv = u16::from_le_bytes([ident[1], ident[2]]);
    let rand: [u8; 8] = ident[3..11].try_into().unwrap();
    for (op, len) in [(0x06u8, 17usize), (0x07, 11), (0x08, 17)] {
        let mut k = vec![op];
        k.resize(len, 0x42);
        m.l2cap(6, &k);
    }
    let mut a = vec![0x09, 1];
    a.extend(OWN);
    m.l2cap(6, &a);
    m.events(6);
    assert!(m.p.report.paired);
    assert_eq!(m.p.report.peer_identity, Some(OWN));
    assert_eq!(m.att(&w), [0x13]);
    m.events(20);
    assert!(m.p.report.notifications > 0);
    // The FM-1 reboots; the peripheral is the same device (same seed), its
    // RPA new: the bond's LTK starts the encryption, no pairing.
    cfg.boot = 1;
    let mut m2 = Master::connect(cfg, 100);
    assert_ne!(m2.p.address, m.p.address);
    assert!(rpa_resolves(&irk[1..17].try_into().unwrap(), &m2.p.address));
    m2.encrypt(&ltk, ediv, &rand);
    assert_eq!(m2.p.report.ltk_reuse, 1);
    let (_, cccd) = m2.discover();
    let mut w = vec![0x12];
    w.extend(cccd.to_le_bytes());
    w.extend([1, 0]);
    assert_eq!(m2.att(&w), [0x13]);
}

#[test]
fn an_unknown_ediv_is_rejected_and_the_options_parse() {
    let mut m = Master::connect(PeripheralConfig::new("X"), 0);
    let mut req = vec![0x03];
    req.extend([0u8; 22]);
    req[9] = 5;
    m.ll_send(&req);
    assert_eq!(m.ll_reply(0x0D), [0x0D, 0x06]);
    let v =
        PeripheralConfig::parse("midi:A:pair:rpa:appear=3000:seed=7;midi:B:secreq:upd:vanish=9000")
            .unwrap();
    assert_eq!(v.len(), 2);
    assert!(v[0].pair && v[0].rpa && v[0].appear_ms == 3000 && v[0].seed == 7);
    assert!(v[1].secreq && v[1].upd && v[1].vanish_ms == Some(9000));
    assert!(PeripheralConfig::parse("midi:A:bogus").is_err());
    assert_eq!(Keys::derive(7), Keys::derive(7));
}
