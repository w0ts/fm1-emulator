// SPDX-License-Identifier: GPL-3.0-only
// SPI0 serial NOR. Application ELF supplies decrypted XIP separately.

enum Pending {
    Program(usize, Vec<u8>),
    Erase(usize),
}

pub struct Nor {
    regs: [u32; 14],
    command: Vec<u8>,
    selected: bool,
    pub bytes: Vec<u8>,
    cursor: usize,
    decoded: Option<Vec<u8>>,
    key: u16,
    write_enabled: bool,
    busy_ticks: u32,
    pending: Option<Pending>,
    /// A suspended sector erase: its remaining busy ticks.
    suspended: Option<u32>,
    /// Counts completed programs and erases (flash contents changed).
    generation: u64,
    /// Completed programs and erases only (not package loads or restores).
    writes: u64,
    /// Per 4 KiB sector: erased or programmed during this run, or restored
    /// from a saved flash state (`flash_state`).
    tracked: Vec<bool>,
}

/// Erase unit of the P25Q80H (sector erase, command 0x20).
pub const SECTOR: usize = 4096;

/// Everything an XIP read depends on besides the address: when it is
/// unchanged, so is what every XIP address reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct XipKey {
    registers: [u32; 6],
    busy: bool,
    generation: u64,
}

impl Default for Nor {
    fn default() -> Self {
        Self {
            regs: [1, 0, 0, 0x4000, 1, 0, 0, 0, 0, 0, 32, 0, 0, 0],
            command: vec![],
            selected: false,
            bytes: vec![255; 1024 * 1024],
            cursor: 0,
            decoded: None,
            key: 0,
            write_enabled: false,
            busy_ticks: 0,
            pending: None,
            suspended: None,
            generation: 0,
            writes: 0,
            tracked: vec![false; 1024 * 1024 / SECTOR],
        }
    }
}

impl Nor {
    pub fn load(&mut self, bytes: &[u8], key: u16) {
        self.bytes[..bytes.len()].copy_from_slice(bytes);
        let mut decoded = self.bytes[0x4000..].to_vec();
        crate::package::sfc(&mut decoded, key);
        self.decoded = Some(decoded);
        self.key = key;
        self.generation += 1;
        self.regs[..3].copy_from_slice(&[0x809803b5, 1, 0x8e17]);
    }
    /// Completed page programs and sector erases so far.
    pub fn writes(&self) -> u64 {
        self.writes
    }
    /// Offsets of the sectors erased, programmed or restored this run.
    pub fn tracked_sectors(&self) -> Vec<usize> {
        (self.tracked.iter().enumerate())
            .filter(|(_, &tracked)| tracked)
            .map(|(index, _)| index * SECTOR)
            .collect()
    }
    /// Lay a saved sector over the flash, as if the firmware had erased and
    /// programmed it: raw bytes, the encrypted XIP view and the XIP key all
    /// follow. `offset` must be sector-aligned and `data` one sector long.
    pub fn restore_sector(&mut self, offset: usize, data: &[u8]) -> Result<(), String> {
        if !offset.is_multiple_of(SECTOR)
            || data.len() != SECTOR
            || offset + SECTOR > self.bytes.len()
        {
            return Err(format!(
                "flash sector {offset:#x} is outside the flash or misaligned"
            ));
        }
        self.bytes[offset..offset + SECTOR].copy_from_slice(data);
        self.sync_decoded(offset, offset + SECTOR);
        self.tracked[offset / SECTOR] = true;
        self.generation += 1;
        Ok(())
    }
    /// Re-encrypt `start..end` of the raw flash into the XIP view, where the
    /// SFC decrypts on the fly (application area, from 0x4000).
    fn sync_decoded(&mut self, start: usize, end: usize) {
        if let Some(decoded) = &mut self.decoded {
            let start = start.max(0x4000);
            if start < end {
                let plain = &mut decoded[start - 0x4000..end - 0x4000];
                plain.copy_from_slice(&self.bytes[start..end]);
                for (i, block) in plain.chunks_mut(32).enumerate() {
                    crate::package::enc(block, self.key ^ ((start - 0x4000) / 4 + i * 8) as u16);
                }
            }
        }
    }
    pub fn packaged(&self) -> bool {
        self.decoded.is_some()
    }
    pub(crate) fn xip_key(&self) -> XipKey {
        let r = &self.regs;
        XipKey {
            registers: [r[0] & 1, r[3], r[4], r[6], r[7], r[10] & 32],
            busy: self.busy_ticks != 0,
            generation: self.generation,
        }
    }
    /// Whether an aligned access of up to 4 bytes is either wholly inside
    /// or wholly outside the plain (unencrypted) window, wherever it is.
    pub(crate) fn plain_window_aligned(&self) -> bool {
        self.regs[4] & 2 == 0 || (self.regs[7].is_multiple_of(4) && self.regs[6] % 4 == 3)
    }
    pub fn xip_active(&self) -> bool {
        // SFC CON (0x40200) enable and IOMAP CON0 (0x5101c) flash routing.
        self.regs[0] & 1 != 0 && self.regs[10] & 32 != 0
    }

