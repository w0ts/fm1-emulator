// SPDX-License-Identifier: GPL-3.0-only
//! The peripheral connection (fact sheet §7–§9): anchors on the engine's
//! time base, CSA #1 hopping from the control block's channel tables,
//! ping-pong RX/TX buffers with SN/NESN, empty PDUs, the event counter,
//! the RX (29) and event (45) interrupts, and instant-gated updates.
//! Supervision is left to software (§7 step 15 [M:s]): the engine keeps
//! scheduling events until column 14 stops the link.

use super::*;

/// A packet from the central, as the RX side stores it.
pub(super) struct Received<'a> {
    pub pdu: &'a [u8],
    pub counter: u16,
    pub channel: u8,
    /// It acknowledged our last PDU (RXSTAT bit8).
    pub acked: bool,
    pub end: u64,
}

impl Engine {
    /// Column 2 = state 7 after a CONNECT_IND (§7 step 9 [M:s]). The
    /// transmit window starts 1.25 ms + transmitWindowOffset after the end
    /// of the CONNECT_IND (Core spec); column 4 holds the offset as
    /// `0x8000 | (2 × WinOffset − 1)` slots (§7 step 8 [M:s]). The anchor
    /// counter is not rewritten for a peripheral (§7, [I]): the engine keeps
    /// the CONNECT_IND timing.
    pub(super) fn begin_connection(&mut self, link: usize) {
        if self.links[link].conn.is_some() {
            return; // a latency rewrite of column 2 during the connection
        }
        let Some(end) = self.links[link].connect_end else {
            self.note(format!(
                "link {link}: state 7 without a CONNECT_IND is not modeled"
            ));
            return;
        };
        let start = end + 2 * SLOT + Self::offset_slots(self.columns[4][link]) * SLOT;
        self.note(format!(
            "link {link}: state 7 programmed {:.0} us after the CONNECT_IND, {:.0} us before \
             the transmit window",
            (self.now as f64 - end as f64) / 24.0,
            (start as f64 - self.now as f64) / 24.0
        ));
        self.links[link].conn = Some(Conn {
            counter: 0,
            anchor: start,
            nominal: start,
            open: start,
            close: start,
            channel: 0,
            last_unmapped: 0,
            interval_slots: 0,
            nesn: 0,
            last_tx: None,
            first: true,
            received: None,
            awaiting: false,
            pairs: 0,
            peer_md: false,
            applied_instant: None,
        });
        // The rest of §7 (interval, window, channel tables) is written after
        // column 2, so the first event is prepared at the window start.
        self.links[link].action = Some((start.max(self.now), Action::ConnOpen));
    }

    fn offset_slots(column4: u32) -> u64 {
        if column4 & 0x8000 != 0 {
            (column4 & 0x7FFF) as u64 + 1
        } else {
            0
        }
    }

    /// The data channel of the next event: CSA #1 with the hop increment of
    /// column 6 [12:8] (§2.3 [M:s]), the used-channel bits of CHMAP0/1/2 and
    /// FRQ_IDX1 as the remapping table, CHMAP2 [15:5] used channels (§3
    /// [M:d]; that the engine reads them is [I]).
    pub(super) fn next_channel(
        &self,
        link: usize,
        block: Block,
        w: &Window,
        last: u8,
    ) -> Res<(u8, u8)> {
        let hop = ((self.columns[6][link] >> 8) & 31) as u8;
        let (map, used) = block.channel_map(w)?;
        if used == 0 || used > 37 {
            return Err("CHMAP2 used-channel count outside 1-37");
        }
        let unmapped = (last + hop) % 37;
        if map >> unmapped & 1 != 0 {
            return Ok((unmapped, unmapped));
        }
        let index = (unmapped % used) as u32;
        Ok((unmapped, block.get8(w, field::FRQ_IDX1 + index)?))
    }

