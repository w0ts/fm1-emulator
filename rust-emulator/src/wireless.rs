// SPDX-License-Identifier: GPL-3.0-only
// WL82.h: WL/WF analog configuration latches. Stock wfhw_init also configures
// two words at 0x30f00; their bit meanings are not published in that header.
// RF transmission, reception,
// LO frequency counting and serial-data DMA are not modeled. The PLL
// comparator uses measured nominal thresholds for this FM-1.
// Explicitly observed setup words in analog initialization and the stock
// controller initializer at 0x02072326. Packet state and event registers are
// deliberately excluded; a configuration latch does not simulate a radio.
const BT_CONFIGURATION: &[u32] = &[
    0x20000, 0x2000c, 0x20018, 0x2002c, 0x20058, 0x2005c, 0x2007c, 0x20080, 0x20084, 0x200c0,
    0x200f0, 0x200f4, 0x20120, 0x20124, 0x20128, 0x2012c, 0x20130, 0x20134, 0x20138, 0x20150,
    0x20154, 0x20158, 0x2015c, 0x20160, 0x20164, 0x20168, 0x2fc00, 0x2fc04, 0x2fc08, 0x2fc0c,
    0x2fc10, 0x2fc14, 0x2fc18, 0x2fc1c, 0x2fc20, 0x2fc24, 0x2fc28, 0x2fc40, 0x2fc44, 0x2fc48,
    0x2fc70, 0x2fc78, 0x2fc7c, 0x2fc80, 0x2fc84, 0x2fc88, 0x2fc98, 0x2fc9c, 0x2fca0, 0x2fcbc,
    0x2fd40, 0x2fd80, 0x2fd84, 0x2fd88, 0x2fd8c, 0x2fd90, 0x2fd94, 0x2fd98, 0x2fd9c,
];

