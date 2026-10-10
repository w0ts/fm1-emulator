// SPDX-License-Identifier: GPL-3.0-only
//! The virtual BLE air interface shared by the baseband engine model
//! (`ble_engine`) and the virtual central (`ble_central`): PDU layouts from
//! the Bluetooth Core specification (Vol 6, Part B), channel selection
//! algorithm #1, air time on the 1M PHY, and the air log.
//!
//! Nothing here is JieLi-specific: it is the public specification.

/// The advertising access address (Core Vol 6 Part B 2.1.2).
pub const ADV_ACCESS_ADDRESS: u32 = 0x8E89_BED6;
/// T_IFS, the inter-frame space (µs).
pub const T_IFS_US: u64 = 150;
/// Oscillator ticks per microsecond (the emulator's 24 MHz time base).
pub const TICKS_PER_US: u64 = 24;
/// One 1.25 ms connection-interval unit, in ticks.
pub const UNIT_1250_TICKS: u64 = 1_250 * TICKS_PER_US;

/// Advertising PDU types (Core Vol 6 Part B 2.3).
pub mod adv {
    pub const ADV_IND: u8 = 0;
    pub const ADV_DIRECT_IND: u8 = 1;
    pub const ADV_NONCONN_IND: u8 = 2;
    pub const SCAN_REQ: u8 = 3;
    pub const SCAN_RSP: u8 = 4;
    pub const CONNECT_IND: u8 = 5;
    pub const ADV_SCAN_IND: u8 = 6;
}

/// LL control opcodes (Core Vol 6 Part B 2.4.2).
pub mod ll {
    pub const CONNECTION_UPDATE_IND: u8 = 0x00;
    pub const CHANNEL_MAP_IND: u8 = 0x01;
    pub const TERMINATE_IND: u8 = 0x02;
    pub const UNKNOWN_RSP: u8 = 0x07;
    pub const FEATURE_REQ: u8 = 0x08;
    pub const FEATURE_RSP: u8 = 0x09;
    pub const VERSION_IND: u8 = 0x0C;
    pub const REJECT_IND: u8 = 0x0D;
    pub const SLAVE_FEATURE_REQ: u8 = 0x0E;
    pub const CONNECTION_PARAM_REQ: u8 = 0x0F;
    pub const CONNECTION_PARAM_RSP: u8 = 0x10;
    pub const REJECT_EXT_IND: u8 = 0x11;
    pub const PING_REQ: u8 = 0x12;
    pub const PING_RSP: u8 = 0x13;
    pub const LENGTH_REQ: u8 = 0x14;
    pub const LENGTH_RSP: u8 = 0x15;
    pub const PHY_REQ: u8 = 0x16;
}

/// Air time of a 1M PHY packet with `payload` PDU payload bytes:
/// preamble 1, access address 4, header 2, payload, CRC 3; 8 µs per byte.
pub fn airtime_us(payload: usize) -> u64 {
    (1 + 4 + 2 + payload as u64 + 3) * 8
}

/// Air time in oscillator ticks.
pub fn airtime_ticks(payload: usize) -> u64 {
    airtime_us(payload) * TICKS_PER_US
}

/// RF channel index (0..=39) to centre frequency in MHz.
pub fn channel_mhz(channel: u8) -> u32 {
    match channel {
        37 => 2402,
        38 => 2426,
        39 => 2480,
        0..=10 => 2404 + 2 * channel as u32,
        _ => 2406 + 2 * channel as u32,
    }
}

/// The data channels marked used in a 37-bit channel map, ascending.
pub fn used_channels(map: u64) -> Vec<u8> {
    (0..37).filter(|c| map >> c & 1 != 0).collect()
}

/// Channel selection algorithm #1 (Core Vol 6 Part B 4.5.8.2):
/// `unmapped = (last_unmapped + hop) mod 37`; a channel not in `used` is
/// remapped to `used[unmapped mod used.len()]`. Returns (unmapped, channel).
pub fn csa1(last_unmapped: u8, hop: u8, used: &[u8]) -> (u8, u8) {
    let unmapped = (last_unmapped + hop) % 37;
    if used.contains(&unmapped) || used.is_empty() {
        (unmapped, unmapped)
    } else {
        (unmapped, used[unmapped as usize % used.len()])
    }
}

