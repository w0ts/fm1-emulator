// SPDX-License-Identifier: GPL-3.0-only
//! Behavioural model of the WL82 BLE baseband engine (registers
//! `0x28000–0x28054`, the per-link column port and the control blocks in
//! the baseband RAM window), driven by the emulator's 24 MHz time base and
//! talking to a virtual central (`ble_central`) over the virtual air
//! (`ble_air`).
//!
//! Every behaviour is a hypothesis built from docs/BLE-HW-FACTS.md (the
//! "fact sheet", optimist repository, branch feat/ble-facts). Comments cite
//! its section and tag: [M:t] trace, [M:d] dump, [M:s] value from the
//! vendor binary, [I] inferred. Where the sheet lists alternatives, the
//! choice is an `EngineConfig` flag. Nothing here ran against hardware.

use crate::ble_air::{
    adv, airtime_ticks, AirRecord, Sender, ADV_ACCESS_ADDRESS, TICKS_PER_US, T_IFS_US,
};
use crate::ble_central::{Central, CentralTx};
use crate::ble_param::{field, Block, Window};
use crate::ble_peripheral::Peripheral;

mod advertising;
mod central;
#[cfg(test)]
mod central_tests;
mod connection;
mod scanning;
#[cfg(test)]
mod tests;

use central::{MasterConn, MasterSetup};
use connection::Received;
pub use scanning::{Advertiser, MIDI_UUID};
use scanning::{AirSource, PendingRx, ScanWin};

/// Interrupt sources (§10 [M]): the event interrupt and the RX interrupt.
pub const EVENT_IRQ: usize = 45;
pub const RX_IRQ: usize = 29;

/// One 625 µs slot in oscillator ticks.
const SLOT: u64 = 625 * TICKS_PER_US;
const T_IFS: u64 = T_IFS_US * TICKS_PER_US;
const LINKS: usize = 8;
const COLUMNS: usize = 17;
/// Records kept in the air log (the count goes on).
const AIR_LOG_LIMIT: usize = 400_000;
const NOTE_LIMIT: usize = 2_000;

/// What the engine does with a SCAN_REQ (§6: "answering a SCAN_REQ within
/// T_IFS needs no software" is [I, strong]; U6 is the open question).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanResponse {
    /// The engine sends TX buffer 1 (the SCAN_RSP) by itself, unless
    /// FORMAT bit8 (ignore SCAN_REQ) is set.
    Automatic,
    /// The engine only stores the SCAN_REQ in an RX buffer and raises the
    /// RX interrupt; nothing is sent (software cannot meet T_IFS).
    Software,
}

/// Q1: a received packet whose SN is the one already acknowledged (the
/// central's retransmission) is acknowledged again in every mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OldSn {
    /// Discarded silently: no buffer, no interrupt.
    Drop,
    /// Discarded, but the RX interrupt is raised (no buffer is filled).
    DropWithIrq,
    /// Stored and signalled like a new packet; the driver must drop it.
    Store,
}

