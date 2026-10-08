// SPDX-License-Identifier: GPL-3.0-only
//! The virtual central's script: one step at a time, each waiting for its
//! procedure to complete (or to time out) before the next starts.

/// Optional CONNECT_IND / connection-update fields (`None`: the default).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkParams {
    pub interval: Option<u16>,
    pub latency: Option<u16>,
    pub timeout: Option<u16>,
    pub win_size: Option<u8>,
    pub win_offset: Option<u16>,
    pub hop: Option<u8>,
    pub channel_map: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Collect `count` ADV_IND reports from the target, and (active
    /// scanning) its SCAN_RSP.
    Scan(u32),
    /// Send a CONNECT_IND at the target's next connectable PDU; done when
    /// the peripheral first answers.
    Connect(LinkParams),
    /// Let `n` connection events pass.
    Wait(u32),
    /// LL_VERSION_IND, done on the peripheral's LL_VERSION_IND.
    Version,
    /// LL_FEATURE_REQ, done on LL_FEATURE_RSP (or a reject).
    Features,
    /// ATT Exchange MTU with this client MTU.
    Mtu(u16),
    /// Primary services, characteristics and descriptors, in the order
    /// CoreBluetooth asks for them.
    Discover,
    /// Read the BLE-MIDI characteristic (the BLE-MIDI spec asks centrals to).
    ReadMidi,
    /// Write 0x0001 to the CCCD of the BLE-MIDI characteristic (or of the
    /// first notifying characteristic when there is no MIDI service).
    Subscribe,
    /// Write Without Response of these bytes to the BLE-MIDI characteristic.
    Midi(Vec<u8>),
    /// Wait until `n` notifications have arrived in total.
    ExpectNotifications(u32),
    /// LL_CONNECTION_UPDATE_IND; done after its instant.
    Update(LinkParams),
    /// LL_CHANNEL_MAP_IND; done after its instant.
    ChannelMap(u64),
    /// LL_TERMINATE_IND (remote user terminated).
    Terminate,
    /// Stop transmitting without a terminate (link loss), then wait for the
    /// peripheral to advertise again.
    Vanish,
}

/// The script CoreBluetooth-like centrals follow, then a MIDI exchange,
/// the LL procedures, and a disconnect.
pub fn default_script() -> Vec<Step> {
    parse(DEFAULT).expect("the default script parses")
}

pub const DEFAULT: &str = "\
scan 2
connect
version
features
mtu 527
discover
read
subscribe
midi 80 80 90 3c 64
wait 40
midi 80 80 80 3c 00
wait 40
chmap 1ff0ffff0f
wait 20
update interval=12 latency=0 timeout=200
wait 40
terminate
";

/// Parse a script: one step per line, `#` comments, `key=value` options.
pub fn parse(text: &str) -> Result<Vec<Step>, String> {
    text.lines()
        .enumerate()
        .filter_map(|(n, line)| {
            let line = line.split('#').next().unwrap_or("").trim();
            (!line.is_empty()).then(|| parse_line(line).map_err(|e| format!("line {}: {e}", n + 1)))
        })
        .collect()
}

fn number<T: TryFrom<u64>>(text: &str) -> Result<T, String> {
    let value = match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    }
    .map_err(|_| format!("bad number {text:?}"))?;
    T::try_from(value).map_err(|_| format!("{text} is out of range"))
}

fn link_params(words: &[&str]) -> Result<LinkParams, String> {
    let mut p = LinkParams::default();
    for word in words {
        let (key, value) = word
            .split_once('=')
            .ok_or(format!("expected key=value: {word}"))?;
        match key {
            "interval" => p.interval = Some(number(value)?),
            "latency" => p.latency = Some(number(value)?),
            "timeout" => p.timeout = Some(number(value)?),
            "winsize" => p.win_size = Some(number(value)?),
            "winoffset" => p.win_offset = Some(number(value)?),
            "hop" => p.hop = Some(number(value)?),
            "chmap" => {
                p.channel_map = Some(u64::from_str_radix(value, 16).map_err(|_| "bad chmap")?)
            }
            _ => return Err(format!("unknown option {key}")),
        }
    }
    Ok(p)
}

fn parse_line(line: &str) -> Result<Step, String> {
    let words: Vec<_> = line.split_whitespace().collect();
    let arg = |i: usize| words.get(i).copied();
    Ok(match words[0] {
        "scan" => Step::Scan(arg(1).map(number).transpose()?.unwrap_or(1)),
        "connect" => Step::Connect(link_params(&words[1..])?),
        "wait" => Step::Wait(number(arg(1).ok_or("wait needs a count")?)?),
        "version" => Step::Version,
        "features" => Step::Features,
        "mtu" => Step::Mtu(arg(1).map(number).transpose()?.unwrap_or(527)),
        "discover" => Step::Discover,
        "read" => Step::ReadMidi,
        "subscribe" => Step::Subscribe,
        "midi" => Step::Midi(
            words[1..]
                .iter()
                .map(|b| u8::from_str_radix(b, 16).map_err(|_| format!("bad byte {b}")))
                .collect::<Result<_, _>>()?,
        ),
        "notifications" => Step::ExpectNotifications(number(arg(1).ok_or("needs a count")?)?),
        "update" => Step::Update(link_params(&words[1..])?),
        "chmap" => Step::ChannelMap(
            u64::from_str_radix(arg(1).ok_or("chmap needs a map")?, 16).map_err(|_| "bad map")?,
        ),
        "terminate" => Step::Terminate,
        "vanish" => Step::Vanish,
        other => return Err(format!("unknown step {other}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scripts_parse_steps_options_and_comments() {
        let s = parse("scan 3 # three\nconnect interval=6 hop=9\n\nmidi 80 80 90 3c 64\nupdate interval=0x10\nchmap 1f\n")
            .unwrap();
        assert_eq!(s[0], Step::Scan(3));
        assert_eq!(
            s[1],
            Step::Connect(LinkParams {
                interval: Some(6),
                hop: Some(9),
                ..Default::default()
            })
        );
        assert_eq!(s[2], Step::Midi(vec![0x80, 0x80, 0x90, 0x3c, 0x64]));
        assert_eq!(
            s[3],
            Step::Update(LinkParams {
                interval: Some(16),
                ..Default::default()
            })
        );
        assert_eq!(s[4], Step::ChannelMap(0x1f));
        assert!(parse("jump").is_err());
        assert!(parse("connect interval").is_err());
        assert_eq!(default_script().len(), 17);
    }
}