/// CONNECT_IND LLData (Core Vol 6 Part B 2.3.3.1), in units of the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectParams {
    pub access_address: u32,
    pub crc_init: u32,
    /// transmitWindowSize, 1.25 ms units.
    pub win_size: u8,
    /// transmitWindowOffset, 1.25 ms units.
    pub win_offset: u16,
    /// connInterval, 1.25 ms units.
    pub interval: u16,
    pub latency: u16,
    /// connSupervisionTimeout, 10 ms units.
    pub timeout: u16,
    /// 37-bit channel map.
    pub channel_map: u64,
    pub hop: u8,
    pub sca: u8,
}

impl Default for ConnectParams {
    fn default() -> Self {
        // An iPhone-like first connection: 30 ms, no latency, 720 ms timeout.
        Self {
            access_address: 0x5065_A1C3,
            crc_init: 0x00A3_5F21,
            win_size: 2,
            win_offset: 0,
            interval: 24,
            latency: 0,
            timeout: 72,
            channel_map: 0x1F_FFFF_FFFF,
            hop: 7,
            sca: 0,
        }
    }
}

impl ConnectParams {
    /// The 22 LLData bytes, little-endian fields in Core-spec order.
    pub fn lldata(&self) -> [u8; 22] {
        let mut d = [0; 22];
        d[0..4].copy_from_slice(&self.access_address.to_le_bytes());
        d[4..7].copy_from_slice(&self.crc_init.to_le_bytes()[..3]);
        d[7] = self.win_size;
        d[8..10].copy_from_slice(&self.win_offset.to_le_bytes());
        d[10..12].copy_from_slice(&self.interval.to_le_bytes());
        d[12..14].copy_from_slice(&self.latency.to_le_bytes());
        d[14..16].copy_from_slice(&self.timeout.to_le_bytes());
        d[16..21].copy_from_slice(&self.channel_map.to_le_bytes()[..5]);
        d[21] = (self.hop & 31) | (self.sca & 7) << 5;
        d
    }
    pub fn from_lldata(d: &[u8]) -> Option<Self> {
        if d.len() < 22 {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        let mut map = [0u8; 8];
        map[..5].copy_from_slice(&d[16..21]);
        Some(Self {
            access_address: u32::from_le_bytes([d[0], d[1], d[2], d[3]]),
            crc_init: u32::from_le_bytes([d[4], d[5], d[6], 0]),
            win_size: d[7],
            win_offset: u16_at(8),
            interval: u16_at(10),
            latency: u16_at(12),
            timeout: u16_at(14),
            channel_map: u64::from_le_bytes(map),
            hop: d[21] & 31,
            sca: d[21] >> 5,
        })
    }
}

/// Who put a packet on the air.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sender {
    /// The emulated FM-1 (the baseband engine model).
    Peripheral,
    /// The virtual central.
    Central,
    /// A virtual advertiser that only the scanner hears (ble_engine::Advertiser).
    Advertiser,
    /// The FM-1 as the master of a connection (it initiated).
    Master,
    /// A virtual peripheral (ble_peripheral) in a connection the FM-1 made.
    Slave,
}

/// One packet on the virtual air, in time order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AirRecord {
    /// Oscillator tick (24 MHz) at the start of the packet.
    pub tick: u64,
    pub sender: Sender,
    pub channel: u8,
    pub access_address: u32,
    /// Connection event counter (data channels only).
    pub event: Option<u16>,
    /// Header (2 bytes) and payload.
    pub pdu: Vec<u8>,
    /// What happened to it: who received it, or why nobody did.
    pub note: String,
}

