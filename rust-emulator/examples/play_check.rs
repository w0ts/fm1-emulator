// SPDX-License-Identifier: GPL-3.0-only
// Headless playback script for reproducing what a user does in fm1-ui:
//   play_check FIRMWARE STEP [STEP...]
// Steps: run:SECONDS (guest time), turn:KNOB:DETENTS (SELECT ALGORITHM
// PRESETS KNOB1..KNOB4), hold:ID,ID.. (pressed, added to those held) /
// release:ID,ID.. (those let go) / release (all let go) (matrix key ids,
// notes 14..40, as fm1-ui's KEYMAP), level:SECONDS (run and print the audio level),
// wav:SECONDS:PATH (record the guest output), png:PATH, words:ADDRESS:N
// (N words, decimal), halves:ADDRESS:N (non-zero words per DMA half),
// cores (instructions per core), click:KNOB:N (N settled detents), align (to
// the next audio DMA half: pins a following hold or release to the same
// audio block at any render cost), master:VALUE (the MASTER potentiometer,
// 0..1023 as the SARADC reads it), leds:SECONDS (run and print the panel LED
// brightness over that time: 1 = lit whenever its column is scanned).
// FM1_CPU_MHZ=N sets the instruction clock (default: the firmware's);
// FM1_NESTED_IRQ=1 lets interrupts nest (USB audio builds); FM1_IDLE_SKIP=0
// steps every halted slot instead of skipping to the next device event.
// FM1_HOT=N profiles the primary core: hot:on / hot:off start and
// pause counting (without them, every step is counted), hot:print reports and
// clears; peek:SYMBOL:WORDS prints WORDS 32-bit words at an ELF symbol; the report (top N functions, their hot address ranges) uses the
// function symbols of FM1_ELF, or of FIRMWARE's .elf sibling. Stops on a guest fault and prints the last instructions with
// registers (PLAY_TRACE=N for the last N, default 40).
use fm1_emu::{
    firmware::{elf_symbols, Symbol},
    flash_state::{Persistence, Store},
    leds,
    player::{knob, Player, OSCILLATOR_HZ},
    png,
    profile::{function_of, Profile},
};
use std::{env, path::Path};

/// Function symbols for the profile: FM1_ELF, the firmware itself if it is an
/// ELF, or an .elf next to it with the same stem.
fn profile_symbols(firmware: &str) -> Result<Vec<Symbol>, String> {
    let path = match env::var("FM1_ELF") {
        Ok(path) => path.into(),
        Err(_) => Path::new(firmware).with_extension("elf"),
    };
    if !path.exists() {
        eprintln!(
            "profile: no ELF at {} (set FM1_ELF): PCs only",
            path.display()
        );
        return Ok(vec![]);
    }
    let data = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    elf_symbols(&data)
}

fn location(symbols: &[Symbol], pc: u32) -> String {
    match function_of(symbols, pc) {
        Some(f) => format!("0x{pc:08x} {}+0x{:x}", f.name, pc - f.address),
        None => format!("0x{pc:08x}"),
    }
}

/// The top `top` functions by executed instructions, then the hot ranges
/// (loops) of the first of them.
fn report(profile: &Profile, symbols: &[Symbol], top: usize) {
    // Optional: FM1_HOT_DUMP=FILE writes every executed PC and its count
    // (hex PC, decimal count per line), e.g. to annotate a disassembly.
    if let Ok(path) = env::var("FM1_HOT_DUMP") {
        let lines: String = profile
            .pcs()
            .iter()
            .map(|(pc, count)| format!("{pc:08x} {count}\n"))
            .collect();
        if let Err(error) = std::fs::write(&path, lines) {
            eprintln!("profile: {path}: {error}");
        }
    }
    let total = profile.total.max(1) as f64;
    let share = |count: u64| 100.0 * count as f64 / total;
    println!(
        "profile: {} primary-core instructions, {} ({:.2}%) in interrupt handlers, {} ({:.2}%) halted in idle",
        profile.total,
        profile.interrupt,
        share(profile.interrupt),
        profile.idle,
        share(profile.idle)
    );
    let functions = profile.by_function(symbols);
    for f in functions.iter().take(top) {
        println!("  {:6.2}% {:>12} {}", share(f.count), f.count, f.name);
    }
    println!("hot ranges (loops) of the top functions:");
    for f in functions.iter().take(top.min(10)) {
        let size = symbols
            .iter()
            .find(|s| s.address == f.address && s.name == f.name)
            .map_or(2, |s| s.size);
        for r in profile.ranges(f.address, f.address + size).iter().take(4) {
            if share(r.count) < 0.5 {
                break;
            }
            println!(
                "  {:6.2}% {:>12} {} .. +0x{:x} ({} B), peak {}",
                share(r.count),
                r.count,
                location(symbols, r.start),
                r.end - f.address,
                r.end + 2 - r.start,
                r.peak
            );
        }
    }
}