/// Choices where the fact sheet leaves the behaviour open.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub scan_response: ScanResponse,
    /// Also store an answered SCAN_REQ and raise the RX interrupt.
    pub scan_req_rx_irq: bool,
    /// U6: the engine stops advertising by itself on a CONNECT_IND for it.
    pub stop_adv_on_connect_ind: bool,
    /// Raise the event interrupt at the end of an advertising event cut
    /// short by a CONNECT_IND.
    pub adv_event_irq_on_connect: bool,
    /// §8 IRQ 29 step 5 needs an RX interrupt per received packet for the TX
    /// bookkeeping, so new empty PDUs are stored like data [I]. False: an
    /// empty PDU is acknowledged without a buffer or an interrupt.
    pub store_empty_pdus: bool,
    /// Q1: what the engine does with a packet that repeats the last SN.
    pub old_sn: OldSn,
    /// Q1: a discarded repeat still updates EVTCOUNT (the event counter of
    /// the last reception).
    pub rx_count_repeats: bool,
    /// Q2: the event interrupt fires at the end of every connection event,
    /// received or not (needed by a software supervision timer, §7 step 15).
    /// False: only after events in which a packet was received.
    pub event_irq_every_event: bool,
    /// Q3: the engine resends an unacknowledged PDU by itself (a copy taken
    /// when it was sent). False: it sends whatever TX buffer TXTOG selects,
    /// so the driver keeps the NAKed buffer loaded.
    pub engine_retransmits: bool,
    /// Q5: request/response pairs per connection event while either side
    /// sets MD (1 = MD ignored).
    pub max_pairs_per_event: u32,
    /// Q7: interval/offset changes (columns 1/15/4) take effect only at the
    /// column-5 instant. False: the engine reads columns 1/15 every event.
    pub instant_gating: bool,
    /// The engine stores only into an RX buffer whose RXBUFnCNTL bit0 is 0
    /// (the fact sheet's "armed by software" reading, §3 [I]). Stock V15
    /// contradicts it: after the CONNECT_IND it repoints RXPTR0 but never
    /// clears RXBUF0CNTL bit0, so with this on it never receives. Default
    /// off: the engine writes the RXTOG buffer and sets bit0 as a status.
    pub rx_needs_armed: bool,
    /// Seed of the advDelay generator (Core: 0–10 ms pseudo-random).
    pub seed: u32,
    /// §21.9 C2: while scanning the engine sets RXBUFnCNTL bit0 on the
    /// buffer it filled (the vendor's connection-path rule). False: it
    /// leaves it 0, as the FM-1 did while advertising (§8.1).
    pub scan_cntl: bool,
    /// §21.9 C1: the engine writes the advertiser's AdvA into the SCAN_REQ
    /// (software leaves zeros there). False: the zeros go out and no
    /// advertiser answers.
    pub scan_adva: bool,
    /// §21.9 C3: while initiating, the engine answers the target's ADV_IND
    /// (or an ADV_DIRECT_IND to LOCALADR) with the CONNECT_IND software
    /// left in the TXTOG buffer, by itself. False: nothing is sent.
    pub init_cind: bool,
    /// §21.9 C4: with column 9 bit0 set the engine stops initiating after
    /// its CONNECT_IND and raises the event interrupt at its end. False:
    /// at the end of that scan window.
    pub init_evt_after_cind: bool,
    /// §21.9 C5: the master's first anchor is the anchor counter's (column
    /// 0 + 1 slots after software started it with column 14). False: the
    /// transmit window's start (1.25 ms + WinOffset after the CONNECT_IND).
    pub master_anchor_from_counter: bool,
    /// §21.9 C6: the master transmits first at each anchor. False: it
    /// sends nothing (and so hears nothing).
    pub master_tx_first: bool,
}

impl EngineConfig {
    /// Apply `key=value` settings separated by commas, e.g.
    /// `scan_rsp=software,old_sn=irq,pairs=4`.
    pub fn apply(&mut self, settings: &str) -> Result<(), String> {
        for item in settings.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, value) = item
                .split_once('=')
                .ok_or(format!("expected key=value: {item}"))?;
            let flag = || match value {
                "1" | "true" | "on" => Ok(true),
                "0" | "false" | "off" => Ok(false),
                _ => Err(format!("{key}: expected 0 or 1")),
            };
            match key {
                "scan_rsp" => {
                    self.scan_response = match value {
                        "auto" => ScanResponse::Automatic,
                        "software" => ScanResponse::Software,
                        _ => return Err("scan_rsp: auto or software".into()),
                    }
                }
                "scan_req_irq" => self.scan_req_rx_irq = flag()?,
                "stop_adv" => self.stop_adv_on_connect_ind = flag()?,
                "adv_irq_on_connect" => self.adv_event_irq_on_connect = flag()?,
                "store_empty" => self.store_empty_pdus = flag()?,
                "old_sn" => {
                    self.old_sn = match value {
                        "drop" => OldSn::Drop,
                        "irq" => OldSn::DropWithIrq,
                        "store" => OldSn::Store,
                        _ => return Err("old_sn: drop, irq or store".into()),
                    }
                }
                "event_irq_every_event" => self.event_irq_every_event = flag()?,
                "engine_retransmits" => self.engine_retransmits = flag()?,
                "pairs" => {
                    self.max_pairs_per_event = value.parse().map_err(|_| "pairs: a number")?
                }
                "instant_gating" => self.instant_gating = flag()?,
                "rx_count_repeats" => self.rx_count_repeats = flag()?,
                "rx_needs_armed" => self.rx_needs_armed = flag()?,
                "seed" => self.seed = value.parse().map_err(|_| "seed: a number")?,
                "scan_cntl" => self.scan_cntl = flag()?,
                "scan_adva" => self.scan_adva = flag()?,
                "init_cind" => self.init_cind = flag()?,
                "init_evt_after_cind" => self.init_evt_after_cind = flag()?,
                "master_anchor_from_counter" => self.master_anchor_from_counter = flag()?,
                "master_tx_first" => self.master_tx_first = flag()?,
                _ => return Err(format!("unknown engine setting {key}")),
            }
        }
        if self.max_pairs_per_event == 0 {
            return Err("pairs must be at least 1".into());
        }
        Ok(())
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            scan_response: ScanResponse::Automatic,
            scan_req_rx_irq: false,
            stop_adv_on_connect_ind: true,
            adv_event_irq_on_connect: true,
            store_empty_pdus: true,
            old_sn: OldSn::Drop,
            rx_count_repeats: true,
            event_irq_every_event: true,
            engine_retransmits: true,
            max_pairs_per_event: 1,
            instant_gating: true,
            rx_needs_armed: false,
            seed: 0x1234_5678,
            scan_cntl: true,
            scan_adva: true,
            init_cind: true,
            init_evt_after_cind: true,
            master_anchor_from_counter: true,
            master_tx_first: true,
        }
    }
}