pub(crate) struct Wireless {
    registers: [u32; 26],
    radio_configuration: [u32; 2],
    mac: [u32; 10],
    bbp_command: u32,
    bbp: [u8; 256],
    analog: [u32; 31],
    sample_strobes: u8,
    sample_result: u32,
    filter_ticks: u32,
    filter_result: u8,
    bt_configuration: [u32; BT_CONFIGURATION.len()],
    bt_table: [u32; 128],
    bt_table_position: usize,
    /// The BLE baseband engine model (0x28000-0x28054, ble_engine.rs).
    pub(crate) ble: crate::ble_engine::Engine,
    bt_clock_ticks: u64,
    bt_sample_ticks: u32,
    bt_clock_sample: u32,
    bt_fine_sample: u32,
    bt_alarm: u32,
    bt_clock_pending: bool,
    slot_alarms: [u32; 8],
    slot_enabled: u8,
    slot_pending: u8,
}
impl Default for Wireless {
    fn default() -> Self {
        Self {
            registers: [0; 26],
            radio_configuration: [0; 2],
            mac: [0; 10],
            bbp_command: 0,
            bbp: [0; 256],
            analog: [0; 31],
            sample_strobes: 0,
            sample_result: 0,
            filter_ticks: 0,
            filter_result: 0,
            bt_configuration: [0; BT_CONFIGURATION.len()],
            bt_table: [0; 128],
            bt_table_position: 0,
            ble: Default::default(),
            bt_clock_ticks: 0,
            bt_sample_ticks: 0,
            bt_clock_sample: 0,
            bt_fine_sample: 0,
            bt_alarm: 0,
            bt_clock_pending: false,
            slot_alarms: [0; 8],
            slot_enabled: 0,
            slot_pending: 0,
        }
    }
}
impl Wireless {
    // Vendor analog.c and the stock RF initialization routine. These are
    // configuration words; controller packet scheduling is still unsupported.
    fn bt_index(address: u32) -> Option<usize> {
        BT_CONFIGURATION.iter().position(|a| *a == address)
    }
    /// Index of a configuration word known to be in `BT_CONFIGURATION`,
    /// evaluated at compile time (the interrupt check runs every step).
    const fn bt_slot(address: u32) -> usize {
        let mut i = 0;
        while BT_CONFIGURATION[i] != address {
            i += 1;
        }
        i
    }
    const CLOCK_CONTROL: usize = Self::bt_slot(0x20000);
    const CLOCK_EVENTS: usize = Self::bt_slot(0x2000c);
    pub(crate) fn advance(&mut self, ticks: u32, ram: &mut [u8]) -> Result<(), &'static str> {
        self.ble.advance(ticks, ram)?;
        self.advance_radio(ticks);
        Ok(())
    }
    fn advance_radio(&mut self, ticks: u32) {
        self.filter_ticks = self.filter_ticks.saturating_sub(ticks);
        // Vendor bredr_frame.c uses full 625 us slots and fine microseconds;
        // slot_timer wraps at 27 bits. Functional nominal crystal timing,
        // not a measured RF clock/power/reset model.
        const SLOT: u64 = 15_000; // 24 MHz * 625 us.
        const WRAP: u64 = SLOT * (1 << 27);
        if self.bt_configuration[Self::CLOCK_CONTROL] & 1 != 0 {
            let old = self.bt_clock_ticks;
            let distance = ((self.bt_alarm as u64).wrapping_sub(old / SLOT)) & ((1 << 27) - 1);
            let until = if distance == 0 { WRAP } else { distance * SLOT } - old % SLOT;
            if self.bt_configuration[Self::CLOCK_EVENTS] & 512 != 0 && ticks as u64 >= until {
                self.bt_clock_pending = true;
            }
            self.bt_clock_ticks = (old + ticks as u64) % WRAP;
            for (channel, alarm) in self.slot_alarms.iter().enumerate() {
                let distance = ((*alarm as u64).wrapping_sub(old / SLOT)) & ((1 << 27) - 1);
                let until = if distance == 0 { WRAP } else { distance * SLOT } - old % SLOT;
                if self.slot_enabled & (1 << channel) != 0 && ticks as u64 >= until {
                    self.slot_pending |= 1 << channel;
                }
            }
        }
        if self.bt_sample_ticks != 0 {
            self.bt_sample_ticks = self.bt_sample_ticks.saturating_sub(ticks);
            if self.bt_sample_ticks == 0 {
                self.bt_clock_sample = (self.bt_clock_ticks / SLOT) as u32;
                self.bt_fine_sample = ((self.bt_clock_ticks % SLOT) / 24) as u32;
            }
        }
    }
    /// Oscillator ticks until `advance` next changes something readable or
    /// an interrupt: a requested clock sample, an enabled clock or slot
    /// alarm. The calibration filter's countdown is only consulted by
    /// register writes, which bring devices up to date first.
    pub(crate) fn ticks_to_event(&self) -> Option<u64> {
        const SLOT: u64 = 15_000;
        const WRAP: u64 = SLOT * (1 << 27);
        let mut next = (self.bt_sample_ticks != 0).then_some(self.bt_sample_ticks as u64);
        if let Some(ble) = self.ble.ticks_to_event() {
            next = Some(next.map_or(ble, |n| n.min(ble)));
        }
        if self.bt_configuration[Self::CLOCK_CONTROL] & 1 != 0 {
            let old = self.bt_clock_ticks;
            let until = |alarm: u32| {
                let distance = ((alarm as u64).wrapping_sub(old / SLOT)) & ((1 << 27) - 1);
                (if distance == 0 { WRAP } else { distance * SLOT } - old % SLOT).max(1)
            };
            if self.bt_configuration[Self::CLOCK_EVENTS] & 512 != 0 && !self.bt_clock_pending {
                next = Some(next.map_or(until(self.bt_alarm), |n| n.min(until(self.bt_alarm))));
            }
            for (channel, alarm) in self.slot_alarms.iter().enumerate() {
                if self.slot_enabled & (1 << channel) != 0
                    && self.slot_pending & (1 << channel) == 0
                {
                    next = Some(next.map_or(until(*alarm), |n| n.min(until(*alarm))));
                }
            }
        }
        next
    }
    pub(crate) fn clock_pending_irq(&self) -> bool {
        self.bt_clock_pending && self.bt_configuration[Self::CLOCK_EVENTS] & 512 != 0
    }
    pub(crate) fn slot_pending_irq(&self) -> bool {
        self.slot_pending & self.slot_enabled != 0
    }
    // Vendor wf_phy_mac_init/wl_hw_init setup words, reached through
    // wl30_mmc_io_rw_extended's direct MAC mapping (0x30000 + offset).
    fn mac_index(address: u32) -> Option<usize> {
        [
            0x30300, 0x30308, 0x3030c, 0x31004, 0x31100, 0x31104, 0x31330, 0x31334, 0x31338,
            0x31348,
        ]
        .iter()
        .position(|a| *a == address)
    }
    fn index(address: u32) -> Option<usize> {
        if address & 3 != 0 {
            return None;
        }
        match address {
            0x14000..=0x14034 | 0x14040..=0x14064 => Some(((address - 0x14000) / 4) as usize),
            _ => None,
        }
    }
    pub(crate) fn read(&self, address: u32, size: usize) -> Option<Result<u32, &'static str>> {
        if (0x2fd40..=0x2fd60).contains(&(address & !3)) {
            return Some(if size != 4 {
                Err("wireless registers require word accesses")
            } else if address == 0x2fd40 {
                Ok(self.slot_enabled as u32 | (self.slot_pending as u32) << 16)
            } else {
                Ok(self.slot_alarms[((address - 0x2fd44) / 4) as usize])
            });
        }
        if matches!(
            address & !3,
            0x20010 | 0x2001c | 0x20020 | 0x20024 | 0x200e4
        ) {
            return Some(if size != 4 {
                Err("wireless registers require word accesses")
            } else {
                match address {
                    0x20010 => Ok(if self.bt_clock_pending { 512 } else { 0 }),
                    0x2001c => Ok((self.bt_sample_ticks != 0) as u32),
                    0x20020 => Ok(self.bt_clock_sample),
                    0x20024 => Ok(self.bt_fine_sample),
                    _ => Err("Bluetooth clock alarm readback is not implemented"),
                }
            });
        }
        if address & !3 == 0x200c0 {
            // Vendor bredr_frame.c __write_reg_txericntl packs a descriptor
            // offset and two control fields here. Configuration only: neither
            // transmit execution nor this register's readback is modeled.
            return Some(Err("Bluetooth TX descriptor readback is not implemented"));
        }
        if crate::ble_engine::Engine::owns(address) {
            return Some(self.ble.read(address & !3, size));
        }
        if let Some(index) = Self::bt_index(address & !3) {
            return Some(if size != 4 {
                Err("wireless registers require word accesses")
            } else if address == 0x2fd9c {
                Err("Bluetooth RF table reads are not implemented")
            } else {
                Ok(self.bt_configuration[index])
            });
        }
        if (0x11900..=0x1197b).contains(&address) {
            return Some(if size == 4 {
                Ok(if address == 0x11978 {
                    self.sample_result
                } else {
                    self.analog[((address - 0x11900) / 4) as usize]
                })
            } else {
                Err("wireless registers require word accesses")
            });
        }
        if address & !3 == 0x3101c || Self::mac_index(address & !3).is_some() {
            return Some(if size != 4 {
                Err("wireless registers require word accesses")
            } else {
                Ok(if address == 0x3101c {
                    self.bbp_command
                } else {
                    self.mac[Self::mac_index(address).unwrap()]
                })
            });
        }
        if matches!(address & !3, 0x30f00 | 0x30f04) {
            return Some(if size == 4 {
                Ok(self.radio_configuration[((address - 0x30f00) / 4) as usize])
            } else {
                Err("wireless registers require word accesses")
            });
        }
        let index = Self::index(address & !3)?;
        Some(if size == 4 {
            Ok(self.registers[index])
        } else {
            Err("wireless registers require word accesses")
        })
    }
    pub(crate) fn write(
        &mut self,
        address: u32,
        value: u32,
        size: usize,
    ) -> Option<Result<(), &'static str>> {
        if (0x2fd40..=0x2fd60).contains(&(address & !3)) {
            return Some(if size != 4 {
                Err("wireless registers require word accesses")
            } else {
                if address == 0x2fd40 {
                    // Vendor bredr_slot_timer.c: enables 0..7, ACK 8..15,
                    // pending 16..23. Pending bits cannot be set by a store.
                    self.slot_enabled = value as u8;
                    self.slot_pending &= !((value >> 8) as u8);
                } else {
                    self.slot_alarms[((address - 0x2fd44) / 4) as usize] = value & 0x07ff_ffff;
                }
                Ok(())
            });
        }
        if matches!(
            address & !3,
            0x20010 | 0x2001c | 0x20020 | 0x20024 | 0x200e4
        ) {
            return Some(if size != 4 {
                Err("wireless registers require word accesses")
            } else {
                match address {
                    0x2001c if value == 1 => {
                        // READ_SLOT_CLK requests a coherent clock/fine pair,
                        // then polls until the request is consumed.
                        self.bt_sample_ticks = 1;
                        Ok(())
                    }
                    0x200e4 => {
                        self.bt_alarm = value & 0x07ff_ffff;
                        Ok(())
                    }
                    0x2001c => Err("unsupported Bluetooth clock sample command"),
                    _ => Err("Bluetooth clock/status registers are read-only"),
                }
            });
        }
        if address & !3 == 0x20018 {
            return Some(if size != 4 {
                Err("wireless registers require word accesses")
            } else if value & !65535 != 0 {
                Err("unsupported Bluetooth event acknowledgement")
            } else {
                // Vendor __timer_register and the slot timer ISR acknowledge
                // the clock event with bit 9. Other radio events are absent.
                if value & 512 != 0 {
                    self.bt_clock_pending = false;
                }
                Ok(())
            });
        }
        if crate::ble_engine::Engine::owns(address) {
            return Some(self.ble.write(address & !3, value, size));
        }
        if let Some(index) = Self::bt_index(address & !3) {
            if size != 4 {
                return Some(Err("wireless registers require word accesses"));
            }
            if address == 0x2fd98 {
                if value != 0 {
                    return Some(Err("unsupported Bluetooth RF table position"));
                }
                self.bt_table_position = 0;
            }
            if address == 0x2fd9c {
                if self.bt_table_position == self.bt_table.len() {
                    return Some(Err("Bluetooth RF table write exceeds its capacity"));
                }
                self.bt_table[self.bt_table_position] = value;
                self.bt_table_position += 1;
            }
            self.bt_configuration[index] = value;
            // The baseband RAM window (fact sheet §2.2, §4 [M:t] H).
            match address {
                0x2fc84 => self.ble.set_ram_window(Some(value), None),
                0x2fcbc => self.ble.set_ram_window(None, Some(value)),
                _ => {}
            }
            return Some(Ok(()));
        }
        if (0x11900..=0x1197b).contains(&address) {
            return Some(if size == 4 {
                if address == 0x11978 {
                    match value {
                        0 => self.sample_strobes = 0,
                        1 => {
                            if self.analog[26] >> 28 != 1 {
                                return Some(Err(
                                    "wireless analog measurement mux is not implemented",
                                ));
                            }
                            self.sample_strobes = self.sample_strobes.saturating_add(1);
                            if self.sample_strobes >= 8 {
                                if self.analog[26] & 1 != 0 {
                                    self.sample_result = 0x80;
                                    if self.filter_ticks == 0 {
                                        self.sample_result |= 32 | (self.filter_result as u32) << 8;
                                    }
                                    return Some(Ok(()));
                                }
                                let cap = ((self.analog[14] >> 19) & 127) as usize;
                                let feedback = (self.analog[15] >> 5) & 255;
                                let (low, high) = PLL_THRESHOLDS[cap];
                                self.sample_result = 0x81;
                                if feedback < low as u32 {
                                    self.sample_result |= 1 << 17;
                                }
                                if feedback >= high as u32 {
                                    self.sample_result |= 1 << 18;
                                }
                            }
                        }
                        _ => return Some(Err("unsupported wireless analog sample strobe")),
                    }
                } else {
                    if address == 0x11968 && value & 1 != 0 && self.analog[26] & 1 == 0 {
                        let period = (value >> 1) & 511;
                        let window = (value >> 10) & 255;
                        if window != 30 || !matches!(period, 254 | 145 | 80) {
                            return Some(Err("unsupported wireless filter calibration settings"));
                        }
                        // A bounded calibration transaction, with nominal timing
                        // proportional to the requested reference window.
                        self.filter_ticks = window * period;
                        // FM-1_985's settled capture with stock analog settings:
                        // all three startup requests return sample 0x0000ffa0.
                        self.filter_result = 255;
                    }
                    self.analog[((address - 0x11900) / 4) as usize] = value;
                }
                Ok(())
            } else {
                Err("wireless registers require word accesses")
            });
        }
        if address & !3 == 0x3101c || Self::mac_index(address & !3).is_some() {
            if size != 4 {
                return Some(Err("wireless registers require word accesses"));
            }
            if address == 0x3101c {
                // bbp_set/bbp_rd: bit 17 starts the byte transaction,
                // bit 16 selects read, bits 8..15 are the BBP register.
                let index = ((value >> 8) & 255) as usize;
                self.bbp_command = value & !(1 << 17);
                if value & (1 << 17) != 0 {
                    if value & (1 << 16) != 0 {
                        self.bbp_command = (self.bbp_command & !255) | self.bbp[index] as u32;
                    } else {
                        self.bbp[index] = value as u8;
                    }
                }
            } else {
                self.mac[Self::mac_index(address).unwrap()] = value;
            }
            return Some(Ok(()));
        }
        if matches!(address & !3, 0x30f00 | 0x30f04) {
            return Some(if size == 4 {
                self.radio_configuration[((address - 0x30f00) / 4) as usize] = value;
                Ok(())
            } else {
                Err("wireless registers require word accesses")
            });
        }
        let index = Self::index(address & !3)?;
        if size != 4 {
            return Some(Err("wireless registers require word accesses"));
        }
        if address == 0x14024 {
            return Some(Err("wireless frequency result is read-only"));
        }
        if address == 0x14020 && value != 0 {
            return Some(Err("wireless frequency calibration is not implemented"));
        }
        self.registers[index] = value;
        Some(Ok(()))
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    fn sample(w: &mut Wireless) -> (u32, u32) {
        w.write(0x2001c, 1, 4).unwrap().unwrap();
        assert_eq!(w.read(0x2001c, 4), Some(Ok(1)));
        w.advance_radio(1);
        assert_eq!(w.read(0x2001c, 4), Some(Ok(0)));
        (
            w.read(0x20020, 4).unwrap().unwrap(),
            w.read(0x20024, 4).unwrap().unwrap(),
        )
    }
    #[test]
    fn sampled_slot_and_fine_time_are_coherent_and_use_625_microseconds() {
        let mut w = Wireless::default();
        w.write(0x20000, 0x107, 4).unwrap().unwrap();
        w.advance_radio(14_998);
        assert_eq!(sample(&mut w), (0, 624));
        assert_eq!(sample(&mut w), (1, 0));
        w.advance_radio(15_000 + 24 * 123 - 1);
        assert_eq!(sample(&mut w), (2, 123));
        w.advance_radio(30_000);
        assert_eq!(w.read(0x20020, 4), Some(Ok(2)));
        assert_eq!(w.read(0x20024, 4), Some(Ok(123)));
        assert_eq!(sample(&mut w), (4, 123));
        assert!(w.write(0x20020, 1, 4).unwrap().is_err());
        assert!(w.write(0x2001c, 2, 4).unwrap().is_err());
        assert!(w.read(0x20024, 2).unwrap().is_err());
    }
    #[test]
    fn link_timeout_latches_and_acknowledges_only_at_the_programmed_slot() {
        let mut w = Wireless::default();
        w.write(0x20000, 0x107, 4).unwrap().unwrap();
        w.write(0x200e4, 3, 4).unwrap().unwrap();
        w.write(0x2000c, 512, 4).unwrap().unwrap();
        w.advance_radio(44_999);
        assert!(!w.clock_pending_irq());
        w.advance_radio(1);
        assert!(w.clock_pending_irq());
        assert_eq!(w.read(0x20010, 4), Some(Ok(512)));
        w.write(0x20018, 1, 4).unwrap().unwrap();
        assert!(w.clock_pending_irq());
        w.write(0x20018, 512, 4).unwrap().unwrap();
        assert!(!w.clock_pending_irq());
        assert_eq!(w.read(0x20018, 4), Some(Ok(0)));
    }
    #[test]
    fn eight_slot_timers_have_separate_deadlines_and_acknowledgements() {
        let mut w = Wireless::default();
        w.write(0x20000, 0x107, 4).unwrap().unwrap();
        for i in 0..8 {
            w.write(0x2fd44 + i * 4, i + 1, 4).unwrap().unwrap();
        }
        w.write(0x2fd40, 0xff00, 4).unwrap().unwrap();
        w.advance_radio(1_000);
        assert!(!w.slot_pending_irq());
        w.write(0x2fd40, 255, 4).unwrap().unwrap();
        w.advance_radio(13_999);
        assert!(!w.slot_pending_irq());
        w.advance_radio(1);
        assert_eq!(w.read(0x2fd40, 4), Some(Ok(0x100ff)));
        w.advance_radio(15_000 * 7);
        assert_eq!(w.read(0x2fd40, 4), Some(Ok(0xff00ff)));
        w.write(0x2fd40, 0x0100 | 255, 4).unwrap().unwrap();
        assert_eq!(w.read(0x2fd40, 4), Some(Ok(0xfe00ff)));
        w.write(0x2fd40, 0xff00, 4).unwrap().unwrap();
        assert_eq!(w.read(0x2fd40, 4), Some(Ok(0)));
        assert!(!w.slot_pending_irq());
        assert!(w.read(0x2fd64, 4).is_none());
    }
    #[test]
    fn clock_and_alarms_wrap_at_27_bits_and_pause_when_disabled() {
        let mut w = Wireless::default();
        w.write(0x20000, 0x107, 4).unwrap().unwrap();
        w.bt_clock_ticks = 15_000 * (1 << 27) - 2;
        w.write(0x2fd44, 0, 4).unwrap().unwrap();
        w.write(0x2fd40, 1, 4).unwrap().unwrap();
        assert_eq!(sample(&mut w), (0x7ffffff, 624));
        assert!(!w.slot_pending_irq());
        assert_eq!(sample(&mut w), (0, 0));
        assert!(w.slot_pending_irq());
        w.write(0x20000, 0, 4).unwrap().unwrap();
        w.advance_radio(1_000_000);
        assert_eq!(sample(&mut w), (0, 0));
    }
}