/// Panel names of the matrix key ids (buttons 0..13, then the note keys).
const BUTTONS: [&str; 14] = [
    "OCT-", "OCT+", "FX", "SEL", "ENV", "LFO", "EDIT", "GLO", "HOME", "SAVE", "ARP", "SEQ", "PLAY",
    "REC",
];

/// Run `seconds` in the UI's 20 ms LED windows and print, per lit key, the
/// range of the raw window level and of the filtered level (what fm1-ui
/// shows), after the filter's first 100 ms.
fn print_levels(player: &mut Player, seconds: f64) -> Result<(), String> {
    let mut filter = leds::LevelFilter::default();
    let mut range: Vec<Option<[f32; 4]>> = vec![None; leds::KEYS];
    let windows = (seconds / 0.02).round() as usize;
    for window in 0..windows {
        player.run_seconds(0.02)?;
        let leds = &mut player.cpu.bus.devices.gpio.leds;
        let ticks = leds.window();
        let sums = leds.take_sums();
        let level = leds::by_key(&filter.push(&sums, ticks));
        let raw = leds::by_key(&sums.brightness());
        if window < 5 {
            continue;
        }
        for (id, slot) in range.iter_mut().enumerate() {
            let [lo, hi, flo, fhi] = slot.unwrap_or([f32::MAX, 0., f32::MAX, 0.]);
            *slot = Some([lo.min(raw[id]), hi.max(raw[id]), flo.min(level[id]), fhi.max(level[id])]);
        }
    }
    for (id, slot) in range.iter().enumerate() {
        if let Some([lo, hi, flo, fhi]) = slot.filter(|r| r[1] > 0.) {
            let name = BUTTONS.get(id).map_or(format!("key{}", id.wrapping_sub(14)), |n| n.to_string());
            println!("  {name:6} raw {lo:.3}..{hi:.3} (spread {:.3})  filtered {flo:.3}..{fhi:.3} (spread {:.3})", hi - lo, fhi - flo);
        }
    }
    Ok(())
}

/// The LED brightness array (`[column][line]`, line = row bit PA5..PA8),
/// then the lit LEDs by key.
fn print_leds(brightness: &leds::Brightness) {
    println!("  leds [column][PA5 PA6 PA7 PA8]:");
    for (column, levels) in brightness.iter().enumerate() {
        let levels: Vec<_> = levels.iter().map(|l| format!("{l:.3}")).collect();
        println!("    {column:2}: {}", levels.join(" "));
    }
    let lit: Vec<_> = leds::by_key(brightness)
        .iter()
        .enumerate()
        .filter(|(_, &level)| level > 0.)
        .map(|(id, level)| match BUTTONS.get(id) {
            Some(name) => format!("{name}={level:.3}"),
            None => format!("key{}={level:.3}", id - 14),
        })
        .collect();
    println!(
        "  lit: {}",
        if lit.is_empty() {
            "none".into()
        } else {
            lit.join(" ")
        }
    );
}

fn seconds(step: &str, text: &str) -> Result<f64, String> {
    text.parse().map_err(|_| format!("bad {step}"))
}

/// hold:IDS presses the keys IDS (the others stay as they are), release:IDS
/// lets go of them only: a key pressed twice under a held one (SAVE + a key,
/// then that key again) needs its own release in between.
fn set_keys(held: &mut [bool; 41], step: &str, ids: &str, down: bool) -> Result<(), String> {
    for id in ids.split(',') {
        let id: usize = id.parse().map_err(|_| format!("bad {step}"))?;
        *held.get_mut(id).ok_or(format!("bad key {id}"))? = down;
    }
    Ok(())
}