/// Counters for reports and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
    pub adv_events: u64,
    pub adv_pdus: u64,
    pub scan_responses: u64,
    pub connect_inds: u64,
    pub conn_events: u64,
    pub conn_events_received: u64,
    pub rx_stored: u64,
    pub rx_not_acknowledged: u64,
    pub tx_data: u64,
    pub tx_empty: u64,
    pub tx_retransmit: u64,
    pub event_irqs: u64,
    pub rx_irqs: u64,
    /// Scanning (state 1): windows opened, reports stored, the engine's
    /// SCAN_REQs, and SCAN_RSPs heard.
    pub scan_windows: u64,
    pub scan_reports: u64,
    pub scan_reqs: u64,
    pub scan_responses_heard: u64,
    /// The central role (states 3 and 6): initiating windows, the engine's
    /// CONNECT_INDs, master events, the master's PDUs (data, empty,
    /// resent) and the peripheral's replies heard.
    pub init_windows: u64,
    pub connect_inds_sent: u64,
    pub master_events: u64,
    pub master_tx_data: u64,
    pub master_tx_empty: u64,
    pub master_tx_retransmit: u64,
    pub master_rx: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    AdvPdu(u8),
    AdvEnd,
    ConnOpen,
    ConnClose,
    ConnTx,
    ScanOpen,
    ScanEnd,
    MasterTx,
    MasterEnd,
}

#[derive(Clone, Debug)]
struct LastTx {
    buffer: u8,
    sn: u8,
    data: bool,
    pdu: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Conn {
    counter: u16,
    /// Anchor of the current (or next) event and the nominal next one.
    anchor: u64,
    nominal: u64,
    open: u64,
    close: u64,
    channel: u8,
    last_unmapped: u8,
    interval_slots: u64,
    nesn: u8,
    last_tx: Option<LastTx>,
    first: bool,
    /// First reception of this event (the anchor re-synchronises to it).
    received: Option<u64>,
    /// Listening for the central's next packet in this event.
    awaiting: bool,
    /// Request/response pairs done in this event, and the last central MD.
    pairs: u32,
    peer_md: bool,
    applied_instant: Option<u16>,
}

#[derive(Clone, Debug, Default)]
struct Link {
    running: bool,
    clock_value: u32,
    clock_tick: u64,
    action: Option<(u64, Action)>,
    adv_start: u64,
    connect_end: Option<u64>,
    conn: Option<Conn>,
    scan: Option<ScanWin>,
    master: Option<MasterConn>,
}

pub struct Engine {
    pub config: EngineConfig,
    pub stats: EngineStats,
    now: u64,
    control: u32,
    latches: [u32; 22],
    command_data: u32,
    command_result: u32,
    columns: [[u32; LINKS]; COLUMNS],
    enables: u32,
    acknowledge: u32,
    pending: u32,
    group2_enables: u32,
    group2_pending: u32,
    ram_base: u32,
    ram_end: u32,
    links: [Link; LINKS],
    irq_due: Vec<(u64, usize, bool)>,
    /// Ticks at which the event (0) and RX (1) interrupts were last raised.
    raised: [Option<u64>; 2],
    expect: Option<u64>,
    random: u32,
    central: Option<Central>,
    /// Devices advertising on the virtual air (scanning hears them).
    pub advertisers: Vec<Advertiser>,
    /// Connectable BLE-MIDI peripherals the FM-1 can connect to as central.
    pub peripherals: Vec<Peripheral>,
    pending_master: [Option<MasterSetup>; LINKS],
    pending_rx: Vec<PendingRx>,
    air: Vec<AirRecord>,
    air_count: u64,
    notes: Vec<String>,
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            config: EngineConfig::default(),
            stats: EngineStats::default(),
            now: 0,
            control: 0,
            latches: [0; 22],
            command_data: 0,
            command_result: 0,
            columns: [[0; LINKS]; COLUMNS],
            enables: 0,
            acknowledge: 0,
            pending: 0,
            group2_enables: 0,
            group2_pending: 0,
            ram_base: 0,
            ram_end: 0,
            links: Default::default(),
            irq_due: Vec::new(),
            raised: [None; 2],
            expect: None,
            random: 0,
            central: None,
            advertisers: Vec::new(),
            peripherals: Vec::new(),
            pending_master: Default::default(),
            pending_rx: Vec::new(),
            air: Vec::new(),
            air_count: 0,
            notes: Vec::new(),
        }
    }
}

