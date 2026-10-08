// SPDX-License-Identifier: GPL-3.0-only
//! A scripted virtual BLE central on the emulator's virtual air
//! (`ble_air`): it scans, sends SCAN_REQ and CONNECT_IND, runs the master
//! side of a connection with its own clock and CSA #1 schedule (so the
//! peripheral only hears it when the engine model listens at the right
//! time on the right channel), the LL control procedures, and an
//! L2CAP/ATT client that discovers GATT the way CoreBluetooth does.
//!
//! Built from the Bluetooth Core specification and the BLE-MIDI
//! specification only.

mod gatt;
mod link;
pub mod script;

pub use gatt::{
    Characteristic, Descriptor, Notification, Service, Uuid, MIDI_CHARACTERISTIC, MIDI_SERVICE,
};
pub use script::{LinkParams, Step};

use crate::ble_air::{adv, airtime_ticks, ConnectParams, TICKS_PER_US, UNIT_1250_TICKS};

/// What the central puts on the air at a connection event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CentralTx {
    pub channel: u8,
    pub access_address: u32,
    pub crc_init: u32,
    pub event: u16,
    pub pdu: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct CentralConfig {
    /// Own address (random static, sent with TxAdd = 1), LSB first.
    pub address: [u8; 6],
    /// Only this advertiser (LSB first); None: the first connectable one.
    pub target: Option<[u8; 6]>,
    pub active_scan: bool,
    /// CONNECT_IND defaults; a `connect` step may override fields.
    pub connect: ConnectParams,
    /// Where in the transmit window the first packet goes (µs).
    pub window_delay_us: u32,
    pub mtu: u16,
    /// LL features offered in LL_FEATURE_REQ/RSP (Core Vol 6 Part B 4.6).
    pub features: u64,
    /// Answer an SMP Security/Pairing Request with Pairing Failed (pairing
    /// not supported). False: leave it unanswered.
    pub answer_smp: bool,
    /// Request/response pairs per connection event (MD continues an event
    /// up to this many; 1 = one pair per event).
    pub max_pairs_per_event: u32,
    /// A step that has not completed after this long fails (ms of guest time).
    pub step_timeout_ms: u32,
}

impl Default for CentralConfig {
    fn default() -> Self {
        Self {
            address: [0x11, 0x22, 0x33, 0x44, 0x55, 0xC6],
            target: None,
            active_scan: true,
            connect: ConnectParams::default(),
            window_delay_us: 0,
            mtu: 527,
            // Connection parameters request, extended reject, slave-initiated
            // features exchange, LE ping, data length extension.
            features: 0x3E,
            answer_smp: true,
            max_pairs_per_event: 1,
            step_timeout_ms: 4_000,
        }
    }
}

/// One advertising or scan-response PDU heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvReport {
    pub tick: u64,
    pub channel: u8,
    pub kind: u8,
    /// AdvA, LSB first, and TxAdd.
    pub address: [u8; 6],
    pub random: bool,
    pub data: Vec<u8>,
}

/// The outcome of one script step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepResult {
    pub step: Step,
    pub tick: u64,
    pub outcome: Result<String, String>,
}

pub struct Central {
    pub config: CentralConfig,
    script: Vec<Step>,
    step: usize,
    /// Tick and connection-event count when the current step started.
    started: Option<(u64, u64)>,
    results: Vec<StepResult>,
    adv_reports: Vec<AdvReport>,
    scan_responses: Vec<AdvReport>,
    link: Option<link::Link>,
    gatt: gatt::Gatt,
    /// Connection events run, over all connections.
    events: u64,
    closed: Option<String>,
    log: Vec<String>,
    now: u64,
}

const REPORT_LIMIT: usize = 4_096;

impl Central {
    pub fn new(config: CentralConfig, script: Vec<Step>) -> Self {
        let mtu = config.mtu;
        Self {
            config,
            script,
            step: 0,
            started: None,
            results: Vec::new(),
            adv_reports: Vec::new(),
            scan_responses: Vec::new(),
            link: None,
            gatt: gatt::Gatt::new(mtu),
            events: 0,
            closed: None,
            log: Vec::new(),
            now: 0,
        }
    }

    // ---- observation API ------------------------------------------------