    /// Prepare the next connection event (ConnOpen).
    pub(super) fn conn_open(&mut self, link: usize, block: Block, w: &mut Window) -> Res<()> {
        let mut conn = self.links[link]
            .conn
            .take()
            .ok_or("connection event without a connection")?;
        let mut anchor = conn.nominal;
        if conn.first && conn.interval_slots == 0 {
            conn.interval_slots = self.interval_slots(link);
        }
        // §9 [M:s]: column 5 = instant; the engine switching at it is [I].
        // At the instant the transmit window starts the old interval after
        // the previous anchor plus the column 4 offset; the new interval
        // (columns 1/15) applies from there (Core spec).
        if !self.config.instant_gating {
            conn.interval_slots = self.interval_slots(link);
        }
        let instant = self.columns[5][link] as u16;
        if self.config.instant_gating
            && !conn.first
            && instant != 0
            && conn.counter == instant
            && conn.applied_instant != Some(instant)
        {
            let offset = Self::offset_slots(self.columns[4][link]);
            anchor += offset * SLOT;
            let old = conn.interval_slots;
            conn.interval_slots = self.interval_slots(link);
            conn.applied_instant = Some(instant);
            self.note(format!(
                "link {link}: instant {instant}: interval {old} -> {} slots, offset {offset} slots",
                conn.interval_slots
            ));
        }
        if conn.interval_slots == 0 {
            return Err("connection interval (columns 1/15) is 0");
        }
        let widen = block.widening_us(w)? as u64 * TICKS_PER_US;
        let window = block.window_us(w)? as u64 * TICKS_PER_US;
        // A late set-up joins at the first event whose window is still open.
        let mut missed = 0;
        loop {
            let (unmapped, channel) = self.next_channel(link, block, w, conn.last_unmapped)?;
            conn.last_unmapped = unmapped;
            conn.channel = channel;
            if anchor + window + widen >= self.now {
                break;
            }
            missed += 1;
            conn.counter = conn.counter.wrapping_add(1);
            anchor += conn.interval_slots * SLOT;
        }
        if missed > 0 {
            self.note(format!(
                "link {link}: joined {missed} events late, at event {}",
                conn.counter
            ));
        }
        conn.anchor = anchor;
        conn.open = anchor.saturating_sub(widen);
        conn.close = anchor + window + widen;
        conn.received = None;
        conn.awaiting = true;
        conn.pairs = 0;
        // Column 3: software reads it and subtracts 1 (§2.3 [M:s]).
        self.columns[3][link] = (conn.counter as u32 + 1) & 0xFFFF;
        self.links[link].action = Some((conn.close, Action::ConnClose));
        self.links[link].conn = Some(conn);
        Ok(())
    }

    /// The central transmits; a link listening on that channel at that time
    /// receives it.
    pub(super) fn central_transmits(&mut self, w: &mut Window) -> Res<()> {
        let now = self.now;
        let Some(tx) = self.central.as_mut().and_then(|c| c.transmit(now)) else {
            return Ok(());
        };
        let end = now + airtime_ticks(tx.pdu.len().saturating_sub(2));
        self.expect = Some(end + T_IFS + TICKS_PER_US);
        let mut note = "no link listening";
        for link in 0..LINKS {
            let Some(conn) = &self.links[link].conn else {
                continue;
            };
            if !conn.awaiting || now < conn.open || now > conn.close {
                continue;
            }
            if conn.channel != tx.channel {
                note = "engine listens on another channel";
                self.note(format!(
                    "link {link}: event {}: engine on channel {}, central on {}",
                    conn.counter, conn.channel, tx.channel
                ));
                continue;
            }
            let block = w.link(link)?.ok_or("connected link has no control block")?;
            note = self.conn_receive(link, block, w, &tx, end)?;
            break;
        }
        self.log(AirRecord {
            tick: now,
            sender: Sender::Central,
            channel: tx.channel,
            access_address: tx.access_address,
            event: Some(tx.event),
            pdu: tx.pdu,
            note: note.into(),
        });
        Ok(())
    }

