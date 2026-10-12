// SPDX-License-Identifier: GPL-3.0-only
// UART1: WL82.h JL_UART_TypeDef and the vendor uart.c/spec_uart.c.
// TX supports 8N1 byte/DMA transfers. Receive: a host can put MIDI bytes on
// the RX line (`receive`, the FM-1's DIN/TRS MIDI IN, hal/fm1_uart.h); they
// arrive back to back at 31250 baud (10 bits = 7680 oscillator ticks each,
// whatever BAUD says: the sender's rate) and the RX DMA writes each at the
// end of its stop bit into the ring RXSADR..RXEADR (wrapping), counting it;
// CON0 RDC (bit 7) latches the count into HRXCNT and restarts it, bit 14
// is RX pending (bit 12 clears it). RX needs UTEN (bit 0) and RXDMA (bit
// 6); bytes arriving otherwise are lost. Unmeasured on hardware: no RX
// interrupt or OT timeout is generated. Without host bytes nothing changes.
#[derive(Default)]
pub(crate) struct Uart {
    registers: [u32; 11],
    transmit: Vec<u8>,
    remaining: u64,
    pending: bool,
    // Bounded observation of bytes that have actually finished transmitting.
    transmitted: Vec<u8>,
    /// Host bytes not yet received, oldest first.
    rx_queue: std::collections::VecDeque<u8>,
    /// Ticks until the byte on the line has been received (0: line idle).
    rx_left: u64,
    /// Next ring offset the RX DMA writes, from RXSADR.
    rx_offset: u32,
    /// Bytes received since the last RDC latch, and the latched count.
    rx_count: u32,
    rx_latched: u32,
    rx_pending: bool,
    /// Bytes the RX DMA wrote, and bytes lost with RX off or no ring.
    rx_bytes: u64,
    rx_lost: u64,
}