type Res<T> = Result<T, &'static str>;

impl Engine {
    // ---- host API -------------------------------------------------------

    pub fn attach_central(&mut self, central: Central) {
        self.central = Some(central);
    }
    pub fn central(&self) -> Option<&Central> {
        self.central.as_ref()
    }
    pub fn central_mut(&mut self) -> Option<&mut Central> {
        self.central.as_mut()
    }
    pub fn air_log(&self) -> &[AirRecord] {
        &self.air
    }
    /// Packets on the air so far (the log keeps the first AIR_LOG_LIMIT).
    pub fn air_count(&self) -> u64 {
        self.air_count
    }
    /// Model diagnostics: connection set-up timing, mismatches.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
    /// Tick at which the event (`rx` false, IRQ 45) or RX (IRQ 29)
    /// interrupt was last raised.
    pub fn raised_at(&self, rx: bool) -> Option<u64> {
        self.raised[rx as usize]
    }
    /// The link's state field (column 2 bits [14:12], §2.5).
    pub fn link_state(&self, link: usize) -> u32 {
        (self.columns[2][link] >> 12) & 7
    }
    pub fn link_running(&self, link: usize) -> bool {
        self.links[link].running
    }
    /// The baseband RAM window (`0x2FC84`, `0x2FCBC`).
    pub(crate) fn set_ram_window(&mut self, base: Option<u32>, end: Option<u32>) {
        if let Some(base) = base {
            self.ram_base = base;
        }
        if let Some(end) = end {
            self.ram_end = end;
        }
    }

    fn note(&mut self, text: String) {
        if self.notes.len() < NOTE_LIMIT {
            self.notes
                .push(format!("{:.6} {text}", self.now as f64 / 24e6));
        }
    }

    fn log(&mut self, record: AirRecord) {
        self.air_count += 1;
        if self.air.len() < AIR_LOG_LIMIT {
            self.air.push(record);
        }
    }

    // ---- interrupts (§2.1 0x28028/2C/30/4C/50 [M:t]/[M:s], §10) ---------

    /// IRQ 45: event pending and enabled (bit n), or the second group.
    pub fn event_irq(&self) -> bool {
        self.enables & self.pending & 0xFF != 0
            || self.group2_enables & self.group2_pending & 0xFF != 0
    }
    /// IRQ 29: RX pending and enabled (bit 8+n).
    pub fn rx_irq(&self) -> bool {
        self.enables & self.pending & 0xFF00 != 0
    }

    fn raise(&mut self, link: usize, rx: bool) {
        self.pending |= 1 << (link + if rx { 8 } else { 0 });
        self.raised[rx as usize] = Some(self.now);
        if rx {
            self.stats.rx_irqs += 1;
        } else {
            self.stats.event_irqs += 1;
        }
    }

    // ---- registers ------------------------------------------------------

    pub(crate) fn owns(address: u32) -> bool {
        (0x28000..=0x28057).contains(&address)
    }

    /// §2.1: 0x2803C–0x28048 and 0x28054 belong to the vendor's low-power
    /// and multi-link code, untouched by both oracles; their meaning is
    /// unknown (L), so accesses fault visibly instead of pretending.
    fn unmodeled(address: u32) -> bool {
        (0x2803C..=0x28048).contains(&address) || address == 0x28054
    }

