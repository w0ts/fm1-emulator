// SPDX-License-Identifier: GPL-3.0-only
use crate::devices::Devices;
use crate::lcd::{Lcd, SPI};
use crate::{RAM, RAM_SIZE, XIP, XIP_END};
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub struct AccessFault {
    pub address: u32,
    pub size: usize,
    pub operation: &'static str,
    pub reason: &'static str,
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    #[test]
    fn mmio_trace_records_writes_and_read_values_in_range_only() {
        let mut b = Bus::new(vec![0; 8]).unwrap();
        b.start_mmio_trace(vec![(0x28000, 0x280ff)], true, 2);
        b.write(0x28008, 0x46, 4).unwrap();
        assert_eq!(b.read(0x28008, 4).unwrap(), 0x46);
        b.write(0x2fc40, 1, 4).unwrap(); // outside the range
        b.write(RAM, 7, 4).unwrap(); // SRAM is never traced
        b.write(0x28010, 0x3e, 4).unwrap(); // past the record limit
        let trace = b.take_mmio_trace().unwrap();
        assert_eq!(trace.matched, 3);
        assert_eq!(trace.records.len(), 2);
        let (w, r) = (trace.records[0], trace.records[1]);
        assert!(w.write && w.address == 0x28008 && w.value == 0x46 && !w.fault);
        assert!(!r.write && r.address == 0x28008 && r.value == 0x46 && r.size == 4);
        assert!(!b.mmio_counting);
        assert!(b.take_mmio_trace().is_none());
    }
    #[test]
    fn mmio_stubs_are_opt_in_read_back_with_the_mask_and_feed_the_console() {
        let mut b = Bus::new(vec![0; 8]).unwrap();
        assert!(
            b.write(0x12f00, 1, 4).is_err(),
            "unmapped MMIO must stay visible"
        );
        b.set_mmio_stubs(vec![(0x12200, 0x122ff, 0x8000)]);
        b.stub_console = Some((0x1220c, Vec::new()));
        b.write(0x12200, 0x2011, 2).unwrap();
        assert_eq!(b.read(0x12200, 2).unwrap() & 0xffff, 0xa011);
        b.write(0x1220c, b'o' as u32, 1).unwrap();
        b.write(0x1220c, b'k' as u32, 1).unwrap();
        assert_eq!(b.stub_console.as_ref().unwrap().1, b"ok");
        assert!(b.read(0x12f00, 4).is_err());
        let report = b.mmio_stub_report();
        assert_eq!(report[0], (0x12200, 0x2011, 2));
    }
}

#[cfg(test)]
mod dma_tests {
    use super::*;
    #[test]
    fn uart_dma_completion_uses_the_selected_clock_and_source_20() {
        let mut b = Bus::new(vec![0; 8]).unwrap();
        b.write(RAM, 0x007f3c90, 4).unwrap();
        b.write(0x10010, 1 << 10, 4).unwrap(); // PLL48M
        b.write(crate::devices::IRQ_CONFIG + 2 * 4, 3 << 16, 4)
            .unwrap();
        b.write(0x12100, 0x6d, 2).unwrap();
        b.write(0x12108, 383, 2).unwrap();
        b.write(0x12114, RAM, 4).unwrap();
        b.write(0x12118, 3, 2).unwrap();
        // Measured: three bytes take 3 * (41 * 384 + 6) PLL48M clocks.
        b.advance_devices(23624);
        assert_eq!(b.pending_irq(0x100), None);
        b.advance_devices(1);
        assert_eq!(b.pending_irq(0x100), Some(20));
        assert_eq!(b.read(crate::devices::IRQ_PENDING, 4).unwrap(), 1 << 20);
        b.write(0x12100, 0x206d, 2).unwrap();
        assert_eq!(b.pending_irq(0x100), None);
        b.write(0x10010, 0, 4).unwrap(); // OSC24M halves the baud rate.
        b.write(0x12118, 3, 2).unwrap();
        b.advance_devices(47249);
        assert_eq!(b.pending_irq(0x100), None);
        b.advance_devices(1);
        assert_eq!(b.pending_irq(0x100), Some(20));
        assert_eq!(b.read(RAM, 4).unwrap(), 0x007f3c90);
    }
    #[test]
    fn packaged_lcd_dma_honors_decryption_mapping_and_xip_enable() {
        let mut plain = vec![0; 0x140];
        plain[0x121..0x129].copy_from_slice(&[0xf8, 0, 7, 0xe0, 0, 0x1f, 0xff, 0xff]);
        crate::package::sfc(&mut plain, 0x980f);
        let mut raw = vec![255; 0x4140];
        raw[0x4000..].copy_from_slice(&plain);
        // The application view deliberately differs from the real flash;
        // DMA must go through SFC/encryption, not read Bus::flash directly.
        let mut b = Bus::new(vec![0; 8]).unwrap();
        b.load_flash(&raw, 0x980f);
        for (address, value) in [
            (0x51020, 0x10),
            (0x50088, !0x780),
            (SPI, 0x4021),
            (0x50080, 0),
            (SPI + 8, 0x11),
            (SPI + 8, 0x3a),
            (0x50080, 0x100),
            (SPI + 8, 0x55),
            (0x50080, 0),
            (SPI + 8, 0x2c),
            (0x50080, 0x100),
            (SPI + 12, XIP + 1),
            (SPI + 16, 4),
        ] {
            b.write(address, value, 4).unwrap();
        }
        assert_eq!(&b.lcd.pixels[..2], &[0xff0000, 0x00ff00]);
        b.write(0x4020c, 0x4004, 4).unwrap();
        b.write(SPI + 16, 4, 4).unwrap();
        assert_eq!(&b.lcd.pixels[2..4], &[0x0000ff, 0xffffff]);
        b.write(0x40200, 0, 4).unwrap();
        assert!(b.write(SPI + 16, 4, 4).unwrap_err().reason.contains("XIP"));
        assert_eq!(b.lcd.pixels_written, 4);
    }
}

