// SPDX-License-Identifier: GPL-3.0-only
// Bounded boot diagnostics using the same CPU/bus as the graphical emulator.
use fm1_emu::{cpu::Cpu, firmware::Firmware};
use std::{
    collections::{BTreeMap, VecDeque},
    env,
    io::{self, Write},
    path::Path,
    process::ExitCode,
};

fn location(symbols: &BTreeMap<String, u32>, pc: u32) -> String {
    let closest = symbols
        .iter()
        .filter(|(_, address)| **address <= pc)
        .max_by_key(|(_, address)| **address);
    match closest {
        Some((name, address)) => format!("0x{pc:08x} ({name}+0x{:x})", pc - address),
        None => format!("0x{pc:08x}"),
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = env::args().skip(1).collect();
    if !(1..=2).contains(&args.len()) {
        return Err("usage: diagnose FIRMWARE.{fwsc,elf,bin} [INSTRUCTION_LIMIT]".into());
    }
    let limit: u64 = args
        .get(1)
        .map(|s| s.parse())
        .transpose()
        .map_err(|_| "invalid instruction limit")?
        .unwrap_or(10_000_000);
    let firmware = Firmware::load(Path::new(&args[0]))?;
    let mut cpu = Cpu::new(firmware.bus()?, firmware.entry);
    cpu.r[0] = 0x01c7_fe08;
    // Optional: FM1_CPU_MHZ=N issues N million instructions per second of
    // guest time instead of following the firmware's system clock (24 is
    // the fixed-clock mode the fork's baselines were measured with).
    if let Ok(mhz) = env::var("FM1_CPU_MHZ") {
        cpu.set_cpu_mhz(mhz.parse().map_err(|_| "invalid FM1_CPU_MHZ")?)?;
    }
    // Optional: FM1_NESTED_IRQ=1 lets a higher-priority interrupt preempt a
    // handler that re-enabled interrupts (the USB audio builds need it).
    cpu.nested_irqs = env::var("FM1_NESTED_IRQ").is_ok_and(|v| v == "1");
    // Optional: FM1_WATCHDOG_OFF=1 keeps counting watchdog ticks but never
    // resets. Diagnostic experiments only; the output says so.
    let watchdog_off = env::var("FM1_WATCHDOG_OFF").is_ok_and(|v| v == "1");
    cpu.bus.system.watchdog_expiry_disabled = watchdog_off;
    let mut recent = VecDeque::new();
    // Optional: FM1_RECENT=N keeps N completed instructions with registers.
    let recent_count: usize = env::var("FM1_RECENT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(12)
        .max(1);
    let mut serial_bytes = 0u64;
    let mut stdout = io::stdout().lock();
    let mut fault = None;
    // Optional: FM1_HOT=N counts primary-core PCs over the final N steps.
    let hot_window: u64 = env::var("FM1_HOT")
        .ok()
        .map(|value| value.parse().map_err(|_| "invalid FM1_HOT"))
        .transpose()?
        .unwrap_or(0);
    let mut hot = BTreeMap::<u32, u64>::new();
    let callers_pc: Option<u32> = env::var("FM1_CALLERS")
        .ok()
        .and_then(|pc| u32::from_str_radix(pc.trim_start_matches("0x"), 16).ok());
    let mut callers = BTreeMap::<u32, u64>::new();
    // Optional: FM1_WATCH=PC[,PC] prints registers whenever CPU 0 reaches PC.
    let watch: Vec<u32> = env::var("FM1_WATCH")
        .map(|list| {
            list.split(',')
                .filter_map(|pc| u32::from_str_radix(pc.trim_start_matches("0x"), 16).ok())
                .collect()
        })
        .unwrap_or_default();
    let mut watch_hits = 0;
    // Optional: FM1_MEMWATCH=ADDRESS prints each instruction that changes the
    // aligned 32-bit word at ADDRESS (first 400 changes).
    let memwatch: Option<u32> = env::var("FM1_MEMWATCH")
        .ok()
        .map(|a| {
            u32::from_str_radix(a.trim_start_matches("0x"), 16).map_err(|_| "invalid FM1_MEMWATCH")
        })
        .transpose()?
        .map(|a| a & !3);
    let mut memwatch_value = memwatch.and_then(|a| cpu.bus.read(a, 4).ok());
    let mut memwatch_hits = 0;
    // Optional: FM1_MMIO=N counts MMIO reads/writes over the final N steps.
    let mmio_window: u64 = env::var("FM1_MMIO")
        .ok()
        .map(|value| value.parse().map_err(|_| "invalid FM1_MMIO"))
        .transpose()?
        .unwrap_or(0);
    // Optional: FM1_OP=NAME counts executions per PC of one operation name.
    let op_filter = env::var("FM1_OP").ok();
    // Optional, experiments only: FM1_STUB_MMIO=LOW-HIGH[,...] (hex) lets
    // unimplemented MMIO in those ranges read back the last word written
    // instead of faulting; the output lists every stubbed word touched.
    let stub_ranges = env::var("FM1_STUB_MMIO").ok().map(|list| {
        list.split(',')
            .filter_map(|range| {
                // LOW-HIGH[:SET[:CLEAR]]: bits set / cleared on every read.
                let mut fields = range.split(':');
                let (low, high) = fields.next()?.split_once('-')?;
                let hex = |v: &str| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok();
                Some((
                    hex(low)?,
                    hex(high)?,
                    hex(fields.next().unwrap_or("0"))?,
                    hex(fields.next().unwrap_or("0"))?,
                ))
            })
            .collect::<Vec<_>>()
    });
    if let Some(ranges) = &stub_ranges {
        cpu.bus.set_mmio_stubs_clearing(ranges.clone());
        // FM1_STUB_CONSOLE=ADDRESS: collect the bytes written there (a
        // debug UART data register inside a stubbed range).
        if let Ok(address) = env::var("FM1_STUB_CONSOLE") {
            let address = u32::from_str_radix(address.trim_start_matches("0x"), 16)
                .map_err(|_| "invalid FM1_STUB_CONSOLE")?;
            cpu.bus.stub_console = Some((address, Vec::new()));
        }
    }
    // Optional: FM1_FLASH_RESTORE=PATH (a FM1_FLASH_DUMP from an earlier run)
    // lays every 4 KiB sector that differs from the package over the flash
    // before the first instruction, as a second boot of the same device.
    if let Ok(path) = env::var("FM1_FLASH_RESTORE") {
        let saved = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let current = cpu.bus.nor_bytes().to_vec();
        if saved.len() != current.len() {
            return Err(format!("{path}: flash size differs"));
        }
        let mut restored = Vec::new();
        for offset in (0..saved.len()).step_by(4096) {
            let sector = &saved[offset..offset + 4096];
            if sector != &current[offset..offset + 4096] {
                cpu.bus.restore_nor_sector(offset, sector)?;
                restored.push(format!("0x{offset:06x}"));
            }
        }
        eprintln!("flash: restored sectors [{}]", restored.join(" "));
    }
    // Optional: FM1_MMIO_TRACE=PATH writes every MMIO write (and with
    // FM1_MMIO_READS=1 every read) inside FM1_MMIO_RANGES=LOW-HIGH[,...]
    // (hex, inclusive) in execution order, at most FM1_MMIO_TRACE_LIMIT
    // records (default 2,000,000).
    let mmio_trace_path = env::var("FM1_MMIO_TRACE").ok();
    if mmio_trace_path.is_some() {
        let ranges = env::var("FM1_MMIO_RANGES")
            .map_err(|_| "FM1_MMIO_TRACE needs FM1_MMIO_RANGES")?
            .split(',')
            .map(|range| {
                let (low, high) = range.split_once('-').ok_or("range needs LOW-HIGH")?;
                let hex = |v: &str| {
                    u32::from_str_radix(v.trim().trim_start_matches("0x"), 16)
                        .map_err(|_| "invalid FM1_MMIO_RANGES address")
                };
                Ok::<_, &str>((hex(low)?, hex(high)?))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let limit = env::var("FM1_MMIO_TRACE_LIMIT")
            .ok()
            .map(|v| v.parse().map_err(|_| "invalid FM1_MMIO_TRACE_LIMIT"))
            .transpose()?
            .unwrap_or(2_000_000);
        let reads = env::var("FM1_MMIO_READS").is_ok_and(|v| v == "1");
        cpu.bus.start_mmio_trace(ranges, reads, limit);
    }
    let ble = setup_ble(&mut cpu)?;
    let mut isr = env::var("FM1_BLE_ISR")
        .is_ok_and(|v| v == "1")
        .then(fm1_emu::isr_profile::IsrProfiler::default);
    // Optional: FM1_PRESS=STEP:COLUMN:ROW:LENGTH[,...] holds a matrix
    // contact from instruction STEP for LENGTH instructions.
    let mut presses: Vec<(u64, usize, usize, u64)> = env::var("FM1_PRESS")
        .ok()
        .map(|list| {
            list.split(',')
                .map(|p| {
                    let f: Vec<u64> = p.split(':').filter_map(|v| v.parse().ok()).collect();
                    (f.len() == 4)
                        .then(|| (f[0], f[1] as usize, f[2] as usize, f[3]))
                        .ok_or("FM1_PRESS needs STEP:COLUMN:ROW:LENGTH")
                })
                .collect::<Result<_, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let mut typist = passkey_typist()?;
    let mut op_pcs = BTreeMap::<u32, u64>::new();
    // Optional: FM1_IDLE_SKIP=0 steps every slot of a core halted in `idle`
    // instead of jumping to the next device event (same guest state).
    cpu.idle_skip = env::var("FM1_IDLE_SKIP").map_or(true, |v| v != "0");
    // Optional: FM1_SPIN_SKIP=0 also steps every instruction of a core
    // busy-waiting in a recorded spin loop (same guest state);
    // FM1_SPIN_LOG=1 prints the loops found and rejected.
    cpu.spin_skip = env::var("FM1_SPIN_SKIP").map_or(true, |v| v != "0");
    cpu.spin_log = env::var("FM1_SPIN_LOG").is_ok_and(|v| v == "1");
    // Halted spans are skipped only where no per-instruction diagnostic
    // looks, and never across the start of the FM1_HOT / FM1_MMIO windows.
    let hot_start = limit - hot_window.min(limit);
    let mmio_start = limit - mmio_window.min(limit);
    let skip_allowed = watch.is_empty() && memwatch.is_none();
    let mut next = 0;
    while next < limit {
        let step = next;
        next += 1;
        if skip_allowed && step < hot_start && cpu.may_skip() {
            let bound = [hot_start, mmio_start, limit]
                .into_iter()
                .filter(|&b| b >= step)
                .min()
                .unwrap_or(limit);
            let skipped = cpu.skip_idle_calls(bound - step);
            if skipped > 0 {
                next = step + skipped;
                continue;
            }
        }
        if let Some(t) = typist.as_mut() {
            t.poll(&mut cpu, step);
        }
        let mut contacts_changed = false;
        if let Some(i) = presses.iter().position(|p| p.0 <= step) {
            let (at, column, row, length) = presses[i];
            let down = at + length > step;
            cpu.bus.devices.gpio.press(column, row, down)?;
            if down {
                presses[i].0 = at + length;
                presses[i].3 = 0;
            } else {
                presses.remove(i);
            }
            contacts_changed = true;
        }
        let pc = cpu.pc;
        if watch.contains(&pc) && watch_hits < 400 {
            watch_hits += 1;
            let list: Vec<_> = cpu.r.iter().map(|r| format!("{r:x}")).collect();
            eprintln!(
                "watch {step}: {} sp={:x} rets={:x} [{}]",
                location(&firmware.symbols, pc),
                cpu.sr[14],
                cpu.sr[3],
                list.join(" ")
            );
        }
        if hot_window > 0 && step + hot_window >= limit {
            *hot.entry(pc).or_default() += 1;
            // Optional: FM1_CALLERS=PC counts rets at PC in the hot window.
            if callers_pc == Some(pc) {
                *callers.entry(cpu.sr[3]).or_default() += 1;
            }
        }
        if mmio_window > 0 && step + mmio_window == limit {
            cpu.bus.start_mmio_stats();
        }
        // `step` re-checks every interrupt source after a contact changed.
        let stepped = if contacts_changed {
            cpu.step()
        } else {
            cpu.step_next()
        };
        if let Some(profiler) = isr.as_mut() {
            profiler.observe(&cpu);
        }
        match stepped {
            Ok(op) => {
                if op_filter.as_deref() == Some(op) {
                    *op_pcs.entry(pc).or_default() += 1;
                }
                if recent.len() == recent_count {
                    recent.pop_front();
                }
                // The instruction word only for the verbose trace (a fetch
                // per step costs time).
                let fetched = if recent_count > 12 {
                    cpu.bus.fetch(pc).unwrap_or(0)
                } else {
                    0
                };
                let registers = if recent_count > 12 { cpu.r } else { [0; 16] };
                recent.push_back((pc, op, fetched, registers));
            }
            Err(error) => {
                fault = Some(error);
                break;
            }
        }
        if let Some(address) = memwatch {
            let value = cpu.bus.read(address, 4).ok();
            if value != memwatch_value && memwatch_hits < 400 {
                memwatch_hits += 1;
                eprintln!(
                    "memwatch {step}: {} wrote 0x{address:08x}: {:x?} -> {:x?}",
                    location(&firmware.symbols, pc),
                    memwatch_value,
                    value
                );
            }
            memwatch_value = value;
        }
        // Terminal stdout contains only real guest CDC endpoint data.
        while let Some(byte) = cpu.bus.usb.serial.pop_front() {
            stdout.write_all(&[byte]).map_err(|e| e.to_string())?;
            serial_bytes += 1;
        }
    }
    stdout.flush().map_err(|e| e.to_string())?;
    if let (Some(path), Some(trace)) = (&mmio_trace_path, cpu.bus.take_mmio_trace()) {
        let mut text = String::from("# step tick pc rw address size value\n");
        for r in &trace.records {
            text.push_str(&format!(
                "{} {} {:08x} {} {:05x} {} {:08x}{}\n",
                r.step,
                r.tick,
                r.pc,
                if r.write { 'W' } else { 'R' },
                r.address,
                r.size,
                r.value,
                if r.fault { " FAULT" } else { "" }
            ));
        }
        std::fs::write(path, text).map_err(|e| format!("{path}: {e}"))?;
        eprintln!(
            "MMIO trace: {} of {} matching accesses saved to {path}",
            trace.records.len(),
            trace.matched
        );
    }
    // Optional: FM1_FLASH_DUMP=PATH saves the raw serial flash and reports
    // the sectors programmed or erased during the run.
    if let Ok(path) = env::var("FM1_FLASH_DUMP") {
        std::fs::write(&path, cpu.bus.nor_bytes()).map_err(|e| format!("{path}: {e}"))?;
        let list: Vec<_> = cpu
            .bus
            .nor_tracked_sectors()
            .iter()
            .map(|s| format!("0x{s:06x}"))
            .collect();
        eprintln!(
            "flash: {} programs/erases; sectors touched: [{}]; saved {path}",
            cpu.bus.nor_writes(),
            list.join(" ")
        );
    }
    if ble {
        report_ble(&cpu)?;
    }
    if let Some(profiler) = &isr {
        eprint!("{}", profiler.report());
    }
    if stub_ranges.is_some() {
        let report = cpu.bus.mmio_stub_report();
        if let Some((_, bytes)) = &cpu.bus.stub_console {
            eprintln!("EXPERIMENT: stub console ({} bytes):", bytes.len());
            eprintln!("{}", String::from_utf8_lossy(bytes));
        }
        eprintln!(
            "EXPERIMENT: {} stubbed MMIO words (FM1_STUB_MMIO):",
            report.len()
        );
        for (address, value, count) in report {
            eprintln!("  0x{address:08x} = 0x{value:08x} ({count} accesses)");
        }
    }
    eprintln!("application: {}", args[0]);
    cpu.bus.sync().map_err(|fault| fault.to_string())?;
    let ticks = cpu.bus.oscillator_ticks();
    eprintln!(
        "executed: {} instructions (CPU0 {}, CPU1 {}); guest time {} ticks ({:.3} s)",
        cpu.steps,
        cpu.core_steps[0],
        cpu.core_steps[1],
        ticks,
        ticks as f64 / 24e6
    );
    if cpu.idle_slots > 0 {
        eprintln!(
            "idle: {} instruction slots halted in idle, both cores ({} skipped)",
            cpu.idle_slots, cpu.idle_skipped
        );
    }
    eprintln!("stopped: {}", location(&firmware.symbols, cpu.pc));
    if let Some(pc) = cpu.secondary_pc() {
        eprintln!("secondary core: pc {}", location(&firmware.symbols, pc));
    }
    eprintln!(
        "LCD: {} pixels written; visible={}",
        cpu.bus.lcd.pixels_written,
        cpu.bus.screen_visible()
    );
    eprintln!(
        "interrupts: {}; USB: {} host setups, {} packets, {} CDC bytes",
        cpu.irq_entries, cpu.bus.usb.setups, cpu.bus.usb.packets, serial_bytes
    );
    if cpu.exception_entries > 0 {
        let (core, at) = cpu.last_exception();
        eprintln!(
            "CPU exceptions (vector 1): {}; last on core {core} at {}",
            cpu.exception_entries,
            location(&firmware.symbols, at)
        );
    }
    eprintln!(
        "watchdog: {} feeds; timeout {:?} ticks, {} since the last feed",
        cpu.bus.system.watchdog_feeds,
        cpu.bus.system.watchdog_timeout(),
        cpu.bus.watchdog_ticks()
    );
    eprintln!(
        "audio: {} stereo frames, {} DMA halves; ADC: {} conversions",
        cpu.bus.audio.frames, cpu.bus.audio.halves, cpu.bus.devices.adc.conversions
    );
    if watchdog_off {
        eprintln!("EXPERIMENT: watchdog expiry disabled (FM1_WATCHDOG_OFF=1)");
    }
    if let Some(&address) = firmware.symbols.get("felucca_dbg") {
        // Existing guest diagnostics from Felucca's audio.c; no guest hooks.
        if cpu.bus.read(address, 4).ok() == Some(0x44424731) {
            let read = |offset| cpu.bus.read(address + offset, 4).unwrap_or(0);
            eprintln!(
                "Felucca: {} UI frames, {} rendered audio halves, {} timer IRQs; stage={}, page={}, home={}",
                read(28), read(4), read(24), read(44), read(48), read(52)
            );
        }
    }
    if let Some(name) = &op_filter {
        eprintln!("{name}: {} distinct PCs", op_pcs.len());
        for (pc, count) in &op_pcs {
            eprintln!("  {count:>9} {}", location(&firmware.symbols, *pc));
        }
    }
    if !callers.is_empty() {
        let mut ranked: Vec<_> = callers.into_iter().collect();
        ranked.sort_by_key(|&(r, count)| (std::cmp::Reverse(count), r));
        eprintln!("callers (rets) at the FM1_CALLERS PC:");
        for (rets, count) in ranked.into_iter().take(16) {
            eprintln!("  {count:>9} 0x{rets:08x}");
        }
    }
    if !hot.is_empty() {
        let mut ranked: Vec<_> = hot.into_iter().collect();
        ranked.sort_by_key(|&(pc, count)| (std::cmp::Reverse(count), pc));
        eprintln!("hottest primary-core PCs in the final {hot_window} steps:");
        for (pc, count) in ranked.into_iter().take(24) {
            eprintln!("  {count:>9} {}", location(&firmware.symbols, pc));
        }
    }
    if let Some(stats) = cpu.bus.take_mmio_stats() {
        let mmio_top: usize = env::var("FM1_MMIO_TOP")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(40);
        let mut ranked: Vec<_> = stats.into_iter().collect();
        ranked.sort_by_key(|&(key, [r, w])| (std::cmp::Reverse(r + w), key));
        eprintln!("MMIO accesses in the final {mmio_window} calls (reads, writes):");
        for ((address, pc), [r, w]) in ranked.into_iter().take(mmio_top) {
            eprintln!("  0x{address:08x}: {r:>9} {w:>9}  pc 0x{pc:08x}");
        }
    }
    // Optional: FM1_DUMP=ADDRESS:BYTES[,ADDRESS:BYTES] hex-dumps guest memory.
    if let Ok(requests) = env::var("FM1_DUMP") {
        for request in requests.split(',') {
            let (address, length) = request.split_once(':').ok_or("FM1_DUMP needs A:N")?;
            let address = u32::from_str_radix(address.trim_start_matches("0x"), 16)
                .map_err(|_| "invalid FM1_DUMP address")?;
            let length: u32 = length.parse().map_err(|_| "invalid FM1_DUMP length")?;
            for line in (0..length).step_by(16) {
                let bytes: Vec<_> = (line..(line + 16).min(length))
                    .map(|i| match cpu.bus.read(address + i, 1) {
                        Ok(byte) => format!("{byte:02x}"),
                        Err(_) => "??".into(),
                    })
                    .collect();
                eprintln!("  {:08x}: {}", address + line, bytes.join(" "));
            }
        }
    }
    // Optional: FM1_DUMPW=ADDRESS:WORDS[,...] dumps 32-bit words (for MMIO
    // that only accepts word reads).
    if let Ok(requests) = env::var("FM1_DUMPW") {
        for request in requests.split(',') {
            let (address, count) = request.split_once(':').ok_or("FM1_DUMPW needs A:N")?;
            let address = u32::from_str_radix(address.trim_start_matches("0x"), 16)
                .map_err(|_| "invalid FM1_DUMPW address")?;
            let count: u32 = count.parse().map_err(|_| "invalid FM1_DUMPW count")?;
            let words: Vec<_> = (0..count)
                .map(|i| match cpu.bus.read(address + 4 * i, 4) {
                    Ok(word) => format!("{word:08x}"),
                    Err(_) => "????????".into(),
                })
                .collect();
            eprintln!("  {address:08x}: {}", words.join(" "));
        }
    }
    // Optional: FM1_RAM=PATH saves all guest SRAM for offline inspection.
    if let Ok(path) = env::var("FM1_RAM") {
        let ram: Vec<u8> = (0..fm1_emu::RAM_SIZE as u32)
            .map(|i| cpu.bus.read(fm1_emu::RAM + i, 1).unwrap_or(0) as u8)
            .collect();
        std::fs::write(&path, ram).map_err(|e| format!("{path}: {e}"))?;
        eprintln!("RAM: saved {path}");
    }
    // Optional: FM1_PNG_RAM=PATH saves the whole 240x320 ST7789V memory.
    if let Ok(path) = env::var("FM1_PNG_RAM") {
        let png = fm1_emu::png::encode_rgb(
            fm1_emu::lcd::WIDTH,
            fm1_emu::lcd::GRAM_HEIGHT,
            cpu.bus.lcd.frame_memory(),
        )?;
        std::fs::write(&path, png).map_err(|e| format!("{path}: {e}"))?;
        eprintln!("frame memory: saved {path}");
    }
    // Optional: FM1_PNG=PATH saves the raw panel framebuffer (ungated).
    if let Ok(path) = env::var("FM1_PNG") {
        let png = fm1_emu::png::encode_rgb(
            fm1_emu::lcd::WIDTH,
            fm1_emu::lcd::HEIGHT,
            &cpu.bus.lcd.pixels,
        )?;
        std::fs::write(&path, png).map_err(|e| format!("{path}: {e}"))?;
        eprintln!("framebuffer: saved {path}");
    }
    eprintln!("recent completed instructions:");
    for (pc, op, word, registers) in recent {
        if recent_count > 12 {
            let list: Vec<_> = registers.iter().map(|r| format!("{r:x}")).collect();
            eprintln!(
                "  {}: {word:04x} {op} [{}]",
                location(&firmware.symbols, pc),
                list.join(" ")
            );
        } else {
            eprintln!("  {}: {op}", location(&firmware.symbols, pc));
        }
    }
    eprintln!("registers:");
    for (i, register) in cpu.r.iter().enumerate() {
        eprintln!("  r{i}=0x{register:08x}");
    }
    eprintln!(
        "  sp=0x{:08x}; rets=0x{:08x}; interrupts_enabled={}",
        cpu.sr[14], cpu.sr[3], cpu.interrupts_enabled
    );
    match fault {
        Some(error) => Err(error.to_string()),
        None => Err(format!("instruction limit {limit} reached")),
    }
}

/// FM1_BLE_CENTRAL=script|scan puts the virtual central on the air:
/// `script` runs FM1_BLE_SCRIPT=PATH (default: the built-in script),
/// `scan` only scans. FM1_BLE_MODEL=key=value,... sets engine hypotheses
/// (ble_engine::EngineConfig::apply). FM1_BLE_ADVERTISERS=default|SPEC puts
/// virtual advertisers on the air for a scanning FM-1 to hear
/// (ble_engine::Advertiser::parse). FM1_BLE_PERIPHERALS=SPEC puts
/// connectable virtual BLE-MIDI peripherals on the air for the FM-1 as
/// central (ble_peripheral::PeripheralConfig::parse); FM1_BLE_PERIPHERAL_BOOT=N
/// sets their boot number (a new RPA per boot, the same keys). Returns
/// whether BLE reporting is on.
fn setup_ble(cpu: &mut Cpu) -> Result<bool, String> {
    use fm1_emu::ble_central::{script, Central, CentralConfig, Step};
    if let Ok(settings) = env::var("FM1_BLE_MODEL") {
        cpu.bus.ble_engine_mut().config.apply(&settings)?;
    }
    let advertisers = env::var("FM1_BLE_ADVERTISERS").ok();
    if let Some(spec) = &advertisers {
        cpu.bus.ble_engine_mut().advertisers = fm1_emu::ble_engine::Advertiser::parse(spec)?;
    }
    let peripherals = env::var("FM1_BLE_PERIPHERALS").ok();
    if let Some(spec) = &peripherals {
        use fm1_emu::ble_peripheral::{Peripheral, PeripheralConfig};
        let mut configs = PeripheralConfig::parse(spec)?;
        if let Ok(boot) = env::var("FM1_BLE_PERIPHERAL_BOOT") {
            let boot = boot
                .parse()
                .map_err(|_| "invalid FM1_BLE_PERIPHERAL_BOOT")?;
            for c in &mut configs {
                c.boot = boot;
            }
        }
        cpu.bus.ble_engine_mut().peripherals = configs.into_iter().map(Peripheral::new).collect();
    }
    let steps = match env::var("FM1_BLE_CENTRAL").as_deref() {
        Err(_) | Ok("off") => {
            return Ok(env::var("FM1_BLE_MODEL").is_ok()
                || advertisers.is_some()
                || peripherals.is_some())
        }
        Ok("scan") => vec![Step::Scan(u32::MAX)],
        Ok("script") => match env::var("FM1_BLE_SCRIPT") {
            Ok(path) => {
                let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
                script::parse(&text)?
            }
            Err(_) => script::default_script(),
        },
        Ok(other) => return Err(format!("FM1_BLE_CENTRAL={other}: use script, scan or off")),
    };
    let mut config = CentralConfig::default();
    if let Ok(delay) = env::var("FM1_BLE_WINDOW_DELAY_US") {
        config.window_delay_us = delay
            .parse()
            .map_err(|_| "invalid FM1_BLE_WINDOW_DELAY_US")?;
    }
    // FM1_BLE_LOSS=N: the central misses every Nth peripheral PDU;
    // FM1_BLE_PAIRS=N: up to N central/peripheral pairs per event (MD).
    for (name, field) in [
        ("FM1_BLE_LOSS", &mut config.lose_every),
        ("FM1_BLE_PAIRS", &mut config.max_pairs_per_event),
    ] {
        if let Ok(value) = env::var(name) {
            *field = value.parse().map_err(|_| format!("invalid {name}"))?;
        }
    }
    // FM1_BLE_SMP=ignore leaves a Security Request unanswered.
    config.answer_smp = env::var("FM1_BLE_SMP").map_or(true, |v| v != "ignore");
    cpu.bus.ble_attach_central(Central::new(config, steps));
    Ok(true)
}

/// The phone's user of a virtual peripheral that waits for a passkey
/// (`auth`): FM1_BLE_PASSKEY_AT=ADDR[:STEPS] reads the passkey the FM-1
/// shows from the guest's RAM (a u32 at ADDR, hex: the firmware's
/// `smp_passkey`, 0xFFFFFFFF while none is shown) once a peripheral has
/// waited STEPS instructions (default 150,000,000: ~1.5 s at 96 MHz, the
/// time to read and type it), and types it.
struct PasskeyTypist {
    addr: u32,
    delay: u64,
    next_check: u64,
    since: Vec<Option<u64>>,
}

fn passkey_typist() -> Result<Option<PasskeyTypist>, String> {
    let Ok(spec) = env::var("FM1_BLE_PASSKEY_AT") else {
        return Ok(None);
    };
    let (addr, delay) = spec.split_once(':').unwrap_or((spec.as_str(), "150000000"));
    let addr = u32::from_str_radix(addr.trim_start_matches("0x"), 16)
        .map_err(|_| "FM1_BLE_PASSKEY_AT=ADDR[:STEPS]: ADDR in hex")?;
    let delay = delay
        .parse()
        .map_err(|_| "FM1_BLE_PASSKEY_AT=ADDR[:STEPS]: STEPS a number")?;
    Ok(Some(PasskeyTypist {
        addr,
        delay,
        next_check: 0,
        since: Vec::new(),
    }))
}

impl PasskeyTypist {
    fn poll(&mut self, cpu: &mut Cpu, step: u64) {
        if step < self.next_check {
            return;
        }
        self.next_check = step + 100_000;
        let shown = cpu.bus.read(self.addr, 4).ok();
        let peripherals = &mut cpu.bus.ble_engine_mut().peripherals;
        self.since.resize(peripherals.len(), None);
        for (p, since) in peripherals.iter_mut().zip(self.since.iter_mut()) {
            if !p.wants_passkey() {
                *since = None;
                continue;
            }
            let t0 = *since.get_or_insert(step);
            match shown {
                Some(v) if v < 1_000_000 && step - t0 >= self.delay => {
                    eprintln!("ble_passkey typed={v:06} step={step}");
                    p.type_passkey(v);
                    *since = None;
                }
                _ => {}
            }
        }
    }
}

fn report_ble(cpu: &Cpu) -> Result<(), String> {
    let engine = cpu.bus.ble_engine();
    eprintln!("BLE engine: {:?}", engine.stats);
    for a in &engine.advertisers {
        eprintln!(
            "  advertiser {:<14} {:02x?} sent {} scan_req {} scan_rsp {}",
            a.name, a.address, a.sent, a.scan_reqs, a.scan_rsps
        );
    }
    let s = &engine.stats;
    eprintln!(
        "ble_engine_central init_windows={} connect_inds_sent={} master_events={} master_tx_data={} \
         master_tx_empty={} master_tx_retransmit={} master_rx={} cind_lost={} busy_stops={}",
        s.init_windows,
        s.connect_inds_sent,
        s.master_events,
        s.master_tx_data,
        s.master_tx_empty,
        s.master_tx_retransmit,
        s.master_rx,
        s.cind_lost,
        s.busy_stops
    );
    for p in &engine.peripherals {
        eprintln!("{}", p.report_line());
    }
    for note in engine.notes() {
        eprintln!("  model: {note}");
    }
    // FM1_BLE_LOG=PATH writes every packet on the virtual air.
    let log: Vec<String> = engine.air_log().iter().map(|r| r.to_string()).collect();
    match env::var("FM1_BLE_LOG") {
        Ok(path) => {
            std::fs::write(
                &path,
                log.join(
                    "
",
                ) + "
",
            )
            .map_err(|e| format!("{path}: {e}"))?;
            eprintln!(
                "BLE air log: {} packets saved to {path}",
                engine.air_count()
            );
        }
        Err(_) => {
            eprintln!(
                "BLE air log: {} packets (first 40; FM1_BLE_LOG=PATH saves all):",
                engine.air_count()
            );
            for line in log.iter().take(40) {
                eprintln!("  {line}");
            }
        }
    }
    let loud = cpu
        .bus
        .audio
        .samples
        .iter()
        .filter(|f| **f != [0, 0])
        .count();
    eprintln!(
        "BLE run: {loud} of {} recorded audio frames are not silent",
        cpu.bus.audio.samples.len()
    );
    let Some(central) = cpu.bus.ble_central() else {
        return Ok(());
    };
    if central.lost > 0 {
        eprintln!(
            "  central: {} peripheral PDUs lost on purpose (FM1_BLE_LOSS)",
            central.lost
        );
    }
    for line in central.log() {
        eprintln!("  central: {line}");
    }
    for r in central.results() {
        eprintln!("  step {:?}: {:?}", r.step, r.outcome);
    }
    for n in central.notifications() {
        eprintln!(
            "  notification {:.6} handle 0x{:04x}: {:02x?}",
            n.tick as f64 / 24e6,
            n.handle,
            n.value
        );
    }
    if let Some(r) = central.adv_reports().first() {
        eprintln!(
            "  first ADV: type {} addr {:02x?} data {:02x?}",
            r.kind, r.address, r.data
        );
    }
    if let Some(r) = central.scan_responses().first() {
        eprintln!(
            "  first SCAN_RSP: addr {:02x?} data {:02x?}",
            r.address, r.data
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("diagnose: {error}");
            ExitCode::FAILURE
        }
    }
}