    /// Little-endian value of 1, 2 or 4 bytes (any other size: 0..4 bytes).
    #[inline(always)]
    fn value(bytes: &[u8]) -> u32 {
        match *bytes {
            [a, b, c, d] => u32::from_le_bytes([a, b, c, d]),
            [a, b] => u16::from_le_bytes([a, b]) as u32,
            [a] => a as u32,
            _ => bytes
                .iter()
                .enumerate()
                .fold(0, |value, (i, byte)| value | ((*byte as u32) << (i * 8))),
        }
    }

    #[inline]
    pub fn xip(&self, address: u32, size: usize) -> Option<Result<u32, &'static str>> {
        // The SFC maps flash offset 0x4000 (SFC BASE, 0x4020c) at CPU
        // address 0x02000000. Registers are read from `regs` directly: see
        // `register_index` for their addresses.
        let offset = address.checked_sub(0x0200_0000)? as usize + self.regs[3] as usize;
        let bytes = self.bytes.get(offset..offset.checked_add(size)?)?;
        if self.busy_ticks != 0 {
            return Some(Err("XIP unavailable while SPI NOR is busy"));
        }
        if !self.xip_active() {
            return Some(Err(
                "XIP unavailable while SFC or flash pin routing is disabled",
            ));
        }
        // ENC CON (0x40300); the plain window is 0x4030c..=0x40308.
        let control = self.regs[4];
        let plain = control & 1 == 0
            || (control & 2 != 0
                && address >= self.regs[7]
                && address.checked_add(size as u32 - 1)? <= self.regs[6]);
        if !plain {
            if let Some(decoded) = &self.decoded {
                if offset < 0x4000 {
                    return Some(Err(
                        "encrypted XIP below application area is not implemented",
                    ));
                }
                return Some(Ok(Self::value(
                    &decoded[offset - 0x4000..offset - 0x4000 + size],
                )));
            } else {
                return Some(Err(
                    "encrypted XIP outside the supplied application is not available",
                ));
            }
        }
        Some(Ok(Self::value(bytes)))
    }

    pub fn read(&self, a: u32) -> Option<u32> {
        Self::register_index(a).map(|index| {
            let value = self.regs[index];
            // Bit 31 is transaction busy, not retained configuration. The
            // functional bus completes each access before a following read.
            if a == 0x40200 {
                value & !0x80000000
            } else {
                value
            }
        })
    }
    fn register_index(a: u32) -> Option<usize> {
        match a {
            0x40200..=0x4020c if a & 3 == 0 => Some(((a - 0x40200) / 4) as usize),
            0x40300..=0x40314 if a & 3 == 0 => Some(4 + ((a - 0x40300) / 4) as usize),
            0x5101c => Some(10),
            0x11c00..=0x11c08 if a & 3 == 0 => Some(11 + ((a - 0x11c00) / 4) as usize),
            _ => None,
        }
    }
    pub fn chip_select(&mut self, selected: bool) {
        if self.selected != selected {
            if !selected && !self.suspend_or_resume() && self.busy_ticks == 0 {
                self.finish_command();
            }
            self.command.clear();
            self.cursor = 0;
        }
        self.selected = selected;
    }

    /// P25Q80H datasheet sections 10.36 and 10.38: erase suspend (75h or B0h)
    /// stops a sector erase (WIP 0, WEL 0, SUS1 1; the array reads again) and
    /// resume (7Ah or 30h) continues it (WIP 1, WEL 1, SUS1 0). Modelled
    /// without the suspend latency (tESL, 30 us max). A suspend of a page
    /// program, and a program while an erase is suspended, are not modelled:
    /// ignored. Returns whether the command was one of the four.
    fn suspend_or_resume(&mut self) -> bool {
        match self.command.as_slice() {
            [0x75] | [0xb0] => {
                if self.suspended.is_none()
                    && self.busy_ticks != 0
                    && matches!(self.pending, Some(Pending::Erase(_)))
                {
                    self.suspended = Some(self.busy_ticks);
                    self.busy_ticks = 0;
                    self.write_enabled = false;
                }
                true
            }
            [0x7a] | [0x30] => {
                if let Some(remaining) = self.suspended.take() {
                    self.busy_ticks = remaining;
                    self.write_enabled = true;
                }
                true
            }
            _ => false,
        }
    }

    fn finish_command(&mut self) {
        // P25Q80H datasheet sections 10.2, 10.3, 10.20 and 10.24:
        // commands latch on CS rising; writes require WEL. Typical times are
        // 2 ms program and 8 ms sector erase, on the 24 MHz oscillator clock.
        if self.suspended.is_some() && matches!(self.command.first(), Some(0x02 | 0x20)) {
            return; // (no new program or erase while an erase is suspended)
        }
        match self.command.as_slice() {
            [0x06] => self.write_enabled = true,
            [0x04] => self.write_enabled = false,
            [0x02, a, b, c, data @ ..] if self.write_enabled && !data.is_empty() => {
                let address = ((*a as usize) << 16) | ((*b as usize) << 8) | *c as usize;
                self.pending = Some(Pending::Program(address % self.bytes.len(), data.to_vec()));
                self.busy_ticks = 48_000;
            }
            [0x20, a, b, c] if self.write_enabled => {
                let address = ((*a as usize) << 16) | ((*b as usize) << 8) | *c as usize;
                self.pending = Some(Pending::Erase((address % self.bytes.len()) & !0xfff));
                self.busy_ticks = 192_000;
            }
            _ => {}
        }
    }

    /// Oscillator ticks until a program or erase in progress completes.
    pub(crate) fn ticks_to_event(&self) -> Option<u64> {
        (self.pending.is_some() && self.suspended.is_none())
            .then(|| (self.busy_ticks as u64).max(1))
    }

    pub(crate) fn advance(&mut self, ticks: u32) {
        if self.suspended.is_some() {
            return; // (a suspended erase does not progress)
        }
        self.busy_ticks = self.busy_ticks.saturating_sub(ticks);
        if self.busy_ticks != 0 {
            return;
        }
        let (start, end) = match self.pending.take() {
            Some(Pending::Program(address, data)) => {
                let page = address & !255;
                // The page buffer wraps; when overloaded, only its last 256
                // bytes survive. Programming can only clear bits.
                for (i, byte) in data.iter().enumerate().skip(data.len().saturating_sub(256)) {
                    self.bytes[page + ((address + i) & 255)] &= byte;
                }
                (page, page + 256)
            }
            Some(Pending::Erase(address)) => {
                self.bytes[address..address + 4096].fill(255);
                (address, address + 4096)
            }
            None => return,
        };
        self.write_enabled = false;
        self.generation += 1;
        self.writes += 1;
        self.tracked[start / SECTOR..end.div_ceil(SECTOR)].fill(true);
        self.sync_decoded(start, end);
    }
    pub fn write(&mut self, a: u32, v: u32) -> Option<Result<(), &'static str>> {
        self.read(a)?;
        if (a == 0x40304 && v != 0) || ((a == 0x40310 || a == 0x40314) && v != 0) {
            return Some(Err(
                "SFC dynamic key and encrypted-window changes are not implemented",
            ));
        }
        let mut value = v;
        if a == 0x11c00 {
            value = v & !0xc000;
            if v & 0x4000 == 0 {
                value |= self.read(a).unwrap() & 0x8000;
            }
        }
        if a == 0x11c08 {
            if !self.selected {
                return Some(Err("SPI0 transfer with flash deselected"));
            }
            self.command.push(v as u8);
            let len = self.command.len();
            value = 255;
            match self.command[0] {
                0x4b => {
                    // Puya P25Q80H: four dummy bytes then a 128-bit ID.
                    // Stable emulator chip identity, not the physical unit's ID.
                    const UID: [u8; 16] = *b"FM1-EMU-NOR-0001";
                    if len > 5 {
                        value = UID.get(len - 6).copied().unwrap_or(255) as u32;
                    }
                }
                0x9f => {
                    if len > 1 {
                        value = [0x85, 0x60, 0x14].get(len - 2).copied().unwrap_or(255);
                    }
                }
                0x05 => value = ((self.write_enabled as u32) << 1) | (self.busy_ticks != 0) as u32,
                0x35 => value = (self.suspended.is_some() as u32) << 2, // SUS1 (S10)
                0x06 | 0x04 | 0x02 | 0x20 | 0x75 | 0xb0 | 0x7a | 0x30 => {}
                0x03 | 0x0b | 0x6b => {
                    let start = if self.command[0] == 3 { 4 } else { 5 };
                    if len == 4 {
                        self.cursor = ((self.command[1] as usize) << 16)
                            | ((self.command[2] as usize) << 8)
                            | self.command[3] as usize;
                    }
                    if len > start {
                        value = self.bytes[self.cursor % self.bytes.len()] as u32;
                        self.cursor += 1;
                    }
                }
                _ => return Some(Err("unimplemented SPI NOR command")),
            }
            self.regs[11] |= 0x8000;
        }
        self.regs[Self::register_index(a).unwrap()] = value;
        Some(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::Nor;

    fn transaction(nor: &mut Nor, bytes: &[u32]) -> u32 {
        nor.chip_select(true);
        for &byte in bytes {
            nor.write(0x11c08, byte).unwrap().unwrap();
        }
        let result = nor.read(0x11c08).unwrap();
        nor.chip_select(false);
        result
    }

    #[test]
    fn writes_require_a_complete_write_enable_transaction() {
        let mut nor = Nor::default();
        nor.chip_select(true);
        nor.write(0x11c08, 6).unwrap().unwrap();
        assert!(!nor.write_enabled);
        nor.chip_select(false);
        assert_eq!(transaction(&mut nor, &[5, 255]), 2);
        transaction(&mut nor, &[4]);
        assert_eq!(transaction(&mut nor, &[5, 255]), 0);
        transaction(&mut nor, &[2, 9, 0, 0, 0]);
        nor.advance(48_000);
        assert_eq!(nor.bytes[0x90000], 255);
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[0x20, 9, 0]); // incomplete address
        assert_eq!(nor.busy_ticks, 0);
        assert!(nor.write_enabled);
    }

    #[test]
    fn page_program_waits_for_completion_wraps_and_only_clears_bits() {
        let mut nor = Nor::default();
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[2, 9, 0, 255, 0x0f, 0xf0]);
        assert_eq!(nor.bytes[0x900ff], 255);
        assert_eq!(transaction(&mut nor, &[5, 255]), 3); // WEL and WIP
        assert!(nor.xip(0x0208c000, 1).unwrap().is_err());
        nor.advance(47_999);
        assert_eq!(transaction(&mut nor, &[5, 255]), 3);
        nor.advance(1);
        assert_eq!(transaction(&mut nor, &[5, 255]), 0);
        assert_eq!(nor.bytes[0x900ff], 0x0f);
        assert_eq!(nor.bytes[0x90000], 0xf0);
        assert_eq!(nor.bytes[0x90100], 255);
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[2, 9, 0, 255, 0xf0]);
        nor.advance(48_000);
        assert_eq!(nor.bytes[0x900ff], 0);
    }

    #[test]
    fn page_buffer_keeps_only_the_last_256_bytes() {
        let mut nor = Nor::default();
        let mut command = vec![2, 9, 0, 0, 0];
        command.extend([255; 256]);
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &command);
        nor.advance(48_000);
        assert!(nor.bytes[0x90000..0x90100].iter().all(|&byte| byte == 255));
    }

    #[test]
    fn erase_updates_raw_and_encrypted_xip_without_touching_neighbors() {
        let mut nor = Nor::default();
        nor.load(&vec![0x55; 0x94000], 0x980f);
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[0x20, 9, 0x11, 0x23]);
        assert_eq!(transaction(&mut nor, &[5, 255]), 3);
        nor.advance(192_000);
        assert_eq!(transaction(&mut nor, &[5, 255]), 0);
        assert!(nor.bytes[0x91000..0x92000].iter().all(|&byte| byte == 255));
        assert_eq!(nor.bytes[0x90fff], 0x55);
        assert_eq!(nor.bytes[0x92000], 0x55);
        let mut block = [255; 32];
        crate::package::enc(&mut block, 0x980f ^ ((0x91000 - 0x4000) / 4) as u16);
        assert_eq!(
            nor.xip(0x0208d000, 4).unwrap().unwrap(),
            u32::from_le_bytes(block[..4].try_into().unwrap())
        );
        nor.write(0x40300, 0).unwrap().unwrap();
        assert_eq!(nor.xip(0x0208d000, 4).unwrap().unwrap(), u32::MAX);
    }

    #[test]
    fn an_erase_suspends_reads_and_resumes_where_it_stopped() {
        let mut nor = Nor::default();
        nor.bytes[0x91000] = 0x55;
        transaction(&mut nor, &[0x75]); // idle: ignored
        transaction(&mut nor, &[0x7a]);
        assert_eq!(transaction(&mut nor, &[5, 255]), 0);
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[0x20, 9, 0x10, 0]);
        nor.advance(100_000);
        transaction(&mut nor, &[0x75]);
        assert_eq!(transaction(&mut nor, &[5, 255]), 0); // WIP 0, WEL 0
        assert_eq!(transaction(&mut nor, &[0x35, 255]), 4); // SUS1
        assert_eq!(nor.ticks_to_event(), None);
        nor.advance(1_000_000); // suspended: no progress
        assert_eq!(nor.bytes[0x91000], 0x55);
        assert_eq!(transaction(&mut nor, &[3, 9, 0x10, 0, 255]), 0x55); // reads
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[0x20, 9, 0x20, 0]); // refused while suspended
        transaction(&mut nor, &[0x7a]);
        assert_eq!(transaction(&mut nor, &[5, 255]), 3);
        assert_eq!(transaction(&mut nor, &[0x35, 255]), 0);
        nor.advance(91_999);
        assert_eq!(transaction(&mut nor, &[5, 255]) & 1, 1);
        nor.advance(1);
        assert_eq!(transaction(&mut nor, &[5, 255]), 0);
        assert_eq!(nor.bytes[0x91000], 255);
        assert_eq!(nor.writes(), 1);
    }

    #[test]
    fn erases_and_programs_track_their_sectors_and_count_as_writes() {
        let mut nor = Nor::default();
        assert!(nor.tracked_sectors().is_empty());
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[0x20, 9, 0xf1, 0x23]);
        assert_eq!(nor.writes(), 0, "an erase counts once it completes");
        nor.advance(192_000);
        transaction(&mut nor, &[6]);
        transaction(&mut nor, &[2, 0x0f, 0xe0, 0x10, 0x5a]);
        nor.advance(48_000);
        assert_eq!(nor.writes(), 2);
        assert_eq!(nor.tracked_sectors(), vec![0x9f000, 0xfe000]);
        // Reads and a write without WEL change nothing.
        transaction(&mut nor, &[3, 1, 0, 0, 255]);
        transaction(&mut nor, &[0x20, 1, 0, 0]);
        nor.advance(192_000);
        assert_eq!(nor.writes(), 2);
        assert_eq!(nor.tracked_sectors().len(), 2);
    }

    #[test]
    fn a_restored_sector_reads_like_an_erased_and_programmed_one() {
        // Two flashes from the same package: one erased and programmed by
        // the guest, one restored from its saved bytes; every view agrees.
        let mut written = Nor::default();
        written.load(&vec![0x55; 0x94000], 0x980f);
        transaction(&mut written, &[6]);
        transaction(&mut written, &[0x20, 9, 0x10, 0]);
        written.advance(192_000);
        let mut program = vec![2, 9, 0x10, 0];
        program.extend((0..256).map(|i| i as u32));
        transaction(&mut written, &[6]);
        transaction(&mut written, &program);
        written.advance(48_000);
        let mut restored = Nor::default();
        restored.load(&vec![0x55; 0x94000], 0x980f);
        let before = restored.xip_key();
        restored
            .restore_sector(0x91000, &written.bytes[0x91000..0x92000])
            .unwrap();
        assert!(restored.xip_key() != before, "XIP caches must rebuild");
        assert_eq!(restored.bytes, written.bytes);
        assert_eq!(restored.decoded, written.decoded);
        assert_eq!(restored.tracked_sectors(), vec![0x91000]);
        assert_eq!(restored.writes(), 0, "a restore is not guest activity");
        for address in [0x0208d000, 0x0208d0fc, 0x0208dffc] {
            assert_eq!(
                restored.xip(address, 4).unwrap().unwrap(),
                written.xip(address, 4).unwrap().unwrap()
            );
        }
        assert!(restored.restore_sector(0x91001, &[0; 4096]).is_err());
        assert!(restored.restore_sector(0x91000, &[0; 16]).is_err());
        assert!(restored.restore_sector(0x100000, &[0; 4096]).is_err());
    }

    #[test]
    fn unique_id_consumes_four_dummy_bytes_and_restarts_on_chip_select() {
        let mut nor = Nor::default();
        for _ in 0..2 {
            nor.chip_select(true);
            for byte in [0x4b, 0, 0, 0, 0] {
                nor.write(0x11c08, byte).unwrap().unwrap();
                assert_eq!(nor.read(0x11c08), Some(255));
            }
            for &byte in b"FM1-EMU-NOR-0001" {
                nor.write(0x11c08, 255).unwrap().unwrap();
                assert_eq!(nor.read(0x11c08), Some(byte as u32));
            }
            nor.chip_select(false);
        }
    }

    #[test]
    fn spi_and_plain_xip_read_the_same_physical_bytes() {
        let mut nor = Nor::default();
        nor.bytes[0xa0000..0xa0004].copy_from_slice(&[0x46, 0x53, 0x4d, 0x50]);
        nor.write(0x4030c, 0x0208f000).unwrap().unwrap();
        nor.write(0x40308, 0x07ffffff).unwrap().unwrap();
        nor.write(0x40300, 3).unwrap().unwrap();
        assert_eq!(nor.xip(0x0209c000, 4).unwrap().unwrap(), 0x504d5346);
        nor.chip_select(true);
        for byte in [0x03, 0x0a, 0x00, 0x00] {
            nor.write(0x11c08, byte).unwrap().unwrap();
        }
        for expected in [0x46, 0x53, 0x4d, 0x50] {
            nor.write(0x11c08, 0xff).unwrap().unwrap();
            assert_eq!(nor.read(0x11c08), Some(expected));
        }
    }

    #[test]
    fn quad_output_read_uses_the_raw_package_bytes_after_its_dummy_byte() {
        let mut nor = Nor::default();
        nor.bytes[0x92ff0..0x92ff4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        nor.chip_select(true);
        for byte in [0x6b, 9, 0x2f, 0xf0, 0] {
            nor.write(0x11c08, byte).unwrap().unwrap();
        }
        for expected in [0x12, 0x34, 0x56, 0x78] {
            nor.write(0x11c08, 255).unwrap().unwrap();
            assert_eq!(nor.read(0x11c08), Some(expected));
        }
    }
}