impl fmt::Display for AccessFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} bytes at 0x{:08x}: {}",
            self.operation, self.size, self.address, self.reason
        )
    }
}

pub struct Bus {
    pub flash: Vec<u8>,
    pub devices: Devices,
    pub lcd: Lcd,
    /// An SD card on the LCD's SPI1 (sd.rs: --sd / FM1_SD), none by default.
    pub sd: Option<crate::sd::SdCard>,
    pub system: crate::system::System,
    ram: Vec<u8>,
    guards: crate::guards::Guards,
    nor: crate::nor::Nor,
    pub usb: crate::usb::Usb,
    pub audio: crate::audio::Audio,
    cache: crate::cache::Cache,
    crc: crate::crc::Crc,
    clock: crate::clock::Clock,
    oscillator_ticks: u64,
    /// The oscillator tick the clocked devices have been advanced to. Between
    /// device events they only count, so they are advanced lazily: in one
    /// span when an event is due, a register is written, or a host asks.
    synced: u64,
    /// The first oscillator tick at which a device may do more than count:
    /// the instruction whose time reaches it advances devices exactly.
    next_event: u64,
    wireless: crate::wireless::Wireless,
    shift_spi: crate::shift_spi::ShiftSpi,
    /// What XIP reads of the application window return, byte by byte,
    /// rebuilt whenever anything they depend on changes (`Nor::xip_key`):
    /// reads stay live, but skip the SFC checks. None: take the full path
    /// (XIP disabled or busy, or an unaligned plain window).
    xip_view: Option<Box<[u8]>>,
    xip_key: Option<crate::nor::XipKey>,
    /// `device_clocks`, recomputed after every register write (the only
    /// way the guest changes a clock selection).
    clocks: (u32, u32, u32),
    /// Diagnostics: MMIO reads and writes by (address, PC), while counting.
    mmio_stats: std::cell::RefCell<Option<MmioStats>>,
    pub(crate) mmio_counting: bool,
    /// PC of the instruction executing, kept while counting MMIO accesses.
    pub(crate) pc_hint: std::cell::Cell<u32>,
    /// Instruction count of the instruction executing, kept like `pc_hint`.
    pub(crate) step_hint: std::cell::Cell<u64>,
    /// Diagnostics: an ordered log of MMIO accesses inside chosen ranges.
    mmio_trace: std::cell::RefCell<Option<MmioTrace>>,
    /// Diagnostics only (`set_mmio_stubs`): otherwise unmapped MMIO ranges
    /// that keep the last word written and read it back (0 before), with a
    /// count of accesses per word. Empty in normal use.
    mmio_stubs: Vec<(u32, u32, u32, u32)>,
    /// Diagnostics only: bytes the guest wrote to `stub_console`.
    pub stub_console: Option<(u32, Vec<u8>)>,
    stub_words: std::cell::RefCell<std::collections::BTreeMap<u32, (u32, u64)>>,
    perf: crate::perf::Perf,
    /// No interrupt source was pending at the last `irq_possible` and no
    /// device state has changed since: the bus clears it whenever its own
    /// methods change a device, the CPU at each public entry (callers may
    /// have changed devices through the public fields in between).
    irq_quiet: bool,
}

/// MMIO reads and writes per (address, PC of the accessing instruction).
pub type MmioStats = std::collections::BTreeMap<(u32, u32), [u64; 2]>;

/// One traced MMIO access, in execution order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioRecord {
    /// Instructions executed before this access (both cores).
    pub step: u64,
    /// Oscillator ticks (24 MHz) at the access.
    pub tick: u64,
    /// PC of the accessing instruction.
    pub pc: u32,
    /// True for a write.
    pub write: bool,
    pub address: u32,
    pub size: u8,
    /// Value written, or value returned by a successful read.
    pub value: u32,
    /// The access faulted (value is then meaningless).
    pub fault: bool,
}

/// Ordered MMIO access log for chosen address ranges (diagnostics).
#[derive(Debug, Default)]
pub struct MmioTrace {
    /// Inclusive address ranges to record.
    pub ranges: Vec<(u32, u32)>,
    /// Record reads too (writes are always recorded).
    pub reads: bool,
    /// Stop recording after this many records (the count keeps going).
    pub limit: usize,
    pub records: Vec<MmioRecord>,
    /// Accesses that matched, including those past `limit`.
    pub matched: u64,
}

impl MmioTrace {
    fn wants(&self, address: u32, write: bool) -> bool {
        (write || self.reads)
            && self
                .ranges
                .iter()
                .any(|&(low, high)| (low..=high).contains(&address))
    }
}

impl Bus {
    /// EMU_CON of `core`: exception enables (bit 2 divide by zero).
    pub(crate) fn emu_con(&self, core: usize) -> u32 {
        self.guards.emu_con(core)
    }
    pub(crate) fn raise_emu_msg(&mut self, core: usize, bits: u32) {
        self.guards.raise_emu_msg(core, bits);
    }
    pub(crate) fn core_control(&self, core: usize) -> u32 {
        self.cache.core_control(core)
    }
    /// Rebuild `xip_view` if what XIP reads depend on changed.
    fn refresh_xip_view(&mut self) {
        let key = self.nor.xip_key();
        if self.xip_key == Some(key) {
            return;
        }
        self.xip_key = Some(key);
        self.xip_view = None;
        if !self.nor.packaged() || !self.nor.plain_window_aligned() {
            return;
        }
        let view: Option<Box<[u8]>> = (0..self.flash.len() as u32)
            .map(|offset| match self.nor.xip(XIP + offset, 1) {
                Some(Ok(byte)) => Some(byte as u8),
                _ => None,
            })
            .collect();
        self.xip_view = view;
    }