    pub(crate) fn read(&self, address: u32, size: usize) -> Res<u32> {
        if size != 4 {
            return Err("BLE baseband registers require word accesses");
        }
        if Self::unmodeled(address) {
            return Err("BLE low-power/multi-link registers are not modeled");
        }
        Ok(match address {
            0x28000 => self.control,
            0x28024 => self.command_result,
            0x2801C | 0x28020 => return Err("BLE column command/data readback is not modeled"),
            0x28028 => self.enables,
            // §2.1: read-back semantics unknown; the trace's reads returned
            // the last value written, which the latch keeps.
            0x2802C => self.acknowledge,
            0x28030 => self.pending,
            // §2.1 [M:s]: bit1 busy (polled to 0 after a link is disabled);
            // the model completes every radio action instantly, so never
            // busy. bit7 is never raised by the model.
            0x28038 => 0,
            0x2804C => self.group2_enables,
            0x28050 => self.group2_pending,
            _ => self.latches[((address - 0x28000) / 4) as usize],
        })
    }

    pub(crate) fn write(&mut self, address: u32, value: u32, size: usize) -> Res<()> {
        if size != 4 {
            return Err("BLE baseband registers require word accesses");
        }
        if Self::unmodeled(address) {
            return Err("BLE low-power/multi-link registers are not modeled");
        }
        match address {
            0x28000 => self.control = value,
            0x28020 => self.command_data = value,
            0x2801C => self.command(value)?,
            0x28024 | 0x28030 | 0x28050 => {
                return Err("BLE baseband status registers are read-only")
            }
            0x28028 => self.enables = value & 0xFFFF,
            0x2802C => {
                // §2.1 [M:s]/[I]: write 1 to acknowledge.
                self.acknowledge = value;
                self.pending &= !value;
            }
            0x28038 => {
                // §2.1 [M:s]: the event ISR acknowledges bit7 by writing bit6.
                if value & !0x40 != 0 {
                    return Err("unsupported BLE baseband status write");
                }
            }
            0x2804C => {
                // §2.1 [M:s]/[I]: bits 0–7 enable, bit 8+n acknowledge.
                self.group2_enables = value;
                self.group2_pending &= !(value >> 8);
            }
            _ => self.latches[((address - 0x28000) / 4) as usize] = value,
        }
        Ok(())
    }

    /// The link-column command (§2.1 0x2801C [M:t] H, §2.3).
    fn command(&mut self, value: u32) -> Res<()> {
        let column = (value >> 10) as usize;
        let link = ((value >> 4) & 63) as usize;
        if column >= COLUMNS || link >= LINKS {
            return Err("BLE column command outside columns 0-16 / links 0-7");
        }
        match value & 15 {
            5 => {
                let data = self.command_data & 0xFFFF;
                self.columns[column][link] = data;
                self.column_written(link, column, data);
            }
            2 => self.command_result = self.column_value(link, column),
            _ => return Err("unsupported BLE column command"),
        }
        Ok(())
    }

    /// The link clock: a slot counter started from columns 0/14 (§2.3
    /// "reading columns 0 and 14 gives a 24-bit running slot count" [M:s]).
    fn link_clock(&self, link: usize) -> u32 {
        let l = &self.links[link];
        if !l.running {
            return l.clock_value;
        }
        (l.clock_value + ((self.now - l.clock_tick) / SLOT) as u32) & 0xFF_FFFF
    }

    fn column_value(&self, link: usize, column: usize) -> u32 {
        match column {
            0 => self.link_clock(link) & 0xFFFF,
            14 => {
                (self.link_clock(link) >> 16) & 0xFF
                    | if self.links[link].running { 0x8000 } else { 0 }
            }
            _ => self.columns[column][link],
        }
    }

    fn column_written(&mut self, link: usize, column: usize, data: u32) {
        match column {
            // Column 14 bit15 starts the link, 0 stops it (§2.3 [M:t]/[M:s]).
            14 if data & 0x8000 != 0 => self.start_link(link, data),
            14 => {
                let l = &mut self.links[link];
                l.clock_value = self.columns[0][link] | (data & 0xFF) << 16;
                if l.running {
                    self.note(format!("link {link} stopped (column 14 = 0)"));
                }
                self.links[link] = Link {
                    clock_value: self.links[link].clock_value,
                    ..Link::default()
                };
            }
            // Column 2 state 7 after a CONNECT_IND: the peripheral connection
            // (§7 step 9 [M:s]).
            2 if (data >> 12) & 7 == 7 && self.links[link].running => self.begin_connection(link),
            // Column 2 state 6 after the engine's CONNECT_IND: the master
            // (§21.3 "Switch to master" [IR]).
            2 if (data >> 12) & 7 == 6 => self.begin_master(link),
            2 if (data >> 12) & 7 != 3 => self.pending_master[link] = None,
            _ => {}
        }
    }

