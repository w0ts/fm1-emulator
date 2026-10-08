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
                let (range, mask) = range.split_once(':').unwrap_or((range, "0"));
                let (low, high) = range.split_once('-')?;
                Some((
                    u32::from_str_radix(low.trim_start_matches("0x"), 16).ok()?,
                    u32::from_str_radix(high.trim_start_matches("0x"), 16).ok()?,
                    u32::from_str_radix(mask.trim_start_matches("0x"), 16).ok()?,
                ))
            })
            .collect::<Vec<_>>()
    });
    if let Some(ranges) = &stub_ranges {
        cpu.bus.set_mmio_stubs(ranges.clone());
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
        match cpu.step_next() {
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

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("diagnose: {error}");
            ExitCode::FAILURE
        }
    }
}