    /// The serial NOR's raw contents (1 MiB, as on the chip).
    pub fn nor_bytes(&self) -> &[u8] {
        &self.nor.bytes
    }
    /// Page programs and sector erases the guest has completed.
    pub fn nor_writes(&self) -> u64 {
        self.nor.writes()
    }
    /// Offsets of the 4 KiB NOR sectors erased, programmed or restored.
    pub fn nor_tracked_sectors(&self) -> Vec<usize> {
        self.nor.tracked_sectors()
    }
    /// Lay a saved NOR sector over the flash through the same path an erase
    /// and program take (raw bytes, encrypted XIP view, XIP caches).
    pub fn restore_nor_sector(&mut self, offset: usize, data: &[u8]) -> Result<(), String> {
        self.nor.restore_sector(offset, data)?;
        self.refresh_xip_view();
        Ok(())
    }
    /// Let a NOR program or erase run for `ticks` (tests without a CPU).
    #[cfg(test)]
    pub(crate) fn advance_nor(&mut self, ticks: u32) {
        self.nor.advance(ticks);
        self.refresh_xip_view();
    }
    pub(crate) fn load_flash(&mut self, bytes: &[u8], key: u16) {
        self.nor.load(bytes, key);
        self.refresh_xip_view();
        // SPL handoff values measured before peripheral initialization.
        self.usb
            .write(0x10010, 0x10000, &mut self.ram)
            .unwrap()
            .unwrap();
        self.usb
            .write(0x51000, 0xe0c, &mut self.ram)
            .unwrap()
            .unwrap();
        self.audio.write(0x10014, 6).unwrap().unwrap();
        self.clocks = self.select_clocks();
    }
    pub fn new(flash: Vec<u8>) -> Result<Self, String> {
        if flash.is_empty() || flash.len() > (XIP_END - XIP) as usize {
            return Err("application image is empty or exceeds the XIP window".into());
        }
        let mut bus = Self {
            flash,
            devices: Devices::default(),
            lcd: Lcd::default(),
            sd: crate::sd::image_path()
                .map(|p| crate::sd::SdCard::open(&p))
                .transpose()?,
            system: Default::default(),
            ram: vec![0; RAM_SIZE],
            guards: Default::default(),
            nor: Default::default(),
            usb: Default::default(),
            audio: Default::default(),
            cache: Default::default(),
            crc: Default::default(),
            clock: Default::default(),
            oscillator_ticks: 0,
            synced: 0,
            next_event: 0,
            wireless: Default::default(),
            shift_spi: Default::default(),
            xip_view: None,
            xip_key: None,
            clocks: (0, 0, 0),
            mmio_stats: Default::default(),
            mmio_counting: false,
            pc_hint: Default::default(),
            step_hint: Default::default(),
            mmio_trace: Default::default(),
            mmio_stubs: Vec::new(),
            stub_console: None,
            stub_words: Default::default(),
            perf: Default::default(),
            irq_quiet: false,
        };
        bus.clocks = bus.select_clocks();
        Ok(bus)
    }

    /// Start counting MMIO reads and writes (diagnostics), from zero.
    pub fn start_mmio_stats(&mut self) {
        *self.mmio_stats.get_mut() = Some(MmioStats::new());
        self.mmio_counting = true;
    }

    /// Stop counting and return the counts, if counting was started.
    pub fn take_mmio_stats(&mut self) -> Option<MmioStats> {
        self.mmio_counting = self.mmio_trace.get_mut().is_some();
        self.mmio_stats.get_mut().take()
    }

    /// Start an ordered MMIO log of `ranges` (inclusive), at most `limit`
    /// records; `reads` adds reads with their returned values.
    pub fn start_mmio_trace(&mut self, ranges: Vec<(u32, u32)>, reads: bool, limit: usize) {
        *self.mmio_trace.get_mut() = Some(MmioTrace {
            ranges,
            reads,
            limit,
            ..Default::default()
        });
        self.mmio_counting = true;
    }

    /// Diagnostics only: let otherwise unmapped MMIO in `ranges` (inclusive)
    /// store and return words instead of faulting, to see how far a firmware
    /// for other hardware gets. Never used by the emulator itself.
    /// Each range is (low, high, mask): reads return the stored word with
    /// `mask` set (for example a transmitter-ready flag).
    pub fn set_mmio_stubs(&mut self, ranges: Vec<(u32, u32, u32)>) {
        self.mmio_stubs = ranges.into_iter().map(|(l, h, m)| (l, h, m, 0)).collect();
    }

    /// `set_mmio_stubs` with a fourth field per range: bits cleared on
    /// read (for example a start bit that a busy-wait polls back to 0).
    pub fn set_mmio_stubs_clearing(&mut self, ranges: Vec<(u32, u32, u32, u32)>) {
        self.mmio_stubs = ranges;
    }

    /// Stubbed words touched so far: (address, last value, accesses).
    pub fn mmio_stub_report(&self) -> Vec<(u32, u32, u64)> {
        self.stub_words
            .borrow()
            .iter()
            .map(|(&a, &(v, n))| (a, v, n))
            .collect()
    }

    fn stubbed(&self, address: u32) -> Option<(u32, u32)> {
        self.mmio_stubs
            .iter()
            .find(|&&(low, high, _, _)| (low..=high).contains(&address))
            .map(|&(_, _, set, clear)| (set, clear))
    }

    fn stub_read(&self, address: u32) -> Option<u32> {
        let (set, clear) = self.stubbed(address)?;
        let mut words = self.stub_words.borrow_mut();
        let entry = words.entry(address & !3).or_insert((0, 0));
        entry.1 += 1;
        Some(((entry.0 & !clear) | set) >> ((address & 3) * 8))
    }

    fn stub_write(&mut self, address: u32, value: u32) -> bool {
        if self.stubbed(address).is_none() {
            return false;
        }
        if let Some((console, bytes)) = &mut self.stub_console {
            if *console == address {
                bytes.push(value as u8);
            }
        }
        let entry = self
            .stub_words
            .get_mut()
            .entry(address & !3)
            .or_insert((0, 0));
        entry.0 = value << ((address & 3) * 8);
        entry.1 += 1;
        true
    }

    /// Stop the MMIO log and return it, if one was started.
    pub fn take_mmio_trace(&mut self) -> Option<MmioTrace> {
        self.mmio_counting = self.mmio_stats.get_mut().is_some();
        self.mmio_trace.get_mut().take()
    }

