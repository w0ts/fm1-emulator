// SPDX-License-Identifier: GPL-3.0-only
//! The engine's view of the baseband RAM window (BLE-HW-FACTS §3, §4):
//! the instance table and the 324-byte per-link control block, reached by
//! "DMA" into guest SRAM. Offsets are the fact sheet's [M:s]/[M:d] field
//! offsets; what the engine does with each field is the model's hypothesis
//! and is labelled where it is used (ble_engine.rs).

use crate::RAM;

/// Control-block field offsets (§3).
pub mod field {
    pub const ADVIDX: u32 = 0x004;
    pub const FORMAT: u32 = 0x006;
    pub const OPTCNTL: u32 = 0x008;
    pub const BDADDR0: u32 = 0x00A;
    pub const BDADDR1: u32 = 0x00C;
    pub const TXTOG: u32 = 0x00E;
    pub const RXTOG: u32 = 0x010;
    pub const TXPTR0: u32 = 0x012;
    pub const TXAHDR0: u32 = 0x016;
    pub const TXDHDR0: u32 = 0x01A;
    pub const RXPTR0: u32 = 0x01E;
    pub const WINCNTL0: u32 = 0x022;
    pub const WINCNTL1: u32 = 0x024;
    pub const RXAHDR0: u32 = 0x026;
    pub const RXDHDR0: u32 = 0x02A;
    pub const CHMAP0: u32 = 0x02E;
    pub const CHMAP1: u32 = 0x030;
    pub const CHMAP2: u32 = 0x032;
    pub const LASTCHMAP: u32 = 0x034;
    pub const RXMAXBUF: u32 = 0x036;
    pub const RXSTAT0: u32 = 0x038;
    pub const FILTERCNTL: u32 = 0x03C;
    pub const WHITELIST0: u32 = 0x03E;
    pub const CRCWORD0: u32 = 0x044;
    pub const CRCWORD1: u32 = 0x046;
    pub const WIDEN0: u32 = 0x048;
    pub const WIDEN1: u32 = 0x04A;
    pub const LOCALADR: u32 = 0x052;
    pub const EVTCOUNT: u32 = 0x058;
    pub const TXBUF0CNTL: u32 = 0x064;
    pub const RXBUF0CNTL: u32 = 0x066;
    /// RSSI2 (§3 0x138 + 4): the word software converts to dBm.
    pub const RSSI2: u32 = 0x13C;
    pub const FRQ_IDX1: u32 = 0x090;
}

/// Size of one control block (§3).
pub const CONTROL_BLOCK: u32 = 324;

/// The baseband RAM window as programmed in `0x2FC84`/`0x2FCBC` over guest
/// SRAM. Every engine access is bounds-checked against the window: an
/// access outside it is a guest/model disagreement and faults (§4 [M:s]:
/// the engine addresses the window with 16-bit offsets).
pub struct Window<'a> {
    pub ram: &'a mut [u8],
    pub base: u32,
    pub end: u32,
}

pub type DmaResult<T> = Result<T, &'static str>;