    /// Receive a central PDU (§8 IRQ 29 [M:s] read side, mirrored as the
    /// engine's write side [I]).
    pub(super) fn conn_receive(
        &mut self,
        link: usize,
        block: Block,
        w: &mut Window,
        tx: &CentralTx,
        end: u64,
    ) -> Res<&'static str> {
        // BDADDR0/1 = access address, CRCWORD0/1 = CRC init (§7 step 2).
        if block.access_address(w)? != tx.access_address {
            return Ok("access address differs from BDADDR0/1");
        }
        if block.crc_init(w)? != tx.crc_init {
            return Ok("CRC init differs from CRCWORD0/1: CRC error");
        }
        let mut conn = self.links[link]
            .conn
            .take()
            .ok_or("receive without a connection")?;
        conn.received.get_or_insert(self.now);
        conn.awaiting = false;
        conn.peer_md = tx.pdu[0] & 0x10 != 0;
        let h0 = tx.pdu[0];
        let (peer_nesn, peer_sn) = ((h0 >> 2) & 1, (h0 >> 3) & 1);
        // Acknowledgement of our last PDU: the central's NESN moved past our
        // SN. A consumed data buffer is marked empty (TXBUFnCNTL bit0 = 1)
        // and TXTOG bit0 moves to the other buffer, whose SN is the other
        // value (§7 step 17: the buffers start with opposite SN; §3 TXTOG
        // and TXBUFnCNTL [I]).
        let mut acked = false;
        if let Some(last) = &conn.last_tx {
            if peer_nesn != last.sn {
                acked = true;
                if last.data {
                    let cntl = block.get8(w, field::TXBUF0CNTL + last.buffer as u32)?;
                    block.set8(w, field::TXBUF0CNTL + last.buffer as u32, cntl | 1)?;
                }
                let tog = block.get(w, field::TXTOG)?;
                block.set(w, field::TXTOG, (tog & !1) | (last.buffer ^ 1) as u16)?;
                conn.last_tx = None;
            }
        }
        let empty = h0 & 3 == 1 && tx.pdu[1] == 0;
        let repeated = peer_sn != conn.nesn;
        // EVTCOUNT is "the event counter of the last RX" (§3 [M:s]); with
        // rx_count_repeats a discarded repeat still counts as a reception.
        if repeated && self.config.rx_count_repeats {
            block.set(w, field::EVTCOUNT, conn.counter)?;
        }
        let note = if repeated && self.config.old_sn == OldSn::Drop {
            "old SN: retransmission ignored"
        } else if repeated && self.config.old_sn == OldSn::DropWithIrq {
            self.irq_due.push((end, link, true));
            "old SN: discarded, RX IRQ"
        } else if repeated {
            let stored = self.store_conn_rx(
                link,
                block,
                w,
                Received {
                    pdu: &tx.pdu,
                    counter: conn.counter,
                    channel: conn.channel,
                    acked,
                    end,
                },
            )?;
            if stored {
                "old SN: stored anyway (old_sn=store), RX IRQ"
            } else {
                "old SN, no armed RX buffer"
            }
        } else if empty && !self.config.store_empty_pdus {
            conn.nesn ^= 1;
            "empty PDU acknowledged"
        } else if self.store_conn_rx(
            link,
            block,
            w,
            Received {
                pdu: &tx.pdu,
                counter: conn.counter,
                channel: conn.channel,
                acked,
                end,
            },
        )? {
            conn.nesn ^= 1;
            self.stats.rx_stored += 1;
            "stored, RX IRQ"
        } else {
            self.stats.rx_not_acknowledged += 1;
            "no armed RX buffer: not acknowledged"
        };
        self.links[link].conn = Some(conn);
        self.links[link].action = Some((end + T_IFS, Action::ConnTx));
        Ok(note)
    }

    /// Store a data-channel PDU in the RX buffer RXTOG selects, if armed;
    /// RXDHDRn takes the Core header ([1:0] LLID, bit2 NESN, bit3 SN, bit4
    /// MD, [15:8] length), RXSTATn = 1 | ack << 8, EVTCOUNT the counter,
    /// LASTCHMAP the channel (§3, §8 IRQ 29 step 4 [M:s]).
    pub(super) fn store_conn_rx(
        &mut self,
        link: usize,
        block: Block,
        w: &mut Window,
        rx: Received,
    ) -> Res<bool> {
        let Received {
            pdu,
            counter,
            channel,
            acked,
            end,
        } = rx;
        let b = (block.get(w, field::RXTOG)? & 1) as u32;
        let len = pdu[1] as u16;
        if (self.config.rx_needs_armed && block.get8(w, field::RXBUF0CNTL + b)? & 1 != 0)
            || len > block.get(w, field::RXMAXBUF)?
        {
            return Ok(false);
        }
        let pointer = block.get(w, field::RXPTR0 + 2 * b)? as u32;
        w.set_bytes(pointer, &pdu[2..])?;
        block.set(w, field::RXDHDR0 + 2 * b, (pdu[0] & 0x1F) as u16 | len << 8)?;
        block.set(w, field::RXSTAT0 + 2 * b, 1 | (acked as u16) << 8)?;
        block.set(w, field::EVTCOUNT, counter)?;
        block.set(w, field::LASTCHMAP, channel as u16)?;
        let cntl = block.get8(w, field::RXBUF0CNTL + b)?;
        block.set8(w, field::RXBUF0CNTL + b, cntl | 1)?;
        let tog = block.get(w, field::RXTOG)?;
        block.set(w, field::RXTOG, tog ^ 1)?;
        self.irq_due.push((end, link, true));
        Ok(true)
    }

    /// The peripheral's reply, T_IFS after the central's packet: the
    /// unacknowledged PDU again, else TX buffer TXTOG bit0 if software
    /// loaded it (TXBUFnCNTL bit0 = 0), else an empty PDU (§8 IRQ 29 step 5
    /// [I]). SN is the buffer's TXDHDRn bit2 (§3, §7 step 17 [M:s]).
    pub(super) fn conn_tx(&mut self, link: usize, block: Block, w: &mut Window) -> Res<()> {
        let mut conn = self.links[link]
            .conn
            .take()
            .ok_or("reply without a connection")?;
        let resend = conn
            .last_tx
            .as_ref()
            .filter(|_| self.config.engine_retransmits);
        let pdu = match resend {
            Some(last) => {
                self.stats.tx_retransmit += 1;
                let mut pdu = last.pdu.clone();
                pdu[0] = (pdu[0] & !4) | conn.nesn << 2;
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
                    let mut pdu = vec![
                        (hdr & 3) as u8 | conn.nesn << 2 | sn << 3 | md << 4,
                        len as u8,
                    ];
                    pdu.extend(w.bytes(pointer, len as u32)?);
                    self.stats.tx_data += 1;
                    pdu
                } else {
                    self.stats.tx_empty += 1;
                    vec![1 | conn.nesn << 2 | sn << 3, 0]
                };
                conn.last_tx = Some(LastTx {
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
        self.expect = None;
        if let Some(c) = self.central.as_mut() {
            c.on_peripheral_pdu(now, &pdu);
        }
        let note = if pdu[0] & 3 == 0 {
            "LLID 0 (reserved)"
        } else {
            ""
        };
        self.log(AirRecord {
            tick: now,
            sender: Sender::Peripheral,
            channel: conn.channel,
            access_address: block.access_address(w)?,
            event: Some(conn.counter),
            pdu,
            note: note.into(),
        });
        // Q5: while either side sets MD the event stays open for another
        // pair, up to max_pairs_per_event (Core Vol 6 Part B 4.5.6).
        conn.pairs += 1;
        let our_md = self.links_last_md(&conn);
        let more = (conn.peer_md || our_md) && conn.pairs < self.config.max_pairs_per_event;
        let close = if more {
            conn.awaiting = true;
            conn.close = end + T_IFS + 2 * TICKS_PER_US;
            conn.close
        } else {
            end
        };
        self.links[link].conn = Some(conn);
        self.links[link].action = Some((close, Action::ConnClose));
        Ok(())
    }

    fn links_last_md(&self, conn: &Conn) -> bool {
        conn.last_tx.as_ref().is_some_and(|t| t.pdu[0] & 0x10 != 0)
    }

    /// The end of a connection event: the event interrupt, the counter, and
    /// the next anchor (re-synchronised to the central's packet).
    pub(super) fn conn_end(&mut self, link: usize) {
        let received = self.links[link]
            .conn
            .as_ref()
            .is_some_and(|c| c.received.is_some());
        if received || self.config.event_irq_every_event {
            self.raise(link, false);
        }
        let Some(conn) = self.links[link].conn.as_mut() else {
            return;
        };
        self.stats.conn_events += 1;
        if let Some(rx) = conn.received.take() {
            self.stats.conn_events_received += 1;
            conn.anchor = rx;
            conn.first = false;
        }
        conn.counter = conn.counter.wrapping_add(1);
        conn.nominal = conn.anchor + conn.interval_slots * SLOT;
        let at = conn.nominal.max(self.now);
        self.links[link].action = Some((at, Action::ConnOpen));
    }
}