    #[cold]
    fn trace_mmio(&self, address: u32, size: usize, write: bool, result: Result<u32, ()>) {
        if let Some(trace) = self.mmio_trace.borrow_mut().as_mut() {
            if !trace.wants(address, write) {
                return;
            }
            trace.matched += 1;
            if trace.records.len() < trace.limit {
                trace.records.push(MmioRecord {
                    step: self.step_hint.get(),
                    tick: self.oscillator_ticks,
                    pc: self.pc_hint.get(),
                    write,
                    address,
                    size: size as u8,
                    value: result.unwrap_or(0),
                    fault: result.is_err(),
                });
            }
        }
    }

    #[inline(always)]
    fn count_mmio(&self, address: u32, kind: usize) {
        if self.mmio_counting {
            self.record_mmio(address, kind);
        }
    }

    #[cold]
    fn record_mmio(&self, address: u32, kind: usize) {
        if let Some(stats) = self.mmio_stats.borrow_mut().as_mut() {
            stats.entry((address, self.pc_hint.get())).or_default()[kind] += 1;
        }
    }

    /// Watchdog ticks since the last feed, as of the current tick.
    pub fn watchdog_ticks(&self) -> u64 {
        let elapsed = self.oscillator_ticks - self.synced;
        if self.system.watchdog_timeout().is_some() {
            self.system.watchdog_ticks + elapsed
        } else {
            self.system.watchdog_ticks
        }
    }

    fn fault(
        address: u32,
        size: usize,
        operation: &'static str,
        reason: &'static str,
    ) -> AccessFault {
        AccessFault {
            address,
            size,
            operation,
            reason,
        }
    }

    fn check(address: u32, size: usize, operation: &'static str) -> Result<(), AccessFault> {
        if !matches!(size, 1 | 2 | 4) {
            return Err(Self::fault(
                address,
                size,
                operation,
                "unsupported access width",
            ));
        }
        if !address.is_multiple_of(size as u32) {
            return Err(Self::fault(address, size, operation, "unaligned access"));
        }
        Ok(())
    }

    fn offset(address: u32, size: usize, base: u32, length: usize) -> Option<usize> {
        let offset = address.checked_sub(base)? as usize;
        (offset.checked_add(size)? <= length).then_some(offset)
    }