    pub fn adv_reports(&self) -> &[AdvReport] {
        &self.adv_reports
    }
    pub fn scan_responses(&self) -> &[AdvReport] {
        &self.scan_responses
    }
    pub fn results(&self) -> &[StepResult] {
        &self.results
    }
    /// Human-readable progress (connection, procedures, GATT).
    pub fn log(&self) -> &[String] {
        &self.log
    }
    pub fn connected(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.established)
    }
    /// Why the last connection ended.
    pub fn closed(&self) -> Option<&str> {
        self.closed.as_deref()
    }
    pub fn services(&self) -> &[Service] {
        &self.gatt.services
    }
    pub fn characteristics(&self) -> &[Characteristic] {
        &self.gatt.characteristics
    }
    pub fn descriptors(&self) -> &[Descriptor] {
        &self.gatt.descriptors
    }
    pub fn notifications(&self) -> &[Notification] {
        &self.gatt.notifications
    }
    /// The negotiated ATT MTU.
    pub fn att_mtu(&self) -> u16 {
        self.gatt.mtu
    }
    /// The connection event counter (None: not connected).
    pub fn event_counter(&self) -> Option<u16> {
        self.link.as_ref().map(|l| l.event)
    }
    /// All steps ran (done or failed).
    pub fn finished(&self) -> bool {
        self.step >= self.script.len()
    }
    /// Append steps (tests drive the central step by step).
    pub fn push_steps(&mut self, steps: impl IntoIterator<Item = Step>) {
        self.script.extend(steps);
    }

    fn say(&mut self, text: String) {
        if self.log.len() < REPORT_LIMIT {
            self.log
                .push(format!("{:.6} {text}", self.now as f64 / 24e6));
        }
    }

    // ---- the air (called by the engine model) ---------------------------

    /// An advertising-channel PDU from the peripheral; the reply (SCAN_REQ
    /// or CONNECT_IND) would start at `reply_at`.
    pub fn on_advertising(
        &mut self,
        tick: u64,
        channel: u8,
        pdu: &[u8],
        reply_at: u64,
    ) -> Option<Vec<u8>> {
        self.now = tick;
        let report = Self::report(tick, channel, pdu)?;
        if self.config.target.is_some_and(|t| t != report.address) {
            return None;
        }
        let connectable = matches!(report.kind, adv::ADV_IND | adv::ADV_DIRECT_IND);
        let scannable = matches!(report.kind, adv::ADV_IND | adv::ADV_SCAN_IND);
        if self.adv_reports.len() < REPORT_LIMIT {
            self.adv_reports.push(report.clone());
        }
        self.run_script();
        let header_rx = if report.random { 0x80 } else { 0 };
        match self.script.get(self.step) {
            Some(Step::Connect(params)) if connectable && self.link.is_none() => {
                let params = self.connect_params(params.clone());
                let mut reply = vec![adv::CONNECT_IND | 0x40 | header_rx, 34];
                reply.extend(self.config.address);
                reply.extend(report.address);
                reply.extend(params.lldata());
                let end = reply_at + airtime_ticks(34);
                let start = end + UNIT_1250_TICKS + params.win_offset as u64 * UNIT_1250_TICKS;
                let delay = (self.config.window_delay_us as u64 * TICKS_PER_US)
                    .min(params.win_size as u64 * UNIT_1250_TICKS);
                self.link = Some(link::Link::new(params, start + delay));
                self.gatt = gatt::Gatt::new(self.config.mtu);
                self.closed = None;
                self.say(format!(
                    "CONNECT_IND on channel {channel}: interval {} x 1.25 ms, window {} at +{} units, \
                     hop {}, map {:010x}",
                    params.interval, params.win_size, params.win_offset, params.hop, params.channel_map
                ));
                Some(reply)
            }
            Some(Step::Scan(_)) if scannable && self.config.active_scan && self.link.is_none() => {
                let mut reply = vec![adv::SCAN_REQ | 0x40 | header_rx, 12];
                reply.extend(self.config.address);
                reply.extend(report.address);
                Some(reply)
            }
            _ => None,
        }
    }

    pub fn on_scan_response(&mut self, tick: u64, channel: u8, pdu: &[u8]) {
        self.now = tick;
        if let Some(report) = Self::report(tick, channel, pdu) {
            if self.scan_responses.len() < REPORT_LIMIT {
                self.scan_responses.push(report);
            }
        }
        self.run_script();
    }

    fn report(tick: u64, channel: u8, pdu: &[u8]) -> Option<AdvReport> {
        if pdu.len() < 8 || pdu.len() - 2 != pdu[1] as usize {
            return None;
        }
        let mut address = [0; 6];
        address.copy_from_slice(&pdu[2..8]);
        Some(AdvReport {
            tick,
            channel,
            kind: pdu[0] & 15,
            address,
            random: pdu[0] & 0x40 != 0,
            data: pdu[8..].to_vec(),
        })
    }

    fn connect_params(&self, p: LinkParams) -> ConnectParams {
        let d = self.config.connect;
        ConnectParams {
            interval: p.interval.unwrap_or(d.interval),
            latency: p.latency.unwrap_or(d.latency),
            timeout: p.timeout.unwrap_or(d.timeout),
            win_size: p.win_size.unwrap_or(d.win_size),
            win_offset: p.win_offset.unwrap_or(d.win_offset),
            hop: p.hop.unwrap_or(d.hop),
            channel_map: p.channel_map.unwrap_or(d.channel_map),
            ..d
        }
    }

    // ---- the script -----------------------------------------------------

    fn finish_step(&mut self, outcome: Result<String, String>) {
        let step = self.script[self.step].clone();
        let text = match &outcome {
            Ok(text) => format!("step {:?}: ok {text}", step),
            Err(text) => format!("step {:?}: FAILED {text}", step),
        };
        self.say(text);
        self.results.push(StepResult {
            step,
            tick: self.now,
            outcome,
        });
        self.step += 1;
        self.started = None;
    }

    /// Start, poll and finish steps until one has to wait.
    fn run_script(&mut self) {
        while self.step < self.script.len() {
            let (start_tick, start_events) = match self.started {
                Some(s) => s,
                None => {
                    self.started = Some((self.now, self.events));
                    if let Err(error) = self.start_step() {
                        self.finish_step(Err(error));
                        continue;
                    }
                    (self.now, self.events)
                }
            };
            if let Some(outcome) = self.poll_step(start_events) {
                self.finish_step(outcome);
                continue;
            }
            let timeout = self.config.step_timeout_ms as u64 * 1_000 * TICKS_PER_US;
            if self.now > start_tick + timeout {
                self.finish_step(Err(format!(
                    "timed out after {} ms",
                    self.config.step_timeout_ms
                )));
                continue;
            }
            break;
        }
    }

    fn start_step(&mut self) -> Result<(), String> {
        let step = self.script[self.step].clone();
        let needs_link = !matches!(step, Step::Scan(_) | Step::Connect(_));
        if needs_link && !self.connected() {
            return Err(match &self.closed {
                Some(why) => format!("not connected ({why})"),
                None => "not connected".into(),
            });
        }
        match step {
            Step::Scan(_) | Step::Connect(_) | Step::Wait(_) | Step::ExpectNotifications(_) => {}
            Step::Version => self.send_version(),
            Step::Features => self.send_ll(
                crate::ble_air::ll::FEATURE_REQ,
                &self.config.features.to_le_bytes(),
            ),
            Step::Mtu(mtu) => self.gatt.exchange_mtu(mtu),
            Step::Discover => self.gatt.discover(),
            Step::ReadMidi => {
                let handle = self
                    .gatt
                    .midi_handle()
                    .ok_or("no BLE-MIDI characteristic discovered")?;
                self.gatt.read(handle);
            }
            Step::Subscribe => {
                let cccd = self
                    .gatt
                    .subscribe_handle()
                    .ok_or("no notifying characteristic discovered")?;
                self.gatt.write_request(cccd, &[1, 0]);
            }
            Step::Midi(bytes) => {
                let handle = self
                    .gatt
                    .midi_handle()
                    .ok_or("no BLE-MIDI characteristic discovered")?;
                self.gatt.write_command(handle, &bytes);
            }
            Step::Update(p) => self.start_update(p),
            Step::ChannelMap(map) => self.start_channel_map(map)?,
            Step::Terminate => self.send_ll(crate::ble_air::ll::TERMINATE_IND, &[0x13]),
        }
        self.flush_gatt();
        Ok(())
    }

    fn poll_step(&mut self, start_events: u64) -> Option<Result<String, String>> {
        let step = self.script[self.step].clone();
        let link = self.link.as_ref();
        let lost = || -> Option<Result<String, String>> {
            Some(Err(format!(
                "connection ended: {}",
                self.closed.as_deref().unwrap_or("?")
            )))
        };
        if !matches!(step, Step::Scan(_) | Step::Connect(_) | Step::Terminate) && link.is_none() {
            return lost();
        }
        match step {
            Step::Scan(n) => {
                let seen = self.adv_reports.len() as u32;
                let rsp = !self.config.active_scan || !self.scan_responses.is_empty();
                (seen >= n && rsp).then(|| {
                    Ok(format!(
                        "{seen} reports, {} scan responses",
                        self.scan_responses.len()
                    ))
                })
            }
            Step::Connect(_) => match link {
                Some(l) if l.established => {
                    Some(Ok(format!("first peripheral PDU at event {}", l.event)))
                }
                None if self.closed.is_some() => lost(),
                _ => None,
            },
            Step::Wait(n) => (self.events >= start_events + n as u64).then(|| Ok(String::new())),
            Step::ExpectNotifications(n) => (self.gatt.notifications.len() as u32 >= n)
                .then(|| Ok(format!("{n} notifications"))),
            Step::Version => link?
                .peer_version
                .map(|(v, c, s)| Ok(format!("version {v}, company 0x{c:04x}, sub 0x{s:04x}"))),
            Step::Features => link?
                .peer_features
                .clone()
                .map(|f| f.map(|f| format!("features 0x{f:016x}"))),
            Step::Mtu(_) | Step::Discover | Step::ReadMidi | Step::Subscribe => {
                self.gatt.take_done()
            }
            Step::Midi(_) => link?.idle().then(|| Ok("acknowledged".into())),
            Step::Update(_) | Step::ChannelMap(_) => {
                let l = link?;
                l.update
                    .is_none()
                    .then(|| Ok(format!("applied, now at event {}", l.event)))
            }
            Step::Terminate => match link {
                None => Some(Ok(self.closed.clone().unwrap_or_default())),
                Some(_) => None,
            },
        }
    }
}
