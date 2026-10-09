// SPDX-License-Identifier: GPL-3.0-only
//! The central role (fact sheet §21.3, §21.4): initiating (link state 3)
//! and the master connection (state 6), against the virtual peripherals of
//! `ble_peripheral`.
//!
//! Model, from §21.3 / §21.4 [IR] and [I]:
//! - initiating opens windows like scanning (columns 1 / 15, WINCNTL0/1,
//!   the channel in column 6 as the window opens); an advertising PDU heard
//!   is stored as a scan report is;
//! - an ADV_IND (or an ADV_DIRECT_IND to LOCALADR) from WHITELIST0 (type
//!   FILTERCNTL bit8, with bits 0 and 4 set) is answered T_IFS later with
//!   the CONNECT_IND in the TXTOG buffer, by the engine (C3: `init_cind`);
//!   with column 9 bit0 set the engine then stops initiating and raises the
//!   event interrupt at the CONNECT_IND's end (C4: `init_evt_after_cind`,
//!   else at the window's end);
//! - software then programs the master: the anchor counter (columns 0 / 14
//!   started while column 2 still says 3) gives the first anchor, the
//!   value of column 0 plus one slots after the start (C5:
//!   `master_anchor_from_counter`, else the
//!   transmit window's start), and column 2 = 0x6000 turns the link into
//!   the master;
//! - at each anchor the engine transmits first (C6: `master_tx_first`) from
//!   the TXTOG buffer (bit0 = 0: software's PDU, else an empty PDU, the
//!   unacknowledged one again), hears the peripheral's reply T_IFS later,
//!   stores it in the RXTOG buffer, and sets TXBUFnCNTL bit0 = 1 and moves
//!   TXTOG on the acknowledgement; the event interrupt at each event's end;
//! - instants: column 5, applied at that event: the anchor moves by column
//!   4 & 0x7FFF slots (the master's 2 x WinOffset), the interval from
//!   columns 1 / 15.
//!
//! Nothing here ran against hardware.

use super::*;
use crate::ble_air::ConnectParams;

/// The CONNECT_IND the engine sent while initiating, until software turns
/// the link into the master (it survives the column 14 writes that set the
/// anchor counter).
#[derive(Clone, Debug)]
pub(super) struct MasterSetup {
    pub cind_end: u64,
    pub params: ConnectParams,
    /// When software started the anchor counter: the first anchor.
    pub anchor: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct MasterConn {
    pub counter: u16,
    pub anchor: u64,
    pub channel: u8,
    pub last_unmapped: u8,
    pub interval_slots: u64,
    pub nesn: u8,
    pub last_tx: Option<LastTx>,
    pub first: bool,
    pub continuing: bool,
    pub pairs: u32,
    pub received: bool,
    pub applied_instant: Option<u16>,
}

impl Engine {
    /// Column 14 started a link in state 3: a fresh initiation, or (after
    /// the CONNECT_IND) the anchor counter for the master.
    pub(super) fn start_initiating(&mut self, link: usize) {
        if let Some(setup) = self.pending_master[link].as_mut() {
            let slots = self.columns[0][link] as u64 + 1;
            let at = self.now + slots * SLOT;
            setup.anchor = Some(at);
            return;
        }
        self.start_scan(link);
        self.note(format!("link {link} initiating started"));
    }