/// Oscillator ticks per MIDI byte on the RX line: 10 bits at 31250 baud.
pub const RX_BYTE_TICKS: u64 = 24_000_000 * 10 / 31_250;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_midi_dma_transmits_three_bytes_at_31250_baud() {
        let mut uart = Uart::default();
        let ram = [0x90, 60, 127, 0xa5];
        uart.write(0x12100, 0x6d, 2, None).unwrap().unwrap();
        uart.write(0x12108, 383, 2, None).unwrap().unwrap();
        uart.write(0x12114, crate::RAM, 4, None).unwrap().unwrap();
        uart.write(0x12118, 3, 2, Some(&ram)).unwrap().unwrap();
        // PLL48M / (4 * (383 + 1)) = 31250 baud; three bytes measured on an
        // FM-1 take 3 * (41 * 384 + 6) = 47250 PLL clocks, 23625 oscillator ticks.
        uart.advance(23624, 48_000_000);
        assert!(!uart.pending_irq());
        assert!(uart.transmitted.is_empty());
        uart.advance(1, 48_000_000);
        assert!(uart.pending_irq());
        assert_eq!(uart.transmitted, [0x90, 60, 127]);
        assert_eq!(uart.read(0x12100, 2), Some(Ok(0x806d)));
        assert_eq!(uart.read(0x12128, 2), Some(Ok(0)));
        assert_eq!(ram, [0x90, 60, 127, 0xa5]);
        uart.write(0x12100, 0x206d, 2, None).unwrap().unwrap();
        assert!(!uart.pending_irq());
        assert_eq!(uart.read(0x12100, 2), Some(Ok(0x6d)));
    }

    #[test]
    fn midi_in_bytes_land_in_the_ring_at_31250_baud_and_wrap() {
        let mut ram = vec![0u8; 16];
        let mut uart = Uart::default();
        uart.write(0x1211c, crate::RAM + 4, 4, None)
            .unwrap()
            .unwrap(); // RXSADR
        uart.write(0x12120, crate::RAM + 6, 4, None)
            .unwrap()
            .unwrap(); // RXEADR
        uart.write(0x12100, 0x41, 2, None).unwrap().unwrap(); // UTEN + RXDMA
        uart.receive(&[0xf8, 0xfa, 0x90]);
        assert_eq!(uart.ticks_to_event(48_000_000), Some(RX_BYTE_TICKS));
        uart.advance_rx((RX_BYTE_TICKS - 1) as u32, &mut ram);
        assert_eq!(ram[4], 0);
        uart.advance_rx(1, &mut ram);
        assert_eq!(ram[4], 0xf8);
        assert_eq!(uart.read(0x12100, 2), Some(Ok(0x4041)));
        uart.advance_rx(RX_BYTE_TICKS as u32, &mut ram);
        uart.advance_rx(RX_BYTE_TICKS as u32, &mut ram);
        assert_eq!((ram[4], ram[5]), (0x90, 0xfa), "a two-byte ring wraps");
        assert_eq!(uart.ticks_to_event(48_000_000), None);
        uart.write(0x12100, 0x10c1, 2, None).unwrap().unwrap(); // RDC, clear RX pending
        assert_eq!(uart.read(0x12128, 2), Some(Ok(3)));
        assert_eq!(uart.read(0x12100, 2), Some(Ok(0x41)));
        assert_eq!(uart.rx_counts(), (0, 3, 0));
    }

    #[test]
    fn midi_in_bytes_are_lost_while_receive_is_off() {
        let mut ram = vec![0u8; 16];
        let mut uart = Uart::default();
        uart.receive(&[0xf8]);
        uart.advance_rx(RX_BYTE_TICKS as u32, &mut ram);
        assert_eq!(uart.rx_counts(), (0, 0, 1));
        assert_eq!(uart.read(0x12128, 2), Some(Ok(0)));
    }

    #[test]
    fn byte_transmit_divider_and_interrupt_enable_are_independent() {
        let mut uart = Uart::default();
        uart.write(0x12100, 0x11, 2, None).unwrap().unwrap();
        uart.write(0x1210c, 0xf8, 1, None).unwrap().unwrap();
        // OSC source, divider 3, BAUD reset to zero: 30 ticks.
        uart.advance(29, 24_000_000);
        assert_eq!(uart.read(0x12100, 2), Some(Ok(0x11)));
        uart.advance(1, 24_000_000);
        assert_eq!(uart.transmitted, [0xf8]);
        assert!(!uart.pending_irq());
        uart.write(0x12100, 0x15, 2, None).unwrap().unwrap();
        assert!(uart.pending_irq());
        uart.write(0x12100, 0x2015, 2, None).unwrap().unwrap();
        assert!(!uart.pending_irq());
        // Read-only pending bits cannot manufacture receive/TX events.
        uart.write(0x12100, 0xc815, 2, None).unwrap().unwrap();
        assert_eq!(uart.read(0x12100, 2), Some(Ok(0x15)));
    }

    #[test]
    fn dma_checks_sram_bounds_and_disabled_transmit_does_not_complete() {
        let mut uart = Uart::default();
        let ram = [0x90, 60];
        uart.write(0x12114, crate::RAM + 1, 4, None)
            .unwrap()
            .unwrap();
        assert!(uart.write(0x12118, 2, 2, Some(&ram)).unwrap().is_err());
        uart.write(0x12114, crate::RAM, 4, None).unwrap().unwrap();
        assert!(uart.write(0x12118, 2, 2, Some(&ram)).unwrap().is_err());
        uart.write(0x12100, 5, 2, None).unwrap().unwrap();
        uart.write(0x12118, 2, 2, Some(&ram)).unwrap().unwrap();
        uart.write(0x12100, 0, 2, None).unwrap().unwrap();
        uart.advance(1_000_000, 24_000_000);
        assert!(uart.transmitted.is_empty());
        assert!(!uart.pending_irq());
        uart.write(0x12100, 7, 2, None).unwrap().unwrap();
        assert!(uart.write(0x1210c, 0x90, 1, None).unwrap().is_err());
        uart.write(0x12100, 5, 2, None).unwrap().unwrap();
        uart.write(0x12104, 1, 2, None).unwrap().unwrap();
        assert!(uart.write(0x1210c, 0x90, 1, None).unwrap().is_err());
    }
}
impl Uart {
    fn index(address: u32) -> Option<usize> {
        let offset = address.checked_sub(0x12100)?;
        (offset <= 40 && offset.is_multiple_of(4)).then_some((offset / 4) as usize)
    }
    fn width(index: usize, size: usize) -> bool {
        match index {
            0..=2 | 6 | 10 => matches!(size, 2 | 4),
            3 => matches!(size, 1 | 4),
            _ => size == 4,
        }
    }
    pub(crate) fn read(&self, address: u32, size: usize) -> Option<Result<u32, &'static str>> {
        let index = Self::index(address)?;
        Some(if !Self::width(index, size) {
            Err("unsupported UART register access width")
        } else {
            match index {
                2 | 6 => Err("UART register is write-only"),
                3 => Err("UART byte input is not implemented"),
                10 => Ok(self.rx_latched),
                0 => Ok(self.registers[0]
                    | if self.pending { 0x8000 } else { 0 }
                    | if self.rx_pending { 0x4000 } else { 0 }),
                _ => Ok(self.registers[index]),
            }
        })
    }
    pub(crate) fn write(
        &mut self,
        address: u32,
        value: u32,
        size: usize,
        ram: Option<&[u8]>,
    ) -> Option<Result<(), &'static str>> {
        let index = Self::index(address)?;
        Some(if !Self::width(index, size) {
            Err("unsupported UART register access width")
        } else {
            match index {
                0 => {
                    // CON0 clear strobes: 0x3400 clears TX/RX/timeout;
                    // 0x80 latches HRXCNT. Pending bits 15/14/11 are RO.
                    // Preserve the receive configuration, never latch strobes
                    // or manufacture pending events on a quiet input.
                    if value & 0x2000 != 0 {
                        self.pending = false;
                    }
                    if value & 0x1000 != 0 {
                        self.rx_pending = false;
                    }
                    if value & 0x80 != 0 {
                        self.rx_latched = self.rx_count & 0xffff;
                        self.rx_count = 0;
                    }
                    self.registers[0] = value & 0x37f;
                    if value & 1 == 0 {
                        self.transmit.clear();
                        self.remaining = 0;
                    }
                    Ok(())
                }
                1 | 2 => {
                    self.registers[index] = value & 65535;
                    Ok(())
                }
                3 => self.start(&[value as u8], false),
                6 => {
                    let count = (value & 65535) as usize;
                    if count == 0 {
                        Ok(())
                    } else {
                        let bytes = ram.and_then(|ram| {
                            let offset = self.registers[5].checked_sub(crate::RAM)? as usize;
                            ram.get(offset..offset.checked_add(count)?)
                        });
                        match bytes {
                            Some(bytes) => self.start(bytes, true),
                            None => Err("UART transmit DMA source is outside SRAM"),
                        }
                    }
                }
                10 => Err("UART receive count latch is read-only"),
                _ => {
                    self.registers[index] = value;
                    if index == 7 {
                        self.rx_offset = 0;
                    }
                    Ok(())
                }
            }
        })
    }

    /// Put bytes on the RX line: the first starts now if the line is idle,
    /// the others follow back to back.
    pub(crate) fn receive(&mut self, bytes: &[u8]) {
        self.rx_queue.extend(bytes.iter().copied());
        if self.rx_left == 0 && !self.rx_queue.is_empty() {
            self.rx_left = RX_BYTE_TICKS;
        }
    }

    /// Bytes waiting for or on the RX line; bytes received; bytes lost.
    pub(crate) fn rx_counts(&self) -> (usize, u64, u64) {
        (self.rx_queue.len(), self.rx_bytes, self.rx_lost)
    }

    /// Bytes that finished transmitting (bounded).
    pub(crate) fn transmitted(&self) -> &[u8] {
        &self.transmitted
    }

    /// Advance the RX line by `ticks` (spans end at its events, so at most
    /// one byte completes); a received byte goes into `ram` (SRAM).
    pub(crate) fn advance_rx(&mut self, ticks: u32, ram: &mut [u8]) {
        if self.rx_left == 0 {
            return;
        }
        if (ticks as u64) < self.rx_left {
            self.rx_left -= ticks as u64;
            return;
        }
        self.rx_left = 0;
        let Some(byte) = self.rx_queue.pop_front() else {
            return;
        };
        if !self.rx_queue.is_empty() {
            self.rx_left = RX_BYTE_TICKS;
        }
        let (start, end) = (self.registers[7], self.registers[8]);
        let on = self.registers[0] & 0x41 == 0x41;
        let offset = start.wrapping_add(self.rx_offset).wrapping_sub(crate::RAM) as usize;
        if !on || end <= start || offset >= ram.len() {
            self.rx_lost += 1;
            return;
        }
        ram[offset] = byte;
        self.rx_offset += 1;
        if start + self.rx_offset >= end {
            self.rx_offset = 0;
        }
        self.rx_count += 1;
        self.rx_bytes += 1;
        self.rx_pending = true;
    }

    fn start(&mut self, bytes: &[u8], dma: bool) -> Result<(), &'static str> {
        if self.registers[0] & 3 != 1 || self.registers[1] != 0 {
            return Err("UART transmit requires enabled 8N1 configuration");
        }
        if !self.transmit.is_empty() {
            return Err("UART transmit already in progress");
        }
        let divider = if self.registers[0] & 16 == 0 { 4 } else { 3 };
        let baud = self.registers[2] as u64 + 1;
        // A DMA byte takes 10 + 1/divider bit times, plus six clocks, not the
        // nominal ten bit times: measured on an FM-1
        // with TIMER4 over 1, 3 and 8 bytes, BAUD 95 to 767, both dividers
        // (upstream 98d08b5: 7881, 23628 and 63004 oscillator ticks for BAUD
        // 383 and divider 4, where the nominal frame would take 7680 per byte).
        let clocks_per_byte = if dma {
            (10 * divider + 1) * baud + 6
        } else {
            10 * divider * baud
        };
        self.remaining = bytes.len() as u64 * clocks_per_byte * 24_000_000;
        self.transmit.extend_from_slice(bytes);
        self.pending = false;
        Ok(())
    }

    /// Oscillator ticks at `hz` until a transmission in progress ends.
    pub(crate) fn ticks_to_event(&self, hz: u32) -> Option<u64> {
        let transmit = (!self.transmit.is_empty() && self.registers[0] & 1 != 0 && hz != 0)
            .then(|| self.remaining.div_ceil(hz as u64).max(1));
        let receive = (self.rx_left > 0).then_some(self.rx_left);
        transmit.into_iter().chain(receive).min()
    }

    pub(crate) fn advance(&mut self, ticks: u32, hz: u32) {
        if self.transmit.is_empty() || self.registers[0] & 1 == 0 {
            return;
        }
        self.remaining = self.remaining.saturating_sub(ticks as u64 * hz as u64);
        if self.remaining == 0 {
            self.pending = true;
            let keep = self.transmit.len().min(4096 - self.transmitted.len());
            self.transmitted.extend_from_slice(&self.transmit[..keep]);
            self.transmit.clear();
        }
    }

    pub(crate) fn pending_irq(&self) -> bool {
        self.pending && self.registers[0] & 5 == 5
    }
}