impl Window<'_> {
    fn index(&self, offset: u32, len: u32) -> DmaResult<usize> {
        let address = self
            .base
            .checked_add(offset)
            .ok_or("BLE engine DMA offset overflows")?;
        if offset > 0xFFFF || address + len > self.end || self.end <= self.base {
            return Err("BLE engine DMA outside the baseband RAM window (0x2FC84..0x2FCBC)");
        }
        let index = address.wrapping_sub(RAM) as usize;
        if index + len as usize > self.ram.len() {
            return Err("BLE engine DMA outside SRAM");
        }
        Ok(index)
    }
    pub fn u8(&self, offset: u32) -> DmaResult<u8> {
        Ok(self.ram[self.index(offset, 1)?])
    }
    pub fn u16(&self, offset: u32) -> DmaResult<u16> {
        let i = self.index(offset, 2)?;
        Ok(u16::from_le_bytes([self.ram[i], self.ram[i + 1]]))
    }
    pub fn set_u8(&mut self, offset: u32, value: u8) -> DmaResult<()> {
        let i = self.index(offset, 1)?;
        self.ram[i] = value;
        Ok(())
    }
    pub fn set_u16(&mut self, offset: u32, value: u16) -> DmaResult<()> {
        let i = self.index(offset, 2)?;
        self.ram[i..i + 2].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }
    pub fn bytes(&self, offset: u32, len: u32) -> DmaResult<Vec<u8>> {
        let i = self.index(offset, len)?;
        Ok(self.ram[i..i + len as usize].to_vec())
    }
    pub fn set_bytes(&mut self, offset: u32, data: &[u8]) -> DmaResult<()> {
        let i = self.index(offset, data.len() as u32)?;
        self.ram[i..i + data.len()].copy_from_slice(data);
        Ok(())
    }
    /// The control block of link `n`, from the instance table at the window
    /// base: entry n = `0x8000 | offset` (§4 [M:d] value, [I] that the
    /// engine reads it). None: the link is unused.
    pub fn link(&self, n: usize) -> DmaResult<Option<Block>> {
        let entry = self.u16(2 * n as u32)?;
        if entry & 0x8000 == 0 {
            return Ok(None);
        }
        let offset = (entry & 0x7FFF) as u32;
        self.index(offset, CONTROL_BLOCK)?;
        Ok(Some(Block(offset)))
    }
}

/// A control block, as its offset in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block(pub u32);

impl Block {
    pub fn get(self, w: &Window, field: u32) -> DmaResult<u16> {
        w.u16(self.0 + field)
    }
    pub fn set(self, w: &mut Window, field: u32, value: u16) -> DmaResult<()> {
        w.set_u16(self.0 + field, value)
    }
    pub fn get8(self, w: &Window, field: u32) -> DmaResult<u8> {
        w.u8(self.0 + field)
    }
    pub fn set8(self, w: &mut Window, field: u32, value: u8) -> DmaResult<()> {
        w.set_u8(self.0 + field, value)
    }
    /// A three-halfword little-endian device address field.
    pub fn address(self, w: &Window, field: u32) -> DmaResult<[u8; 6]> {
        let mut a = [0; 6];
        for i in 0..3 {
            a[2 * i..2 * i + 2].copy_from_slice(&self.get(w, field + 2 * i as u32)?.to_le_bytes());
        }
        Ok(a)
    }
    pub fn access_address(self, w: &Window) -> DmaResult<u32> {
        Ok(self.get(w, field::BDADDR0)? as u32 | (self.get(w, field::BDADDR1)? as u32) << 16)
    }
    pub fn crc_init(self, w: &Window) -> DmaResult<u32> {
        Ok(self.get(w, field::CRCWORD0)? as u32
            | (self.get(w, field::CRCWORD1)? as u32 & 0xFF) << 16)
    }
    /// The 37-bit channel map and the used-channel count (CHMAP0/1/2).
    pub fn channel_map(self, w: &Window) -> DmaResult<(u64, u8)> {
        let c2 = self.get(w, field::CHMAP2)?;
        let map = self.get(w, field::CHMAP0)? as u64
            | (self.get(w, field::CHMAP1)? as u64) << 16
            | ((c2 & 31) as u64) << 32;
        Ok((map, (c2 >> 5) as u8))
    }
    /// Receive window in µs (WINCNTL0 low, WINCNTL1 high).
    pub fn window_us(self, w: &Window) -> DmaResult<u32> {
        Ok(self.get(w, field::WINCNTL0)? as u32 | (self.get(w, field::WINCNTL1)? as u32) << 16)
    }
    /// Window widening in µs: WIDEN0 whole 625 µs slots, WIDEN1 [9:0] µs.
    pub fn widening_us(self, w: &Window) -> DmaResult<u32> {
        Ok(self.get(w, field::WIDEN0)? as u32 * 625 + (self.get(w, field::WIDEN1)? & 0x3FF) as u32)
    }
}