    /// An ADV_IND / ADV_DIRECT_IND heard by an initiating link: the engine's
    /// CONNECT_IND when it comes from the target. Returns the CONNECT_IND
    /// (start tick, end tick, PDU) when one was sent.
    pub(super) fn initiator_hears(
        &mut self,
        link: usize,
        w: &mut Window,
        pdu: &[u8],
        end: u64,
        channel: u8,
    ) -> Res<Option<(u64, u64, Vec<u8>)>> {
        let kind = pdu[0] & 15;
        if (kind != adv::ADV_IND && kind != adv::ADV_DIRECT_IND)
            || self.pending_master[link].is_some()
            || pdu.len() < 8
        {
            return Ok(None);
        }
        let block = w
            .link(link)?
            .ok_or("an initiating link has no control block")?;
        let filter = block.get(w, field::FILTERCNTL)?;
        let target = block.address(w, field::WHITELIST0)?;
        let random = pdu[0] & 0x40 != 0;
        if filter & 0x11 != 0x11 || pdu[2..8] != target || random != (filter & 0x100 != 0) {
            return Ok(None);
        }
        if kind == adv::ADV_DIRECT_IND
            && (pdu.len() < 14 || pdu[8..14] != block.address(w, field::LOCALADR)?)
        {
            return Ok(None);
        }
        if block.address(w, field::TARGETADR)? != target {
            self.note(format!("link {link}: TARGETADR differs from WHITELIST0"));
        }
        if !self.config.init_cind {
            self.note(format!(
                "link {link}: the target's advertising heard; no CONNECT_IND (init_cind=0)"
            ));
            return Ok(None);
        }
        let b = (block.get(w, field::TXTOG)? & 1) as u32;
        let cind = Self::adv_buffer(block, w, b)?;
        if cind.len() != 36 || cind[0] & 15 != adv::CONNECT_IND || cind[8..14] != target {
            self.note(format!(
                "link {link}: TX buffer {b} holds no CONNECT_IND to the target: none sent"
            ));
            return Ok(None);
        }
        let Some(params) = ConnectParams::from_lldata(&cind[14..36]) else {
            return Ok(None);
        };
        let at = end + T_IFS;
        let cind_end = at + airtime_ticks(34);
        self.stats.connect_inds_sent += 1;
        self.pending_master[link] = Some(MasterSetup {
            cind_end,
            params,
            anchor: None,
        });
        self.log(AirRecord {
            tick: at,
            sender: Sender::Peripheral,
            channel,
            access_address: ADV_ACCESS_ADDRESS,
            event: None,
            pdu: cind.clone(),
            note: "the engine's CONNECT_IND".into(),
        });
        self.note(format!(
            "link {link}: CONNECT_IND on channel {channel}: interval {}, window {} at +{}, hop {}",
            params.interval, params.win_size, params.win_offset, params.hop
        ));
        if self.columns[9][link] & 1 != 0 {
            if self.config.init_evt_after_cind {
                self.links[link].scan = None;
                self.links[link].action = None;
                self.irq_due.push((cind_end, link, false));
            } else if let Some(s) = self.links[link].scan.as_mut() {
                s.start = u64::MAX; // no window after this one
            }
        }
        Ok(Some((at, cind_end, cind)))
    }

    /// Column 2 = state 6 on a link that sent a CONNECT_IND: the master.
    pub(super) fn begin_master(&mut self, link: usize) {
        if self.links[link].master.is_some() {
            return; // a latency rewrite of column 2 at an instant
        }
        let Some(setup) = self.pending_master[link].take() else {
            self.note(format!(
                "link {link}: state 6 without a CONNECT_IND is not modeled"
            ));
            return;
        };
        let window = setup.cind_end + 2 * SLOT + setup.params.win_offset as u64 * 2 * SLOT;
        let anchor = match (self.config.master_anchor_from_counter, setup.anchor) {
            (true, Some(a)) => a,
            (true, None) => {
                self.note(format!(
                    "link {link}: state 6 before the anchor counter was started: window start used"
                ));
                window
            }
            (false, _) => window,
        };
        self.note(format!(
            "link {link}: master: first anchor {:.0} us after the CONNECT_IND, {:.0} us after the \
             transmit window opens",
            (anchor as f64 - setup.cind_end as f64) / 24.0,
            (anchor as f64 - window as f64) / 24.0
        ));
        self.links[link].running = true;
        self.links[link].master = Some(MasterConn {
            counter: 0,
            anchor,
            channel: 0,
            last_unmapped: 0,
            interval_slots: 0,
            nesn: 0,
            last_tx: None,
            first: true,
            continuing: false,
            pairs: 0,
            received: false,
            applied_instant: None,
        });
        self.links[link].action = Some((anchor.max(self.now), Action::MasterTx));
    }