impl std::fmt::Display for AirRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let who = match self.sender {
            Sender::Peripheral => "P->C",
            Sender::Central => "C->P",
            Sender::Advertiser => "A->S",
            Sender::Master => "M->S",
            Sender::Slave => "S->M",
        };
        let event = self.event.map_or("  -  ".into(), |e| format!("{e:5}"));
        let hex: Vec<_> = self.pdu.iter().map(|b| format!("{b:02x}")).collect();
        write!(
            f,
            "{:12.6} {who} ch{:02} ev{event} {:<28} {}",
            self.tick as f64 / 24e6,
            self.channel,
            describe(self.access_address, &self.pdu),
            hex.join(" "),
        )?;
        if !self.note.is_empty() {
            write!(f, "  [{}]", self.note)?;
        }
        Ok(())
    }
}

/// A short name for a PDU (advertising type, LL control opcode, or the
/// L2CAP channel and ATT/signalling opcode of a start fragment).
pub fn describe(access_address: u32, pdu: &[u8]) -> String {
    if pdu.len() < 2 {
        return "short".into();
    }
    if access_address == ADV_ACCESS_ADDRESS {
        let name = match pdu[0] & 15 {
            adv::ADV_IND => "ADV_IND",
            adv::ADV_DIRECT_IND => "ADV_DIRECT_IND",
            adv::ADV_NONCONN_IND => "ADV_NONCONN_IND",
            adv::SCAN_REQ => "SCAN_REQ",
            adv::SCAN_RSP => "SCAN_RSP",
            adv::CONNECT_IND => "CONNECT_IND",
            adv::ADV_SCAN_IND => "ADV_SCAN_IND",
            _ => "ADV_?",
        };
        return name.into();
    }
    let flags = format!(
        "{}{}{}",
        if pdu[0] & 8 != 0 { "S" } else { "s" },
        if pdu[0] & 4 != 0 { "N" } else { "n" },
        if pdu[0] & 16 != 0 { "M" } else { "" }
    );
    let payload = &pdu[2..];
    let what = match pdu[0] & 3 {
        1 if payload.is_empty() => "empty".to_string(),
        1 => "L2CAP cont".to_string(),
        2 => describe_l2cap(payload),
        3 => payload
            .first()
            .map_or("LL ?".into(), |&op| ll_name(op).into()),
        _ => "LLID 0".into(),
    };
    format!("{what} {flags}")
}

fn describe_l2cap(payload: &[u8]) -> String {
    if payload.len() < 5 {
        return "L2CAP start".into();
    }
    let cid = u16::from_le_bytes([payload[2], payload[3]]);
    let op = payload[4];
    match cid {
        4 => format!("ATT {}", att_name(op)),
        5 => format!("SIG 0x{op:02x}"),
        6 => format!("SMP 0x{op:02x}"),
        _ => format!("L2CAP cid {cid}"),
    }
}

pub fn ll_name(op: u8) -> &'static str {
    match op {
        ll::CONNECTION_UPDATE_IND => "LL_CONNECTION_UPDATE_IND",
        ll::CHANNEL_MAP_IND => "LL_CHANNEL_MAP_IND",
        ll::TERMINATE_IND => "LL_TERMINATE_IND",
        ll::UNKNOWN_RSP => "LL_UNKNOWN_RSP",
        ll::FEATURE_REQ => "LL_FEATURE_REQ",
        ll::FEATURE_RSP => "LL_FEATURE_RSP",
        ll::VERSION_IND => "LL_VERSION_IND",
        ll::REJECT_IND => "LL_REJECT_IND",
        ll::SLAVE_FEATURE_REQ => "LL_SLAVE_FEATURE_REQ",
        ll::CONNECTION_PARAM_REQ => "LL_CONNECTION_PARAM_REQ",
        ll::CONNECTION_PARAM_RSP => "LL_CONNECTION_PARAM_RSP",
        ll::REJECT_EXT_IND => "LL_REJECT_EXT_IND",
        ll::PING_REQ => "LL_PING_REQ",
        ll::PING_RSP => "LL_PING_RSP",
        ll::LENGTH_REQ => "LL_LENGTH_REQ",
        ll::LENGTH_RSP => "LL_LENGTH_RSP",
        ll::PHY_REQ => "LL_PHY_REQ",
        _ => "LL_?",
    }
}

