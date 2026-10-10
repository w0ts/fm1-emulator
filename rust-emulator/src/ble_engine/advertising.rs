// SPDX-License-Identifier: GPL-3.0-only
//! Advertising events (fact sheet §6) and the advertiser's answers to a
//! SCAN_REQ or a CONNECT_IND (§6, §7, U6).

use super::*;

/// One advertising PDU being answered.
#[derive(Clone, Copy)]
struct Exchange {
    link: usize,
    block: Block,
    channel: u8,
    /// When the central's reply ends.
    reply_end: u64,
    /// The TX buffer holding the advertising PDU.
    adv_buffer: u32,
}

/// What the advertiser does with the central's reply to one ADV PDU.
enum Reply {
    Ignored(&'static str),
    Stored,
    ScanResponse(Vec<u8>, &'static str),
    Connect,
}

impl Engine {
    pub(super) fn interval_slots(&self, link: usize) -> u64 {
        // Column 1 low, column 15 [14:0] high (§2.3 [M:t]).
        (self.columns[1][link] | (self.columns[15][link] & 0x7FFF) << 16) as u64
    }

    /// The PDU in TX buffer `b` as an advertising PDU: header byte 0 from
    /// TXAHDRn ([3:0] type, bit4 TxAdd, bit5 RxAdd, moved to the Core's
    /// bits 6/7; §3 [M:s]/[I]), length from TXDHDRn [15:8], payload at
    /// TXPTRn (§6 step 8 [M:d]).
    pub(super) fn adv_buffer(block: Block, w: &Window, b: u32) -> Res<Vec<u8>> {
        let ahdr = block.get(w, field::TXAHDR0 + 2 * b)?;
        let len = block.get(w, field::TXDHDR0 + 2 * b)? >> 8;
        let pointer = block.get(w, field::TXPTR0 + 2 * b)?;
        let header = (ahdr & 15) as u8 | (((ahdr >> 4) & 3) as u8) << 6;
        let mut pdu = vec![header, len as u8];
        pdu.extend(w.bytes(pointer as u32, len as u32)?);
        Ok(pdu)
    }

    pub(super) fn adv_pdu(
        &mut self,
        link: usize,
        block: Block,
        index: u8,
        w: &mut Window,
    ) -> Res<()> {
        // ADVIDX [15:14] PDUs per event, [13:0] their spacing in µs (§3,
        // §6 step 4 [M:d]); column 6 [5:0] first channel, [12:8] the step
        // between channels (§6 step 6 [M:s]/[I]). Bits 6/7 (U15) unused.
        let advidx = block.get(w, field::ADVIDX)?;
        let count = (advidx >> 14).max(1) as u8;
        let spacing = (advidx & 0x3FFF) as u64 * TICKS_PER_US;
        let col6 = self.columns[6][link];
        let channel = (col6 & 63) as u8 + index * ((col6 >> 8) & 31) as u8;
        if !(37..=39).contains(&channel) {
            return Err("advertising channel outside 37-39 (column 6)");
        }
        if index == 0 {
            self.stats.adv_events += 1;
        }
        self.stats.adv_pdus += 1;
        // TXTOG bit0 picks the advertising buffer; the other one holds the
        // SCAN_RSP (§8 IRQ 45 step 2, "TXTOG decides which is 0" [M:s]/[I]).
        let b = (block.get(w, field::TXTOG)? & 1) as u32;
        let pdu = Self::adv_buffer(block, w, b)?;
        let start = self.now;
        let end = start + airtime_ticks(pdu.len() - 2);
        let reply_at = end + T_IFS;
        let reply = self
            .central
            .as_mut()
            .and_then(|c| c.on_advertising(start, channel, &pdu, reply_at));
        self.log(AirRecord {
            tick: start,
            sender: Sender::Peripheral,
            channel,
            access_address: ADV_ACCESS_ADDRESS,
            event: None,
            pdu,
            note: String::new(),
        });
        let mut busy_until = end;
        let mut connected = false;
        if let Some(reply) = reply {
            let reply_end = reply_at + airtime_ticks(reply.len().saturating_sub(2));
            busy_until = reply_end;
            let x = Exchange {
                link,
                block,
                channel,
                reply_end,
                adv_buffer: b,
            };
            let outcome = self.adv_reply(x, w, &reply)?;
            let note = match &outcome {
                Reply::Ignored(why) => *why,
                Reply::Stored => "stored in an RX buffer, RX IRQ",
                Reply::ScanResponse(_, why) => why,
                Reply::Connect => "CONNECT_IND stored, RX IRQ",
            };
            self.log(AirRecord {
                tick: reply_at,
                sender: Sender::Central,
                channel,
                access_address: ADV_ACCESS_ADDRESS,
                event: None,
                pdu: reply,
                note: note.into(),
            });
            match outcome {
                Reply::ScanResponse(rsp, _) => {
                    let rsp_at = reply_end + T_IFS;
                    busy_until = rsp_at + airtime_ticks(rsp.len() - 2);
                    if let Some(c) = self.central.as_mut() {
                        c.on_scan_response(rsp_at, channel, &rsp);
                    }
                    self.stats.scan_responses += 1;
                    self.log(AirRecord {
                        tick: rsp_at,
                        sender: Sender::Peripheral,
                        channel,
                        access_address: ADV_ACCESS_ADDRESS,
                        event: None,
                        pdu: rsp,
                        note: String::new(),
                    });
                }
                Reply::Connect => connected = true,
                Reply::Ignored(_) | Reply::Stored => {}
            }
        }
        let stop = connected && self.config.stop_adv_on_connect_ind;
        let l = &mut self.links[link];
        let next = index + 1;
        l.action = if next < count && !stop {
            let at = (l.adv_start + next as u64 * spacing).max(busy_until);
            Some((at, Action::AdvPdu(next)))
        } else {
            Some((busy_until, Action::AdvEnd))
        };
        Ok(())
    }

    fn adv_reply(&mut self, x: Exchange, w: &mut Window, reply: &[u8]) -> Res<Reply> {
        let Exchange {
            link,
            block,
            channel,
            reply_end,
            ..
        } = x;
        if reply.len() < 2 || reply.len() - 2 != reply[1] as usize {
            return Ok(Reply::Ignored("malformed PDU"));
        }
        let kind = reply[0] & 15;
        let payload = &reply[2..];
        let expected = match kind {
            adv::SCAN_REQ => 12,
            adv::CONNECT_IND => 34,
            _ => return Ok(Reply::Ignored("not a reply to an advertiser")),
        };
        if payload.len() != expected {
            return Ok(Reply::Ignored("wrong length for its type"));
        }
        // OPTCNTL bit4 = local-address match disable, cleared by the
        // advertising set-up (§3, §6 step 9 [M:s]); LOCALADR [M:d].
        let optcntl = block.get(w, field::OPTCNTL)?;
        if optcntl & 0x10 == 0 && payload[6..12] != block.address(w, field::LOCALADR)? {
            return Ok(Reply::Ignored("AdvA is not LOCALADR"));
        }
        // FILTERCNTL bit3 scan-request filter, bit4 connect-request filter
        // against WHITELIST0 (§3 [M:s]/[I]; both oracles use policy 0).
        let filter = block.get(w, field::FILTERCNTL)?;
        let filtered = if kind == adv::SCAN_REQ {
            filter & 8
        } else {
            filter & 16
        } != 0;
        if filtered && payload[0..6] != block.address(w, field::WHITELIST0)? {
            return Ok(Reply::Ignored("filtered by FILTERCNTL"));
        }
        if kind == adv::SCAN_REQ {
            return self.scan_request(x, w, reply);
        }
        if !self.store_adv_rx(x, w, reply)? {
            return Ok(Reply::Ignored("no armed RX buffer: CONNECT_IND lost"));
        }
        self.stats.connect_inds += 1;
        self.links[link].connect_end = Some(reply_end);
        self.note(format!(
            "link {link}: CONNECT_IND on channel {channel}, ends at {:.6}",
            reply_end as f64 / 24e6
        ));
        Ok(Reply::Connect)
    }

    fn scan_request(&mut self, x: Exchange, w: &mut Window, reply: &[u8]) -> Res<Reply> {
        let Exchange {
            link,
            block,
            adv_buffer,
            ..
        } = x;
        let to_software =
            self.config.scan_req_rx_irq || self.config.scan_response == ScanResponse::Software;
        let stored = to_software && self.store_adv_rx(x, w, reply)?;
        // FORMAT bit8 = ignore SCAN_REQ (§3 [M:s]/[I]).
        let ignore = block.get(w, field::FORMAT)? & 0x100 != 0;
        if self.config.scan_response == ScanResponse::Automatic && !ignore {
            let rsp = Self::adv_buffer(block, w, adv_buffer ^ 1)?;
            if rsp[0] & 15 != adv::SCAN_RSP {
                self.note(format!(
                    "link {link}: TX buffer {} holds PDU type {}, not SCAN_RSP",
                    adv_buffer ^ 1,
                    rsp[0] & 15
                ));
            }
            let why = if stored {
                "answered by the engine; also stored, RX IRQ"
            } else {
                "answered by the engine"
            };
            return Ok(Reply::ScanResponse(rsp, why));
        }
        Ok(if stored {
            Reply::Stored
        } else if ignore {
            Reply::Ignored("FORMAT bit8: SCAN_REQ ignored")
        } else {
            Reply::Ignored("no SCAN_RSP (software mode, no RX buffer)")
        })
    }

    /// Store an advertising-channel PDU in the RX buffer RXTOG selects, if
    /// software armed it (RXBUFnCNTL bit0 = 0, §3/§6 step 7 [M:d]/[I]), and
    /// raise the RX interrupt when the packet ends. Header byte 0 goes to
    /// RXAHDRn in the Core layout (§3 [M:s]); the length to RXDHDRn
    /// [15:8], like TXDHDR for advertising [I].
    fn store_adv_rx(&mut self, x: Exchange, w: &mut Window, pdu: &[u8]) -> Res<bool> {
        let Exchange {
            link,
            block,
            channel,
            reply_end: end,
            ..
        } = x;
        let b = (block.get(w, field::RXTOG)? & 1) as u32;
        let len = pdu[1] as u16;
        if (self.config.rx_needs_armed && block.get8(w, field::RXBUF0CNTL + b)? & 1 != 0)
            || len > block.get(w, field::RXMAXBUF)?
        {
            return Ok(false);
        }
        let pointer = block.get(w, field::RXPTR0 + 2 * b)? as u32;
        w.set_bytes(pointer, &pdu[2..])?;
        block.set(w, field::RXAHDR0 + 2 * b, pdu[0] as u16)?;
        block.set(w, field::RXDHDR0 + 2 * b, len << 8)?;
        block.set(w, field::RXSTAT0 + 2 * b, 1)?;
        block.set(w, field::LASTCHMAP, channel as u16)?;
        let cntl = block.get8(w, field::RXBUF0CNTL + b)?;
        block.set8(w, field::RXBUF0CNTL + b, cntl | 1)?;
        let tog = block.get(w, field::RXTOG)?;
        block.set(w, field::RXTOG, tog ^ 1)?;
        self.irq_due.push((end, link, true));
        Ok(true)
    }

    /// The end of an advertising event: the event interrupt (§8 IRQ 45
    /// step 2 services advertising [M:s]), then the next event after the
    /// interval plus advDelay when column 8 bit15 is set (§2.3, §6 step 5
    /// [M:t]; 0–10 ms per the Core specification).
    pub(super) fn adv_end(&mut self, link: usize) {
        let connecting = self.links[link].connect_end.is_some();
        if connecting && self.config.stop_adv_on_connect_ind {
            if self.config.adv_event_irq_on_connect {
                self.raise(link, false);
            }
            return;
        }
        self.raise(link, false);
        let delay = if self.columns[8][link] & 0x8000 != 0 {
            self.random = self
                .random
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223 ^ self.config.seed);
            (self.random >> 8) as u64 % 10_001 * TICKS_PER_US
        } else {
            0
        };
        let next = (self.links[link].adv_start + self.interval_slots(link).max(1) * SLOT + delay)
            .max(self.now);
        let l = &mut self.links[link];
        l.adv_start = next;
        l.action = Some((next, Action::AdvPdu(0)));
    }
}