    /// The master's packet: a new event at its anchor, or the next pair of
    /// an event that continues (MD).
    pub(super) fn master_tx(&mut self, link: usize, block: Block, w: &mut Window) -> Res<()> {
        let mut m = self.links[link]
            .master
            .take()
            .ok_or("master event without a master link")?;
        if !m.continuing {
            if m.first && m.interval_slots == 0 {
                m.interval_slots = self.interval_slots(link);
            }
            if !self.config.instant_gating {
                m.interval_slots = self.interval_slots(link);
            }
            let instant = self.columns[5][link] as u16;
            if self.config.instant_gating
                && !m.first
                && instant != 0
                && m.counter == instant
                && m.applied_instant != Some(instant)
            {
                let offset = (self.columns[4][link] & 0x7FFF) as u64;
                m.anchor += offset * SLOT;
                let old = m.interval_slots;
                m.interval_slots = self.interval_slots(link);
                m.applied_instant = Some(instant);
                self.note(format!(
                    "link {link}: master instant {instant}: interval {old} -> {} slots, offset {offset} slots",
                    m.interval_slots
                ));
            }
            if m.interval_slots == 0 {
                return Err("master connection interval (columns 1/15) is 0");
            }
            let (unmapped, channel) = self.next_channel(link, block, w, m.last_unmapped)?;
            m.last_unmapped = unmapped;
            m.channel = channel;
            m.pairs = 0;
            m.received = false;
            self.columns[3][link] = (m.counter as u32 + 1) & 0xFFFF;
            self.stats.master_events += 1;
        }
        m.continuing = false;
        if !self.config.master_tx_first {
            let close = self.now + 2 * SLOT;
            self.links[link].master = Some(m);
            self.links[link].action = Some((close, Action::MasterEnd));
            return Ok(());
        }
        let pdu = match m
            .last_tx
            .as_ref()
            .filter(|_| self.config.engine_retransmits)
        {
            Some(last) => {
                self.stats.master_tx_retransmit += 1;
                let mut pdu = last.pdu.clone();
                pdu[0] = (pdu[0] & !4) | m.nesn << 2;
                pdu
            }
            None => {
                let b = block.get(w, field::TXTOG)? & 1;
                let hdr = block.get(w, field::TXDHDR0 + 2 * b as u32)?;
                let sn = ((hdr >> 2) & 1) as u8;
                let loaded = block.get8(w, field::TXBUF0CNTL + b as u32)? & 1 == 0;
                let pdu = if loaded {
                    let len = hdr >> 8;
                    let pointer = block.get(w, field::TXPTR0 + 2 * b as u32)? as u32;
                    let md = ((hdr >> 3) & 1) as u8;
                    let mut pdu =
                        vec![(hdr & 3) as u8 | m.nesn << 2 | sn << 3 | md << 4, len as u8];
                    pdu.extend(w.bytes(pointer, len as u32)?);
                    self.stats.master_tx_data += 1;
                    pdu
                } else {
                    self.stats.master_tx_empty += 1;
                    vec![1 | m.nesn << 2 | sn << 3, 0]
                };
                m.last_tx = Some(LastTx {
                    buffer: b as u8,
                    sn,
                    data: loaded,
                    pdu: pdu.clone(),
                });
                pdu
            }
        };
        let now = self.now;
        let end = now + airtime_ticks(pdu.len() - 2);
        let aa = block.access_address(w)?;
        let crc = block.crc_init(w)?;
        let reply = self
            .peripherals
            .iter_mut()
            .find_map(|p| p.on_master_pdu(now, m.channel, aa, crc, &pdu));
        self.log(AirRecord {
            tick: now,
            sender: Sender::Master,
            channel: m.channel,
            access_address: aa,
            event: Some(m.counter),
            pdu: pdu.clone(),
            note: if reply.is_some() {
                String::new()
            } else {
                "no peripheral answered".into()
            },
        });
        m.pairs += 1;
        let Some(reply) = reply else {
            let close = end + T_IFS + 30 * TICKS_PER_US;
            self.links[link].master = Some(m);
            self.links[link].action = Some((close, Action::MasterEnd));
            return Ok(());
        };
        let reply_at = end + T_IFS;
        let reply_end = reply_at + airtime_ticks(reply.len() - 2);
        let note = self.master_receive(link, block, w, &mut m, &reply, reply_end)?;
        self.log(AirRecord {
            tick: reply_at,
            sender: Sender::Slave,
            channel: m.channel,
            access_address: aa,
            event: Some(m.counter),
            pdu: reply.clone(),
            note: note.into(),
        });
        let our_md = m.last_tx.as_ref().is_some_and(|t| t.pdu[0] & 0x10 != 0) || pdu[0] & 0x10 != 0;
        let peer_md = reply[0] & 0x10 != 0;
        let more = (our_md || peer_md) && m.pairs < self.config.max_pairs_per_event;
        m.continuing = more;
        self.links[link].master = Some(m);
        self.links[link].action = Some(if more {
            (reply_end + T_IFS, Action::MasterTx)
        } else {
            (reply_end, Action::MasterEnd)
        });
        Ok(())
    }