pub fn att_name(op: u8) -> &'static str {
    match op {
        0x01 => "ERROR_RSP",
        0x02 => "MTU_REQ",
        0x03 => "MTU_RSP",
        0x04 => "FIND_INFO_REQ",
        0x05 => "FIND_INFO_RSP",
        0x08 => "READ_BY_TYPE_REQ",
        0x09 => "READ_BY_TYPE_RSP",
        0x0A => "READ_REQ",
        0x0B => "READ_RSP",
        0x10 => "READ_BY_GROUP_REQ",
        0x11 => "READ_BY_GROUP_RSP",
        0x12 => "WRITE_REQ",
        0x13 => "WRITE_RSP",
        0x1B => "NOTIFY",
        0x1D => "INDICATE",
        0x1E => "CONFIRM",
        0x52 => "WRITE_CMD",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors derived by hand from the CSA #1 definition (Core v5.3 Vol 6
    // Part B 4.5.8.2); the Core specification's sample-data chapter covers
    // CSA #2 only.
    #[test]
    fn csa1_with_all_channels_steps_by_the_hop() {
        let used = used_channels(0x1F_FFFF_FFFF);
        assert_eq!(used.len(), 37);
        let mut last = 0;
        let mut seq = Vec::new();
        for _ in 0..8 {
            let (unmapped, channel) = csa1(last, 7, &used);
            assert_eq!(unmapped, channel);
            seq.push(channel);
            last = unmapped;
        }
        assert_eq!(seq, [7, 14, 21, 28, 35, 5, 12, 19]);
    }

    #[test]
    fn csa1_remaps_unused_channels_by_unmapped_mod_used_count() {
        // Channels 0, 1, 2, 10, 20 and 36 used (6 channels).
        let map = 1 | 2 | 4 | 1 << 10 | 1 << 20 | 1 << 36;
        let used = used_channels(map);
        assert_eq!(used, [0, 1, 2, 10, 20, 36]);
        let mut last = 0;
        let mut seq = Vec::new();
        for _ in 0..6 {
            let (unmapped, channel) = csa1(last, 10, &used);
            seq.push((unmapped, channel));
            last = unmapped;
        }
        // 10 used; 20 used; 30 -> 30 % 6 = 0 -> ch 0; 40 % 37 = 3 -> 3 % 6
        // = 3 -> ch 10; 13 -> 13 % 6 = 1 -> ch 1; 23 -> 23 % 6 = 5 -> ch 36.
        assert_eq!(
            seq,
            [(10, 10), (20, 20), (30, 0), (3, 10), (13, 1), (23, 36)]
        );
    }

    #[test]
    fn csa1_visits_every_used_channel_with_any_valid_hop() {
        let used = used_channels(0x1F_FFFF_FFFF);
        for hop in 5..=16 {
            let mut seen = [false; 37];
            let mut last = 0;
            for _ in 0..37 {
                let (unmapped, channel) = csa1(last, hop, &used);
                seen[channel as usize] = true;
                last = unmapped;
            }
            assert!(seen.iter().all(|s| *s), "hop {hop}");
        }
    }

    #[test]
    fn connect_lldata_round_trips_in_core_order() {
        let p = ConnectParams {
            win_offset: 3,
            channel_map: 0x1F_0000_00FF,
            hop: 12,
            sca: 5,
            ..Default::default()
        };
        let d = p.lldata();
        assert_eq!(&d[0..4], &[0xC3, 0xA1, 0x65, 0x50]);
        assert_eq!(&d[4..7], &[0x21, 0x5F, 0xA3]);
        assert_eq!(d[21], 12 | 5 << 5);
        assert_eq!(ConnectParams::from_lldata(&d), Some(p));
    }

    #[test]
    fn airtime_and_frequencies_follow_the_1m_phy() {
        assert_eq!(airtime_us(0), 80);
        assert_eq!(airtime_us(27), 296);
        assert_eq!(channel_mhz(37), 2402);
        assert_eq!(channel_mhz(0), 2404);
        assert_eq!(channel_mhz(10), 2424);
        assert_eq!(channel_mhz(11), 2428);
        assert_eq!(channel_mhz(36), 2478);
        assert_eq!(channel_mhz(39), 2480);
    }
}