    /// Little-endian value of `size` (1, 2 or 4) bytes at `offset`; the
    /// caller has checked the range.
    #[inline(always)]
    fn value_at(bytes: &[u8], offset: usize, size: usize) -> u32 {
        match size {
            4 => u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()),
            2 => u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as u32,
            _ => bytes[offset] as u32,
        }
    }

    /// An aligned read of SRAM or the application XIP view (instruction
    /// fetches and most data), inline; `None`: take the full path.
    #[inline(always)]
    pub(crate) fn read_fast(&self, address: u32, size: usize) -> Option<u32> {
        if matches!(size, 1 | 2 | 4) && address.is_multiple_of(size as u32) {
            // Aligned accesses of at most 4 bytes never cross either end.
            let ram_offset = address.wrapping_sub(RAM) as usize;
            if ram_offset < RAM_SIZE {
                return Some(Self::value_at(&self.ram, ram_offset, size));
            }
            if let Some(view) = &self.xip_view {
                let offset = address.wrapping_sub(XIP) as usize;
                if offset + size <= view.len() {
                    return Some(Self::value_at(view, offset, size));
                }
            }
        }
        None
    }

    /// `read_fast`, everything else in `read_slow`.
    #[inline(always)]
    fn read_as(
        &self,
        address: u32,
        size: usize,
        operation: &'static str,
    ) -> Result<u32, AccessFault> {
        match self.read_fast(address, size) {
            Some(value) => Ok(value),
            None => self.read_slow(address, size, operation),
        }
    }

    #[inline(never)]
    fn read_slow(
        &self,
        address: u32,
        size: usize,
        operation: &'static str,
    ) -> Result<u32, AccessFault> {
        let result = self.read_slow_inner(address, size, operation);
        if self.mmio_counting && operation == "read" {
            self.trace_mmio(
                address,
                size,
                false,
                result.as_ref().map(|v| *v).map_err(|_| ()),
            );
        }
        result
    }

    fn read_slow_inner(
        &self,
        address: u32,
        size: usize,
        operation: &'static str,
    ) -> Result<u32, AccessFault> {
        Self::check(address, size, operation)?;
        let ram_offset = address.wrapping_sub(RAM) as usize;
        if ram_offset < RAM_SIZE {
            // Aligned accesses of at most 4 bytes never cross the end.
            let ram = &self.ram;
            return Ok(match size {
                4 => u32::from_le_bytes(ram[ram_offset..ram_offset + 4].try_into().unwrap()),
                2 => u16::from_le_bytes(ram[ram_offset..ram_offset + 2].try_into().unwrap()) as u32,
                _ => ram[ram_offset] as u32,
            });
        }
        if let Some(view) = &self.xip_view {
            let offset = address.wrapping_sub(XIP) as usize;
            if offset + size <= view.len() {
                return Ok(match size {
                    4 => u32::from_le_bytes(view[offset..offset + 4].try_into().unwrap()),
                    2 => u16::from_le_bytes(view[offset..offset + 2].try_into().unwrap()) as u32,
                    _ => view[offset] as u32,
                });
            }
        }
        let bytes = if let Some(offset) = Self::offset(address, size, XIP, self.flash.len()) {
            if !self.nor.xip_active() {
                return Err(Self::fault(
                    address,
                    size,
                    operation,
                    "application XIP is disabled",
                ));
            }
            if self.nor.packaged() {
                return self
                    .nor
                    .xip(address, size)
                    .unwrap()
                    .map_err(|reason| Self::fault(address, size, operation, reason));
            }
            &self.flash[offset..offset + size]
        } else if let Some(offset) = Self::offset(address, size, RAM, self.ram.len()) {
            &self.ram[offset..offset + size]
        } else {
            self.count_mmio(address, 0);
            if let Some(value) = self.shift_spi.read(address & !3) {
                return if size == 4 {
                    Ok(value)
                } else {
                    Err(Self::fault(
                        address,
                        size,
                        operation,
                        "SPI registers require word accesses",
                    ))
                };
            }
            if let Some(result) = self.wireless.read(address, size) {
                return result.map_err(|reason| Self::fault(address, size, operation, reason));
            }
            if let Some(value) = self.clock.read(address) {
                return Ok(value);
            }
            if let Some(value) = self.crc.read(address) {
                return Ok(value);
            }
            if let Some(value) = self.cache.read(address, size) {
                return Ok(value);
            }
            if let Some(value) = self.audio.read(address) {
                return Ok(value);
            }
            if let Some(value) = self.nor.xip(address, size) {
                return value.map_err(|reason| Self::fault(address, size, operation, reason));
            }
            if let Some(value) = self.usb.read(address) {
                return Ok(value);
            }
            if let Some(value) = self.nor.read(address) {
                return Ok(value);
            }
            if size == 4 {
                if let Some(value) = self.perf.read(address, self.clock.cycles()) {
                    return Ok(value);
                }
            }
            if let Some(value) = self.guards.read(address) {
                return Ok(value);
            }
            if let Some(value) = self.system.read(address) {
                return Ok(value);
            }
            if let Some(value) = self.lcd.read(address & !3) {
                return if size == 4 {
                    Ok(value)
                } else {
                    Err(Self::fault(
                        address,
                        size,
                        operation,
                        "SPI registers require word accesses",
                    ))
                };
            }
            let (peripheral_hz, core_hz, _) = self.device_clocks();
            if let Some(value) = self.devices.read_at(
                address,
                size,
                self.oscillator_ticks - self.synced,
                peripheral_hz,
                core_hz,
            ) {
                return value
                    .map(|value| {
                        if address == crate::devices::IRQ_PENDING && self.audio.pending_irq() {
                            value | (1 << crate::audio::IRQ)
                        } else {
                            value
                        }
                    })
                    .map_err(|reason| Self::fault(address, size, operation, reason));
            }
            if let Some(value) = self.stub_read(address) {
                return Ok(value);
            }
            // No generic zero-filled MMIO: missing peripherals must be visible.
            return Err(Self::fault(
                address,
                size,
                operation,
                "unmapped memory or unimplemented MMIO",
            ));
        };
        Ok(bytes
            .iter()
            .enumerate()
            .fold(0, |value, (i, byte)| value | ((*byte as u32) << (i * 8))))
    }

    #[inline]
    pub fn read(&self, address: u32, size: usize) -> Result<u32, AccessFault> {
        self.read_as(address, size, "read")
    }

    #[inline]
    pub fn fetch(&self, address: u32) -> Result<u16, AccessFault> {
        // Startup later copies .ram_text here; instructions can execute from RAM.
        self.read_as(address, 2, "fetch").map(|value| value as u16)
    }

    /// An aligned SRAM store that write protection allows (most data),
    /// inline; false: nothing was written, take the full path.
    #[inline(always)]
    pub(crate) fn write_fast(&mut self, address: u32, value: u32, size: usize) -> bool {
        if matches!(size, 1 | 2 | 4) && address.is_multiple_of(size as u32) {
            // Aligned accesses of at most 4 bytes never cross the end.
            let offset = address.wrapping_sub(RAM) as usize;
            if offset < RAM_SIZE && self.guards.check_write(address, size).is_ok() {
                match size {
                    4 => self.ram[offset..offset + 4].copy_from_slice(&value.to_le_bytes()),
                    2 => {
                        self.ram[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes())
                    }
                    _ => self.ram[offset] = value as u8,
                }
                return true;
            }
        }
        false
    }

    /// `write_fast`, everything else (and a protection fault) in `write_slow`.
    #[inline(always)]
    pub fn write(&mut self, address: u32, value: u32, size: usize) -> Result<(), AccessFault> {
        if self.write_fast(address, value, size) {
            return Ok(());
        }
        self.write_slow(address, value, size)
    }

    #[inline(never)]
    fn write_slow(&mut self, address: u32, value: u32, size: usize) -> Result<(), AccessFault> {
        Self::check(address, size, "write")?;
        if let Some(offset) = Self::offset(address, size, RAM, self.ram.len()) {
            self.guards
                .check_write(address, size)
                .map_err(|reason| Self::fault(address, size, "write", reason))?;
            self.ram[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
            return Ok(());
        }
        self.sync()?;
        let result = self.write_mmio(address, value, size);
        if self.mmio_counting {
            self.trace_mmio(
                address,
                size,
                true,
                result.as_ref().map(|_| value).map_err(|_| ()),
            );
        }
        self.clocks = self.select_clocks();
        self.next_event = self.oscillator_ticks + 1;
        self.irq_quiet = false;
        self.refresh_xip_view();
        result
    }

    fn write_mmio(&mut self, address: u32, value: u32, size: usize) -> Result<(), AccessFault> {
        self.count_mmio(address, 1);
        if let Some(result) = self.devices.write_uart(address, value, size, &self.ram) {
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if self.shift_spi.read(address & !3).is_some() {
            if size != 4 {
                return Err(Self::fault(
                    address,
                    size,
                    "write",
                    "SPI registers require word accesses",
                ));
            }
            if self.shift_spi.write(address, value) {
                let single = [value as u8];
                let bytes = if address == crate::shift_spi::BASE + 8 {
                    &single[..]
                } else {
                    let source = self.shift_spi.dma_address();
                    let length = self.shift_spi.dma_length();
                    let offset =
                        Self::offset(source, length, RAM, self.ram.len()).ok_or_else(|| {
                            Self::fault(
                                source,
                                length,
                                "DMA read",
                                "matrix SPI DMA source must be in SRAM",
                            )
                        })?;
                    &self.ram[offset..offset + length]
                };
                let iomap = self.lcd.read(crate::lcd::IOMAP).unwrap();
                self.shift_spi
                    .start(bytes, iomap)
                    .map_err(|reason| Self::fault(address, size, "write", reason))?;
            }
            return Ok(());
        }
        if let Some(result) = self.wireless.write(address, value, size) {
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if self.clock.write(address, value).is_some() {
            return Ok(());
        }
        if self.crc.write(address, value).is_some() {
            return Ok(());
        }
        if let Some(result) = self.audio.write(address, value) {
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if let Some(result) = self.usb.write(address, value, &mut self.ram) {
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if address == 0x500c0 {
            self.nor.chip_select(value & 1 == 0);
        }
        if let Some(result) = self.nor.write(address, value) {
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        self.guards
            .check_write(address, size)
            .map_err(|reason| Self::fault(address, size, "write", reason))?;
        if self.cache.write(address, size, value).is_some() {
            return Ok(());
        }
        if size == 4
            && self
                .perf
                .write(address, value, self.clock.cycles())
                .is_some()
        {
            return Ok(());
        }
        if address == crate::perf::DBG_CON {
            self.perf.control(value, self.clock.cycles());
        }
        if let Some(result) = self.guards.write(address, value) {
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if let Some(result) = self.system.write(address, value) {
            self.devices.adc.select_pmu(self.system.adc_pmu_selection());
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if self.lcd.read(address & !3).is_some() {
            if size != 4 {
                return Err(Self::fault(
                    address,
                    size,
                    "write",
                    "SPI registers require word accesses",
                ));
            }
            if self.sd_transfer(address, value)? {
                return Ok(());
            }
            let bytes = if address == SPI + 16 {
                let source = self.lcd.dma_address();
                let length = value as usize;
                if let Some(offset) = Self::offset(source, length, RAM, self.ram.len()) {
                    self.ram[offset..offset + length].to_vec()
                } else if Self::offset(source, length, XIP, self.flash.len()).is_some() {
                    // Stock LCD initialization sends constant data from flash.
                    // Read bytes through XIP so packaged encryption/mapping,
                    // flash modifications and disabled-XIP faults still apply.
                    (0..length)
                        .map(|i| {
                            self.read_as(source + i as u32, 1, "DMA read")
                                .map(|v| v as u8)
                        })
                        .collect::<Result<Vec<_>, _>>()?
                } else {
                    return Err(Self::fault(
                        source,
                        length,
                        "DMA read",
                        "LCD DMA source is outside SRAM and application XIP",
                    ));
                }
            } else {
                Vec::new()
            };
            let pc_out = self.devices.gpio.read(0x50080).unwrap();
            let pc_dir = self.devices.gpio.read(0x50088).unwrap();
            let selected = pc_dir & 0x180 == 0 && pc_out & 0x80 == 0;
            return self
                .lcd
                .write(address, value, selected, pc_out & 0x100 != 0, &bytes)
                .map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if let Some(result) = self.devices.write(address, value, size) {
            return result.map_err(|reason| Self::fault(address, size, "write", reason));
        }
        if self.stub_write(address, value) {
            return Ok(());
        }
        let offset = Self::offset(address, size, RAM, self.ram.len()).ok_or_else(|| {
            Self::fault(
                address,
                size,
                "write",
                "unmapped memory, read-only XIP, or unimplemented MMIO",
            )
        })?;
        self.ram[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
        Ok(())
    }

    /// SPI1 in an SD session (CON b3, fm1_sd_bus.h): a BUF write clocks one byte through the card (its answer in BUF),
    /// a CNT write n bytes by DMA, received into RAM (CON b12) or sent from it. The card is selected while PC7 (the
    /// LCD's CS, through the SD board's inverter) is driven high; bytes clocked while it is not go to the LCD as data
    /// after the NOP the firmware sends first, which the panel ignores: dropped here. false: not the card's transfer.
    fn sd_transfer(&mut self, address: u32, value: u32) -> Result<bool, AccessFault> {
        let con = self.lcd.read(SPI).unwrap();
        if self.sd.is_none() || con & 8 == 0 || (address != SPI + 8 && address != SPI + 16) {
            return Ok(false);
        }
        let pc_out = self.devices.gpio.read(0x50080).unwrap();
        let pc_dir = self.devices.gpio.read(0x50088).unwrap();
        let on = pc_dir & 0x80 == 0 && pc_out & 0x80 != 0;
        let sd = self.sd.as_mut().unwrap();
        sd.select(on);
        if address == SPI + 8 {
            let r = if on { sd.xchg(value as u8) } else { 0xFF };
            self.lcd.sd_store(address, r as u32);
            return Ok(true);
        }
        let at = self.lcd.dma_address();
        let n = value as usize;
        let offset = Self::offset(at, n, RAM, self.ram.len())
            .ok_or_else(|| Self::fault(at, n, "DMA", "SD DMA buffer must be in SRAM"))?;
        for i in 0..n {
            if con & 0x1000 != 0 {
                self.ram[offset + i] = if on { sd.xchg(0xFF) } else { 0xFF };
            } else if on {
                sd.xchg(self.ram[offset + i]);
            }
        }
        self.lcd.sd_store(address, value);
        Ok(true)
    }

    /// The peripheral (LSB), core and UART clocks the guest selected.
    #[inline]
    fn device_clocks(&self) -> (u32, u32, u32) {
        self.clocks
    }

    fn select_clocks(&self) -> (u32, u32, u32) {
        let clk_con3 = self.audio.read(0x10014).unwrap();
        let peripheral_hz = self.clock.timer_hz(clk_con3);
        let core_hz = self.clock.system_hz(clk_con3);
        // spec_uart.c: CLK_CON2[11:10] selects OSC, PLL48M, or LSB.
        let uart_hz = match (self.usb.read(0x10010).unwrap() >> 10) & 3 {
            0 => 24_000_000,
            1 => 48_000_000,
            2 => peripheral_hz,
            _ => 0,
        };
        (peripheral_hz, core_hz, uart_hz)
    }

    fn advance_devices(&mut self, ticks: u32) {
        let (peripheral_hz, core_hz, uart_hz) = self.device_clocks();
        self.devices
            .advance_with_clocks(ticks, peripheral_hz, core_hz);
        self.devices.advance_uart(ticks, uart_hz, &mut self.ram);
        if let Some(bytes) = self.shift_spi.advance(ticks, peripheral_hz) {
            self.devices.gpio.shift_spi(&bytes);
        }
    }

    /// Advance every clocked device by `ticks` oscillator ticks.
    fn advance_clocked(&mut self, ticks: u32) -> Result<(), AccessFault> {
        self.irq_quiet = false;
        self.advance_devices(ticks);
        self.nor.advance(ticks);
        self.refresh_xip_view();
        self.wireless
            .advance(ticks, &mut self.ram)
            .map_err(|reason| Self::fault(0x28000, 4, "BLE engine", reason))?;
        self.system
            .advance(ticks)
            .map_err(|reason| Self::fault(0x13e08, 4, "watchdog", reason))?;
        self.advance_usb(ticks)?;
        self.advance_audio(ticks)
    }

    /// Bring the clocked devices up to tick `to` in one span. Only valid
    /// while no device event lies in between (`to < next_event`): each
    /// device's `advance(n)` then equals n single ticks.
    fn catch_up(&mut self, to: u64) -> Result<(), AccessFault> {
        let span = to - self.synced;
        if span == 0 {
            return Ok(());
        }
        self.synced = to;
        self.advance_clocked(u32::try_from(span).expect("device spans are bounded"))
    }

    /// Bring the clocked devices up to the current tick (host inspection of
    /// device state; register writes do this themselves).
    pub fn sync(&mut self) -> Result<(), AccessFault> {
        self.catch_up(self.oscillator_ticks)
    }

    /// Ticks from `synced` to the next device event, capped so that a
    /// catch-up span fits the devices' tick arguments.
    fn ticks_to_event(&self) -> u64 {
        let (peripheral_hz, core_hz, uart_hz) = self.device_clocks();
        [
            self.devices.ticks_to_event(peripheral_hz, core_hz, uart_hz),
            self.shift_spi.ticks_to_event(peripheral_hz),
            self.nor.ticks_to_event(),
            self.wireless.ticks_to_event(),
            self.system.ticks_to_event(),
            self.usb.ticks_to_event(),
            self.audio.ticks_to_event(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(u64::MAX)
        .min(1 << 30)
    }

    /// Device time after an instruction that took `ticks` oscillator ticks.
    /// Until the next device event devices only count, so nothing runs; the
    /// instruction that reaches the event brings them up to date and then
    /// advances its own ticks exactly, as every instruction once did.
    #[inline(always)]
    pub(crate) fn advance_time(&mut self, ticks: u32) -> Result<(), AccessFault> {
        if self.oscillator_ticks < self.next_event {
            return Ok(());
        }
        self.advance_event(ticks)
    }

    #[inline(never)]
    fn advance_event(&mut self, ticks: u32) -> Result<(), AccessFault> {
        let now = self.oscillator_ticks;
        self.catch_up(now - ticks as u64)?;
        self.synced = now;
        // Should this tick fault, the next one runs exactly too.
        self.next_event = now + 1;
        self.advance_clocked(ticks)?;
        self.next_event = now + self.ticks_to_event();
        Ok(())
    }

    /// The BLE baseband engine model (state, statistics, air log).
    pub fn ble_engine(&self) -> &crate::ble_engine::Engine {
        &self.wireless.ble
    }
    /// The engine, for configuration; event times are recomputed.
    pub fn ble_engine_mut(&mut self) -> &mut crate::ble_engine::Engine {
        self.irq_quiet = false;
        self.next_event = self.next_event.min(self.oscillator_ticks + 1);
        &mut self.wireless.ble
    }
    /// Put a virtual central on the virtual air (replacing any other).
    pub fn ble_attach_central(&mut self, central: crate::ble_central::Central) {
        self.ble_engine_mut().attach_central(central);
    }
    pub fn ble_central(&self) -> Option<&crate::ble_central::Central> {
        self.wireless.ble.central()
    }
    /// The central, to queue steps; event times are recomputed.
    pub fn ble_central_mut(&mut self) -> Option<&mut crate::ble_central::Central> {
        self.ble_engine_mut().central_mut()
    }

    /// Put MIDI bytes on the UART1 RX line (the FM-1's DIN/TRS MIDI IN):
    /// they arrive back to back at 31250 baud from the next tick.
    pub fn uart_midi_send(&mut self, bytes: &[u8]) {
        self.irq_quiet = false;
        self.next_event = self.next_event.min(self.oscillator_ticks + 1);
        self.devices.uart_mut().receive(bytes);
    }

    /// UART1 receive line: bytes still waiting, received, lost.
    pub fn uart_rx_counts(&self) -> (usize, u64, u64) {
        self.devices.uart().rx_counts()
    }

    /// Bytes UART1 finished transmitting (MIDI OUT; the first 4096).
    pub fn uart_transmitted(&self) -> &[u8] {
        self.devices.uart().transmitted()
    }

    /// Queue terminal bytes for the guest's CDC OUT endpoint (see
    /// `Usb::receive_serial`); the next tick delivers them.
    pub fn receive_serial(&mut self, bytes: &[u8]) -> bool {
        self.irq_quiet = false;
        self.next_event = self.next_event.min(self.oscillator_ticks + 1);
        self.usb.receive_serial(bytes)
    }
    /// Issue one instruction per `hz` of guest time instead of following the
    /// firmware's system clock; `None` restores the firmware clock. Timers,
    /// DMA, USB and the watchdog keep their own clocks, so a lower rate gives
    /// the guest fewer instructions per second of guest time.
    pub fn set_instruction_clock(&mut self, hz: Option<u32>) {
        self.clock.issue_override = hz;
    }

    pub(crate) fn instruction_ticks(&mut self) -> u32 {
        let ticks = self
            .clock
            .instruction_ticks(self.audio.read(0x10014).unwrap());
        self.oscillator_ticks += ticks as u64;
        ticks
    }

    /// The most instructions (of the core carrying guest time) that can
    /// issue before the next device event; the instruction reaching the
    /// event is left to the normal path.
    pub(crate) fn issues_before_event(&mut self) -> u64 {
        let Some(room) = self.next_event.checked_sub(self.oscillator_ticks + 1) else {
            return 0;
        };
        let clk_con3 = self.audio.read(0x10014).unwrap();
        self.clock.issues_within(clk_con3, room)
    }

    /// The oscillator tick after `count` more instruction issues; valid right
    /// after `issues_before_event` (which brings the issue clock up to date).
    pub(crate) fn tick_after_issues(&self, count: u64) -> u64 {
        self.oscillator_ticks + self.clock.ticks_after(count)
    }

    /// The tick the clocked devices have been advanced to.
    pub(crate) fn synced(&self) -> u64 {
        self.synced
    }

    /// The peripheral (LSB) clock the guest selected, which TIMER4/5 count.
    pub(crate) fn peripheral_hz(&self) -> u32 {
        self.clocks.0
    }

    /// A register write makes the next tick exact only to recompute the
    /// device event times (`write`): recompute them now instead. They are
    /// the same times, since no device has an event before `next_event`.
    pub(crate) fn reschedule(&mut self) -> Result<(), AccessFault> {
        if self.next_event == self.oscillator_ticks + 1 {
            self.sync()?;
            self.next_event = self.oscillator_ticks + self.ticks_to_event().max(1);
        }
        Ok(())
    }

    /// Bring the clocked devices up to tick `to` (at most the current tick,
    /// with no device event in between).
    pub(crate) fn sync_to(&mut self, to: u64) {
        self.catch_up(to)
            .expect("no device has an event before next_event");
    }

    /// What a device register write leaves behind (`write`), for registers
    /// written other than through it: clocks and event times recomputed.
    pub(crate) fn registers_written(&mut self) {
        self.clocks = self.select_clocks();
        self.next_event = self.oscillator_ticks + 1;
        self.irq_quiet = false;
        self.refresh_xip_view();
    }

    /// Account `count` instructions in which nothing but time passes, all
    /// before the next device event (`issues_before_event`).
    pub(crate) fn skip_issues(&mut self, count: u64) {
        let clk_con3 = self.audio.read(0x10014).unwrap();
        self.oscillator_ticks += self.clock.issue(clk_con3, count);
        debug_assert!(self.oscillator_ticks < self.next_event);
    }

    /// Device state may have changed other than through the bus's methods
    /// (the public device fields): `irq_possible` checks again.
    pub(crate) fn devices_changed(&mut self) {
        self.irq_quiet = false;
    }

    /// `any_irq_pending`, without checking again while no device state
    /// changed since a check found nothing pending.
    #[inline(always)]
    pub(crate) fn irq_possible(&mut self) -> bool {
        !self.irq_quiet && self.irq_check()
    }

    #[inline(never)]
    fn irq_check(&mut self) -> bool {
        let pending = self.any_irq_pending();
        self.irq_quiet = !pending;
        pending
    }

    /// Whether any interrupt source is pending for either core.
    #[inline]
    pub(crate) fn any_irq_pending(&self) -> bool {
        self.devices.pending_sources(0) != 0
            || self.devices.pending_sources(1) != 0
            || self.lcd.pending_irq()
            || self.audio.pending_irq()
            || self.shift_spi.pending_irq()
            || self.wireless.clock_pending_irq()
            || self.wireless.slot_pending_irq()
            || self.wireless.ble.event_irq()
            || self.wireless.ble.rx_irq()
    }

    /// 24 MHz oscillator ticks of guest time so far.
    pub fn oscillator_ticks(&self) -> u64 {
        self.oscillator_ticks
    }

    /// Queue USB-MIDI packets for the firmware (host to device). They are
    /// delivered once the MIDI host has configured the device, one bulk
    /// packet (up to 16 events) whenever its OUT endpoint buffer is free.
    pub fn usb_midi_send(&mut self, packets: &[crate::usb_midi::Packet]) {
        self.irq_quiet = false;
        self.next_event = self.next_event.min(self.oscillator_ticks + 1);
        self.usb.midi_send(packets);
    }

    pub fn advance_usb(&mut self, ticks: u32) -> Result<(), AccessFault> {
        self.irq_quiet = false;
        self.usb
            .advance(ticks, &mut self.ram)
            .map_err(|reason| Self::fault(0x11800, 4, "USB host", reason))
    }

    pub fn screen_visible(&self) -> bool {
        self.lcd.display_on
            && !self.lcd.sleeping
            && self.devices.gpio.read(0x50000).unwrap() & 4 == 0
            && self.devices.gpio.read(0x50008).unwrap() & 4 == 0
    }

    pub fn advance_audio(&mut self, ticks: u32) -> Result<(), AccessFault> {
        self.irq_quiet = false;
        // A frame transfers only on a device event, whose tick is `synced`.
        self.audio
            .advance(ticks, &self.ram, self.synced)
            .map_err(|reason| Self::fault(0x12e1c, 4, "audio DMA", reason))
    }

    pub fn pending_irq(&self, icfg: u32) -> Option<usize> {
        self.pending_irq_for(icfg, 0)
    }
    pub(crate) fn pending_irq_for(&self, icfg: u32, core: usize) -> Option<usize> {
        let sources = self.devices.pending_sources(core)
            | ((self.lcd.pending_irq() as u128) << crate::lcd::IRQ)
            | ((self.audio.pending_irq() as u128) << crate::audio::IRQ)
            | ((self.shift_spi.pending_irq() as u128) << crate::shift_spi::IRQ)
            | ((self.wireless.clock_pending_irq() as u128) << 40)
            | ((self.wireless.slot_pending_irq() as u128) << 41)
            | ((self.wireless.ble.rx_irq() as u128) << crate::ble_engine::RX_IRQ)
            | ((self.wireless.ble.event_irq() as u128) << crate::ble_engine::EVENT_IRQ);
        self.devices.select_irq(sources, icfg, core)
    }
}