    /// The peripheral's reply: the acknowledgement of our PDU, then the
    /// connection RX path (§8, the same code for states 6 and 7 [IR]).
    fn master_receive(
        &mut self,
        link: usize,
        block: Block,
        w: &mut Window,
        m: &mut MasterConn,
        reply: &[u8],
        end: u64,
    ) -> Res<&'static str> {
        let h0 = reply[0];
        let (peer_nesn, peer_sn) = ((h0 >> 2) & 1, (h0 >> 3) & 1);
        let mut acked = false;
        if let Some(last) = &m.last_tx {
            if peer_nesn != last.sn {
                acked = true;
                if last.data {
                    let cntl = block.get8(w, field::TXBUF0CNTL + last.buffer as u32)?;
                    block.set8(w, field::TXBUF0CNTL + last.buffer as u32, cntl | 1)?;
                }
                let tog = block.get(w, field::TXTOG)?;
                block.set(w, field::TXTOG, (tog & !1) | (last.buffer ^ 1) as u16)?;
                m.last_tx = None;
            }
        }
        m.received = true;
        self.stats.master_rx += 1;
        let repeated = peer_sn != m.nesn;
        if repeated && self.config.rx_count_repeats {
            block.set(w, field::EVTCOUNT, m.counter)?;
        }
        let rx = Received {
            pdu: reply,
            counter: m.counter,
            channel: m.channel,
            acked,
            end,
        };
        Ok(if repeated && self.config.old_sn == OldSn::Drop {
            "old SN: retransmission ignored"
        } else if repeated && self.config.old_sn == OldSn::DropWithIrq {
            self.irq_due.push((end, link, true));
            "old SN: discarded, RX IRQ"
        } else if repeated {
            self.store_conn_rx(link, block, w, rx)?;
            "old SN: stored anyway (old_sn=store), RX IRQ"
        } else if self.store_conn_rx(link, block, w, rx)? {
            m.nesn ^= 1;
            self.stats.rx_stored += 1;
            "stored, RX IRQ"
        } else {
            self.stats.rx_not_acknowledged += 1;
            "no armed RX buffer: not acknowledged"
        })
    }

    /// A master event ends: the event interrupt, the next anchor.
    pub(super) fn master_end(&mut self, link: usize) {
        let received = self.links[link].master.as_ref().is_some_and(|m| m.received);
        if received || self.config.event_irq_every_event {
            self.raise(link, false);
        }
        let Some(m) = self.links[link].master.as_mut() else {
            return;
        };
        m.first = false;
        m.continuing = false;
        m.counter = m.counter.wrapping_add(1);
        m.anchor += m.interval_slots * SLOT;
        let at = m.anchor.max(self.now);
        self.links[link].action = Some((at, Action::MasterTx));
    }

    pub(super) fn next_peripheral_tick(&self) -> Option<u64> {
        self.peripherals.iter().filter_map(|p| p.next_tick()).min()
    }

    /// A virtual peripheral's own time came: maybe an advertising PDU.
    pub(super) fn peripheral_step(&mut self, w: &mut Window) -> Res<()> {
        let now = self.now;
        let Some(i) = self
            .peripherals
            .iter()
            .position(|p| p.next_tick().is_some_and(|t| t <= now))
        else {
            return Ok(());
        };
        let Some(out) = self.peripherals[i].tick(now) else {
            return Ok(());
        };
        self.air_adv(
            w,
            AirSource::Peripheral(i),
            out.channel,
            out.pdu,
            Some(out.scan_rsp),
            out.rssi,
        )
    }
}