    fn start_link(&mut self, link: usize, data: u32) {
        let state = self.link_state(link);
        let clock = self.columns[0][link] | (data & 0xFF) << 16;
        self.links[link] = Link {
            running: true,
            clock_value: clock,
            clock_tick: self.now,
            ..Link::default()
        };
        match state {
            // §6 step 12: "column 0 = start slot − 1" [M:t]/[I]: the first
            // advertising event one slot after the start.
            2 => {
                let start = self.now + SLOT;
                self.links[link].adv_start = start;
                self.links[link].action = Some((start, Action::AdvPdu(0)));
                self.note(format!("link {link} advertising started"));
            }
            1 => self.start_scan(link),
            3 => self.start_initiating(link),
            _ => self.note(format!(
                "link {link} started in state {state}: only scanning (1), advertising (2), \
                 initiating (3) and the connections (6 and 7, after a CONNECT_IND) are modeled"
            )),
        }
    }

    // ---- time -----------------------------------------------------------

    fn next_time(&self) -> Option<u64> {
        let links = self.links.iter().filter_map(|l| l.action.map(|(t, _)| t));
        let irqs = self.irq_due.iter().map(|(t, _, _)| *t);
        let central = self.central.as_ref().and_then(|c| c.next_tx_tick());
        links
            .chain(irqs)
            .chain(central)
            .chain(self.expect)
            .chain(self.next_pending_tick())
            .chain(self.next_advertiser_tick())
            .chain(self.next_peripheral_tick())
            .min()
            .map(|t| t.max(self.now))
    }

    /// Ticks until the engine or the central next does something.
    pub(crate) fn ticks_to_event(&self) -> Option<u64> {
        self.next_time().map(|t| (t - self.now).max(1))
    }

    pub(crate) fn advance(&mut self, ticks: u32, ram: &mut [u8]) -> Res<()> {
        let target = self.now + ticks as u64;
        while let Some(t) = self.next_time().filter(|&t| t <= target) {
            self.now = t;
            let mut w = Window {
                ram: &mut *ram,
                base: self.ram_base,
                end: self.ram_end,
            };
            self.step(&mut w)?;
        }
        self.now = target;
        Ok(())
    }

    /// Run everything due at `now`, in a fixed order: interrupts, link
    /// actions, then the central.
    fn step(&mut self, w: &mut Window) -> Res<()> {
        let now = self.now;
        let mut due: Vec<_> = self
            .irq_due
            .iter()
            .filter(|e| e.0 <= now)
            .copied()
            .collect();
        if !due.is_empty() {
            self.irq_due.retain(|e| e.0 > now);
            due.sort();
            for (_, link, rx) in due {
                self.raise(link, rx);
            }
            return Ok(());
        }
        for link in 0..LINKS {
            if let Some((t, action)) = self.links[link].action {
                if t <= now {
                    self.links[link].action = None;
                    return self.act(link, action, w);
                }
            }
        }
        if self.store_pending(w)? {
            return Ok(());
        }
        if self.next_advertiser_tick().is_some_and(|t| t <= now) {
            return self.advertiser_sends(w);
        }
        if self.next_peripheral_tick().is_some_and(|t| t <= now) {
            return self.peripheral_step(w);
        }
        if self
            .central
            .as_ref()
            .and_then(|c| c.next_tx_tick())
            .is_some_and(|t| t <= now)
        {
            return self.central_transmits(w);
        }
        if self.expect.is_some_and(|t| t <= now) {
            self.expect = None;
            if let Some(c) = self.central.as_mut() {
                c.on_no_response(now);
            }
        }
        Ok(())
    }

    fn act(&mut self, link: usize, action: Action, w: &mut Window) -> Res<()> {
        let block = w
            .link(link)?
            .ok_or("a running BLE link has no control block in the instance table")?;
        match action {
            Action::AdvPdu(index) => self.adv_pdu(link, block, index, w),
            Action::AdvEnd => {
                self.adv_end(link);
                Ok(())
            }
            Action::ConnOpen => self.conn_open(link, block, w),
            Action::ConnClose => {
                self.conn_end(link);
                Ok(())
            }
            Action::ConnTx => self.conn_tx(link, block, w),
            Action::ScanOpen => self.scan_open(link, block, w),
            Action::ScanEnd => {
                self.scan_end(link);
                Ok(())
            }
            Action::MasterTx => self.master_tx(link, block, w),
            Action::MasterEnd => {
                self.master_end(link);
                Ok(())
            }
        }
    }
}