// Nominal comparator bounds measured on the connected FM-1 with FM-1_985.
// Index: seven-bit capacitor bank; values: eight-bit feedback divider at
// which LOW drops and HIGH rises. 256 means outside the divider range.
// The 16-bank discontinuities are measured hardware behavior. These are
// fixed device measurements, not an RF voltage/noise or temperature model.
const PLL_THRESHOLDS: [(u16, u16); 128] = [
    (256, 256),
    (256, 256),
    (256, 256),
    (256, 256),
    (256, 256),
    (256, 256),
    (256, 256),
    (256, 256),
    (251, 256),
    (243, 256),
    (235, 251),
    (228, 243),
    (220, 235),
    (212, 227),
    (205, 220),
    (197, 212),
    (256, 256),
    (253, 256),
    (245, 256),
    (237, 253),
    (230, 245),
    (222, 237),
    (214, 230),
    (207, 222),
    (199, 214),
    (192, 206),
    (184, 199),
    (177, 192),
    (170, 184),
    (162, 177),
    (155, 169),
    (148, 162),
    (208, 223),
    (201, 216),
    (193, 208),
    (186, 200),
    (178, 193),
    (171, 186),
    (164, 178),
    (157, 171),
    (150, 164),
    (143, 157),
    (136, 150),
    (129, 142),
    (122, 135),
    (115, 129),
    (108, 122),
    (102, 115),
    (159, 173),
    (151, 166),
    (144, 158),
    (137, 151),
    (131, 144),
    (124, 137),
    (117, 130),
    (110, 123),
    (103, 117),
    (97, 110),
    (90, 103),
    (84, 97),
    (77, 90),
    (71, 83),
    (64, 77),
    (58, 70),
    (110, 123),
    (103, 116),
    (96, 109),
    (90, 103),
    (83, 96),
    (77, 90),
    (70, 83),
    (64, 77),
    (58, 70),
    (51, 64),
    (45, 57),
    (39, 51),
    (33, 45),
    (27, 39),
    (21, 33),
    (15, 26),
    (65, 78),
    (59, 72),
    (53, 65),
    (47, 59),
    (40, 53),
    (34, 47),
    (28, 40),
    (22, 34),
    (16, 28),
    (10, 22),
    (4, 16),
    (0, 10),
    (0, 4),
    (0, 0),
    (0, 0),
    (0, 0),
    (23, 35),
    (17, 29),
    (11, 23),
    (5, 17),
    (0, 11),
    (0, 5),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
    (0, 0),
];

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(w: &mut Wireless) -> u32 {
        for _ in 0..8 {
            w.write(0x11978, 1, 4).unwrap().unwrap();
        }
        w.write(0x11978, 0, 4).unwrap().unwrap();
        w.read(0x11978, 4).unwrap().unwrap()
    }
    #[test]
    fn bluetooth_controller_keeps_dma_base_and_independent_channel_offsets() {
        let mut w = Wireless::default();
        // Stock initialization installs the base pointer, then packs per-channel
        // relative offsets. Subsequent base reads must preserve the RAM address.
        w.write(0x2fc44, 0x01c0a05c, 4).unwrap().unwrap();
        let offsets = 0x0ee00ee0;
        for addr in [0x20160, 0x2015c, 0x20130, 0x2012c] {
            w.write(addr, offsets, 4).unwrap().unwrap();
        }
        w.write(0x20138, 0x14050770, 4).unwrap().unwrap();
        w.write(0x20168, 0x14050770, 4).unwrap().unwrap();
        assert_eq!(w.read(0x2fc44, 4).unwrap().unwrap(), 0x01c0a05c);
        assert_eq!(w.read(0x20130, 4).unwrap().unwrap(), offsets);
        assert_eq!(w.read(0x20138, 4).unwrap().unwrap(), 0x14050770);
        assert!(w.read(0x2013c, 4).is_none());
        assert!(w.write(0x2fc44, 0, 2).unwrap().is_err());
        assert_eq!(w.read(0x2fc44, 4).unwrap().unwrap(), 0x01c0a05c);
    }
    #[test]
    fn bluetooth_rf_table_retains_every_word_and_checks_its_capacity() {
        let mut w = Wireless::default();
        w.write(0x2fc40, 0xfcfc, 4).unwrap().unwrap();
        assert_eq!(w.read(0x2fc40, 4).unwrap().unwrap(), 0xfcfc);
        assert!(w.read(0x2fc4c, 4).is_none());
        w.write(0x2fd98, 0, 4).unwrap().unwrap();
        for i in 0..128 {
            w.write(0x2fd9c, i ^ 0x13579bdf, 4).unwrap().unwrap();
        }
        for i in 0..128 {
            assert_eq!(w.bt_table[i], i as u32 ^ 0x13579bdf);
        }
        assert!(w.write(0x2fd9c, 0, 4).unwrap().is_err());
        assert!(w.read(0x2fd9c, 4).unwrap().is_err());
        w.write(0x2fd98, 0, 4).unwrap().unwrap();
        w.write(0x2fd9c, 7, 4).unwrap().unwrap();
        assert_eq!(w.bt_table[0], 7);
    }
    #[test]
    fn filter_calibration_completes_with_measured_results_and_can_restart() {
        let mut w = Wireless::default();
        for period in [254, 145, 80] {
            w.write(0x11968, 0, 4).unwrap().unwrap();
            w.write(0x11968, 0x10007801 | period << 1, 4)
                .unwrap()
                .unwrap();
            assert_eq!(sample(&mut w), 0x80);
            w.advance_radio(30 * period - 1);
            assert_eq!(sample(&mut w), 0x80);
            w.advance_radio(1);
            assert_eq!(sample(&mut w), 0xffa0);
        }
        w.write(0x11968, 0, 4).unwrap().unwrap();
        assert!(w.write(0x11968, 0x10007803, 4).unwrap().is_err());
        assert_eq!(w.read(0x11968, 4).unwrap().unwrap(), 0);
    }
}