/// Run `seconds` of guest time and write the audio DMA output to a 24-bit
/// stereo 44.1 kHz WAV (exactly the samples the guest produced).
fn record(player: &mut Player, seconds: f64, path: &str) -> Result<(), String> {
    let mut frames: Vec<[i32; 2]> = vec![];
    player.cpu.bus.audio.samples.clear();
    let slices = (seconds / 0.25).ceil().max(1.0) as u32;
    for _ in 0..slices {
        player.run_seconds(seconds / slices as f64)?;
        frames.extend(player.cpu.bus.audio.samples.drain(..));
    }
    let data = frames.len() as u32 * 6;
    let mut out = Vec::with_capacity(44 + data as usize);
    for (tag, value) in [(b"RIFF", 36 + data), (b"WAVE", 0)] {
        out.extend_from_slice(tag);
        if tag == b"RIFF" {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    for v in [1u16, 2] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in [44_100u32, 44_100 * 6] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in [6u16, 24] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for [l, r] in &frames {
        out.extend_from_slice(&l.to_le_bytes()[..3]);
        out.extend_from_slice(&r.to_le_bytes()[..3]);
    }
    std::fs::write(path, out).map_err(|error| format!("{path}: {error}"))?;
    println!("  wrote {} frames to {path}", frames.len());
    Ok(())
}

/// `[--state PATH [--fresh]]` before the firmware: the flash state to start
/// from and save to at the end (fm1-ui's `--state`). Without it a run starts
/// from the package alone and writes nothing, so measurements repeat.
fn state_options(args: &mut Vec<String>) -> Result<Option<(String, bool)>, String> {
    let mut state = None;
    let mut fresh = false;
    while let Some(first) = args.first().cloned() {
        match first.as_str() {
            "--state" if args.len() > 1 => {
                state = Some(args[1].clone());
                args.drain(..2);
            }
            "--fresh" => {
                fresh = true;
                args.remove(0);
            }
            _ => break,
        }
    }
    match (state, fresh) {
        (None, true) => Err("--fresh needs --state PATH".into()),
        (state, fresh) => Ok(state.map(|state| (state, fresh))),
    }
}

fn main() -> Result<(), String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let state = state_options(&mut args)?;
    let (firmware, steps) = args
        .split_first()
        .ok_or("usage: play_check [--state PATH [--fresh]] FIRMWARE STEP...")?;
    let trace = env::var("PLAY_TRACE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let mut player = Player::boot(Path::new(firmware), trace)?;
    let mut persistence = state.map(|(state, fresh)| {
        let store = Store::for_firmware(Some(Path::new(&state)), Path::new(firmware));
        let (persistence, message) = Persistence::attach(
            store,
            &mut player.cpu.bus,
            Path::new(firmware),
            player.code_end,
            fresh,
        );
        println!("{message}");
        persistence
    });
    let result = play(&mut player, firmware, steps);
    if let Some(persistence) = &mut persistence {
        match persistence.flush(&player.cpu.bus) {
            Some(Ok(())) => println!(
                "flash state: saved {} sectors to {}",
                player.cpu.bus.nor_tracked_sectors().len(),
                persistence.store.image.display()
            ),
            Some(Err(error)) => eprintln!("flash state: not saved: {error}"),
            None => println!("flash state: no flash writes, nothing to save"),
        }
    }
    result
}

fn play(player: &mut Player, firmware: &str, steps: &[String]) -> Result<(), String> {
    player.cpu.nested_irqs = env::var("FM1_NESTED_IRQ").is_ok_and(|v| v == "1");
    player.cpu.idle_skip = env::var("FM1_IDLE_SKIP").map_or(true, |v| v != "0");
    player.cpu.spin_skip = env::var("FM1_SPIN_SKIP").map_or(true, |v| v != "0");
    player.cpu.spin_log = env::var("FM1_SPIN_LOG").is_ok_and(|v| v == "1");
    // Optional: FM1_CPU_MHZ=N issues one instruction per N MHz of guest time.
    if let Ok(mhz) = env::var("FM1_CPU_MHZ") {
        let mhz: u32 = mhz.parse().map_err(|_| "invalid FM1_CPU_MHZ")?;
        player
            .cpu
            .bus
            .set_instruction_clock(Some(mhz.max(1) * 1_000_000));
    }
    // Optional: FM1_HOT=N profiles the primary core (see the top).
    let hot_top: usize = env::var("FM1_HOT")
        .ok()
        .map(|value| value.parse().map_err(|_| "invalid FM1_HOT"))
        .transpose()?
        .unwrap_or(0);
    let wants_symbols = hot_top > 0 || steps.iter().any(|step| step.starts_with("peek:"));
    let symbols = if wants_symbols {
        profile_symbols(firmware)?
    } else {
        vec![]
    };
    let mut paused = None;
    if hot_top > 0 {
        let profile = Profile::new();
        if steps.iter().any(|step| step == "hot:on") {
            paused = Some(profile);
        } else {
            player.profile = Some(profile);
        }
    }
    for step in steps {
        let started = (std::time::Instant::now(), player.cpu.bus.oscillator_ticks());
        let parts: Vec<&str> = step.split(':').collect();
        let result = match parts.as_slice() {
            ["run", s] => player.run_seconds(seconds(step, s)?),
            ["turn", name, detents] => {
                let detents: i32 = detents.parse().map_err(|_| format!("bad {step}"))?;
                player.encoders.turn(knob(name)?, detents);
                Ok(())
            }
            ["hold", ids] => set_keys(&mut player.held, step, ids, true),
            ["release", ids] => set_keys(&mut player.held, step, ids, false),
            ["release"] => {
                player.held = [false; 41];
                Ok(())
            }
            ["wav", s, path] => record(player, seconds(step, s)?, path),
            ["click", name, clicks] => {
                // One detent at a time, each settled (as preset_sweep): a
                // multi-detent turn can skip or add steps in the firmware.
                let clicks: i32 = clicks.parse().map_err(|_| format!("bad {step}"))?;
                let id = knob(name)?;
                (0..clicks.abs()).try_for_each(|_| {
                    player.encoders.turn(id, clicks.signum());
                    player.settle_encoders(1.0)?;
                    player.run_seconds(0.3)
                })
            }
            ["align"] => {
                // Run to the next audio DMA half boundary (the start of the
                // audio ISR's burst). A key held or released here completes
                // its debounce 2-4 ms (press) or 8-10 ms (release) later, in
                // the gap between two bursts at any CPU load: the note lands
                // on the same audio block whatever the render costs.
                let start = player.cpu.bus.audio.halves;
                let mut waited = 0u64;
                while player.cpu.bus.audio.halves == start {
                    player.run(64)?;
                    waited += 64;
                    if waited > 1_000_000_000 {
                        return Err("align: the audio DMA is not running".into());
                    }
                }
                Ok(())
            }
            ["master", value] => {
                // The MASTER potentiometer as the SARADC reads it (0..1023).
                let value: u16 = value.parse().map_err(|_| format!("bad {step}"))?;
                if value > 1023 {
                    return Err(format!("bad {step}: 0..1023"));
                }
                player.cpu.bus.devices.adc.master = value;
                Ok(())
            }
            ["cores"] => {
                // Instructions per core and guest time so far.
                let c = &player.cpu;
                println!(
                    "  cores: {} instructions (CPU0 {}, CPU1 {}), {} ticks of guest time; secondary pc {:?}",
                    c.steps,
                    c.core_steps[0],
                    c.core_steps[1],
                    c.bus.oscillator_ticks(),
                    c.secondary_pc().map(|pc| format!("0x{pc:08x}"))
                );
                Ok(())
            }
            ["words", base, words] => {
                // 32-bit words of guest memory (decimal), e.g. a firmware's counters.
                let base = u32::from_str_radix(base.trim_start_matches("0x"), 16)
                    .map_err(|_| format!("bad {step}"))?;
                let words: u32 = words.parse().map_err(|_| format!("bad {step}"))?;
                let values: Vec<String> = (0..words)
                    .map(|w| {
                        player
                            .cpu
                            .bus
                            .read(base + w * 4, 4)
                            .map_or("?".into(), |v| v.to_string())
                    })
                    .collect();
                println!("  0x{base:08x}: {}", values.join(" "));
                Ok(())
            }
            ["halves", base, words] => {
                // Non-zero words in each half of a DMA double buffer.
                let base = u32::from_str_radix(base.trim_start_matches("0x"), 16)
                    .map_err(|_| format!("bad {step}"))?;
                let words: u32 = words.parse().map_err(|_| format!("bad {step}"))?;
                for half in 0..2 {
                    let nonzero = (0..words)
                        .filter(|w| {
                            player
                                .cpu
                                .bus
                                .read(base + (half * words + w) * 4, 4)
                                .unwrap_or(0)
                                != 0
                        })
                        .count();
                    println!("  half {half}: {nonzero} of {words} words non-zero");
                }
                Ok(())
            }
            ["level", s] => player.level(seconds(step, s)?).map(|level| {
                println!(
                    "  audio: {} frames, rms {:.4}, peak {:.4} (full scale 1.0)",
                    level.frames,
                    level.rms(),
                    level.peak
                );
            }),
            ["hot", "on"] => {
                player.profile = player
                    .profile
                    .take()
                    .or(paused.take())
                    .or(Some(Profile::new()));
                Ok(())
            }
            ["hot", "off"] => {
                paused = player.profile.take().or(paused.take());
                Ok(())
            }
            ["hot", "print"] => {
                if let Some(profile) = player.profile.as_mut().or(paused.as_mut()) {
                    report(profile, &symbols, hot_top.max(1));
                    profile.clear();
                }
                Ok(())
            }
            ["peek", name, words] => {
                let words: u32 = words.parse().map_err(|_| format!("bad {step}"))?;
                let symbol = symbols
                    .iter()
                    .find(|s| s.name == *name)
                    .ok_or(format!("no symbol {name}"))?;
                let values: Vec<String> = (0..words)
                    .map(|w| {
                        let value = player.cpu.bus.read(symbol.address + w * 4, 4).unwrap_or(0);
                        format!("{value}")
                    })
                    .collect();
                println!("  {name} @ 0x{:08x}: {}", symbol.address, values.join(" "));
                Ok(())
            }
            ["png", path] => {
                let bytes = png::encode_rgb(240, 240, &player.cpu.bus.lcd.pixels)?;
                std::fs::write(path, bytes).map_err(|error| format!("{path}: {error}"))
            }
            ["leds", s] => {
                player.cpu.bus.devices.gpio.leds.take();
                player.run_seconds(seconds(step, s)?)?;
                print_leds(&player.cpu.bus.devices.gpio.leds.take());
                Ok(())
            }
            ["levels", s] => {
                player.cpu.bus.devices.gpio.leds.take();
                print_levels(player, seconds(step, s)?)
            }
            _ => return Err(format!("unknown step {step}")),
        };
        if let Err(report) = result {
            // The trace goes to stdout; the fault line is the error.
            let (trace, fault) = report.rsplit_once('\n').unwrap_or(("", &report));
            println!("{trace}");
            return Err(fault.to_string());
        }
        let guest = (player.cpu.bus.oscillator_ticks() - started.1) as f64 / OSCILLATOR_HZ;
        if guest > 0.0 {
            let host = started.0.elapsed().as_secs_f64();
            println!(
                "{step}: ok ({} instructions; {guest:.2} s guest in {host:.2} s: {:.2}x real time)",
                player.cpu.steps,
                guest / host
            );
        } else {
            println!("{step}: ok ({} instructions)", player.cpu.steps);
        }
    }
    if let Some(profile) = player.profile.as_ref().or(paused.as_ref()) {
        if profile.total > 0 {
            report(profile, &symbols, hot_top);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::set_keys;

    #[test]
    fn release_with_ids_lets_go_of_those_keys_only() {
        // SAVE (9) held, a key (21) pressed, then that key let go and pressed
        // again with SAVE still held: two presses of 21 the guest can see.
        let mut held = [false; 41];
        set_keys(&mut held, "hold:9,21", "9,21", true).unwrap();
        set_keys(&mut held, "release:21", "21", false).unwrap();
        assert!(held[9] && !held[21]);
        set_keys(&mut held, "hold:21", "21", true).unwrap();
        assert!(held[9] && held[21]);
    }

    #[test]
    fn bad_ids_are_errors() {
        let mut held = [false; 41];
        assert!(set_keys(&mut held, "release:41", "41", false).is_err());
        assert!(set_keys(&mut held, "hold:x", "x", true).is_err());
    }
}
