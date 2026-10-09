# Rust FM-1 emulator

Rust 1.91.1 is managed by the root `mise.toml`. The interpreter executes vendor-built pi32v2 machine code without external
Rust dependencies. The optional `gui` feature uses eframe/egui 0.31.1 for a
native OpenGL window; the version and transitive dependencies are locked.

The [full Felucca boot investigation](FELUCCA.md) records the verified firmware
inputs, resolved startup failures, decoder fixes, full boot and note checks. It also
documents the bounded `diagnose` runner for symbol and peripheral reports.

On a fresh checkout, fetch the locked dependency metadata before offline tests:

```sh
mise exec -- cargo fetch --manifest-path rust-emulator/Cargo.toml --locked
mise run rust-test
mise run rust-probe
mise run build-foundation
mise run rust-foundation
```

## Native device window

### The desktop app

Start `fm1-ui` with no firmware (double-click the app) and the window opens
with the panel idle and a **Load firmware…** button; the same button is always
in the toolbar. It opens the system's own file dialog, filtered to `.fwsc`,
`.elf` and `.bin` (any file can still be picked with "All files"). You can also
drop a firmware file on the window, or pick one from the **Recent** menu.

Loading a firmware reboots into it, like a power cycle: the running firmware's
flash state is saved first, the CPU and devices are rebuilt from the new
package, and the new firmware's own flash state (its family, see below) is
restored. Nothing is lost, so there is no confirmation. The window title and the
toolbar show which firmware runs. The web editor follows the firmware too.

The last firmware is remembered (`recent` in `fm1-ui.toml`, up to eight) and
opened again at the next start unless a firmware is named on the command line
(that wins) or Recent's "Reopen the last firmware at start" is off.

**macOS app.** `scripts/bundle-macos.sh fm1-ui OUTPUT_DIR` makes `FM-1
Emulator.app` (bundle id `org.fm1-emulator.ui`, the version from `Cargo.toml`,
Retina, macOS 11 or later, an ad-hoc signature). CI attaches it, zipped, to
every commit release. The icon is a generated placeholder
(`macos/icon-placeholder.png`, made by `macos/make-placeholder-icon.py`); put a
real 1024x1024 `macos/icon.png` there and the script uses it. The app is not
signed with an Apple developer identity or notarized (TODO), so the first launch
needs one of: right-click the app and choose Open, or `xattr -dr
com.apple.quarantine "FM-1 Emulator.app"`.

**Where the app keeps its files.** Run as an installed app, it keeps
`fm1-ui.toml` and the flash state in the operating system's per-user
application-data folder (outside any repository), because the folder the
program sits in can be read-only:

| OS | Folder |
|---|---|
| macOS | `~/Library/Application Support/FM-1 Emulator/` |
| Windows | `%APPDATA%\FM-1 Emulator\` |
| Linux | `$XDG_DATA_HOME/fm1-emulator/` (default `~/.local/share/fm1-emulator/`) |

`fm1-ui.toml` is there and the flash state is in its `state/` subfolder. This
"app mode" applies to a macOS `.app` bundle, to any launch with no arguments at
all (a double-click or a desktop entry on Windows and Linux), and to
`--app-data`. Everything else keeps the old places: `state/` beside the
emulator (the crate directory for a build under `target/`) or `--state PATH`,
which always wins, so `./emulator`, `cargo run` and the sloop `tools/emu.py`
behave as before.

**Windows and Linux.** The CI artifacts are the plain executable (`emulator.exe`,
`emulator`) and open with the same picker when started without arguments. On
Windows the program is built for the GUI subsystem, so a double-click opens no
console window; started from a terminal it joins that terminal's console, so
`--help`, messages and the USB serial stdin work as for a console program. On
Linux the dialog is the XDG desktop portal's (GNOME, KDE and others provide it;
no GTK is needed to build).

From the project root:

```sh
./emulator                   # no firmware: opens with the Load firmware button
./emulator build/display/firmware.elf
./emulator "$HOME/Downloads/FM-1.fwsc"
mise run build-display       # optional: rebuild guest with the vendor compiler
mise run rust-gui-test       # core and GUI input integration tests
```

The launcher compiles a release `fm1-ui` executable using mise. On macOS it
creates a local application bundle under `target/release`; on Linux it runs the
binary directly (the host needs the normal eframe OpenGL/windowing development
libraries). macOS is visually verified; Linux is not tested here. The existing
`fm1-emu probe|boot` CLI is unchanged and does not require a display server.

The panel is an original vector illustration drawn in Rust, using the device's
[front-panel photograph](https://m.media-amazon.com/images/I/71fgwUHLJKL._AC_SL1500_.jpg)
as a layout reference. No vendor product photo is bundled. Button identities
follow the [pinned Felucca panel defaults](https://github.com/hugelton/Felucca/blob/1e838e17e170b20ff09b9660c9a7171aadfc5dca/firmware/src/panel.c)
and the local `fm1_input.h` wiring. All fourteen buttons and twenty-seven note
keys feed matrix contacts. The seven encoders turn by dragging around them,
scrolling, or keys (`[ ]`, `9 0`, `- =`, `1`-`8`); each click plays one
quadrature cycle into the encoder's A/B matrix contacts (Felucca's
`FM1_ENC` wiring), and each phase is held until the guest has read both
contact columns three times, so the pace follows the firmware's own scan rate.
MASTER is the ADC potentiometer (drag, scroll, or `N M`). Short
clicks/keystrokes are held for at least 100 ms of both host and guest time so a
slow guest scan can observe and debounce them.

Every button also has a computer key, held for as long as the key is down, so
layer chords (SAVE + note, FX + knob, the "press again" confirmations) need no
mouse; each button's tooltip names its key. The first thirteen notes are
`A W S E D R F G T H Y J K` (a piano row); hold **Shift** with the same keys
for the next thirteen, one octave up (`Shift+A` is Note 66; the tooltip names
the combo). The octave is chosen when the key goes down and kept until it is
released, so letting go of Shift first never leaves a key stuck. The topmost
(27th) key has no computer key. The buttons use letters those and the knob
keys leave free:

| Button | Key | Button | Key | Button | Key |
|---|---|---|---|---|---|
| OCT− | `←` | FX | `X` | HOME | `U` |
| OCT+ | `→` | SEL | `B` | SAVE | `Z` |
| PLAY / STOP | `Space` | ENV | `V` | ARP | `P` |
| REC | `C` | LFO | `L` | SEQ | `Q` |
| | | EDIT | `I` | GLO | `O` |

Shift only selects the upper note octave; Ctrl/Alt/Cmd are not used (egui
reports modifiers as state rather than as keys). A button letter with Shift
held still presses its button and plays no note.
Losing window focus releases contacts. Pause stops guest execution; Restart
reloads the selected image and resets CPU, RAM, peripherals, and input state.

### Web editor (USB-MIDI over a local WebSocket)

When the firmware has a web editor, the window serves it on
`http://127.0.0.1:8765/` (loopback only) and connects it to the emulated
device: `FIRMWARE-ui.zip` next to `FIRMWARE.fwsc`, or `--ui DIR` (a folder with
`index.html` or `editor.html`, such as a Felucca-family `web/`). "Open editor"
opens the default browser; the status line shows the URL, connected pages, MIDI
message counts and the port name. Without an editor the button is disabled.

```sh
./emulator --ui ~/src/Felucca/web felucca-0.9-beta.fwsc
rust-emulator/scripts/make-ui-sidecar.sh ~/src/Felucca v0.9-beta felucca-0.9-beta.fwsc
```

- The USB host model then also acts as a USB-MIDI host, and the GUI worker
  moves MIDI between the server and the device between instruction batches.
- Every served HTML page gets a Web MIDI shim (`src/web/shim.js`):
  `navigator.requestMIDIAccess` returns one input and one output named
  `"<USB product> (FM-1 Emulator)"` ("Felucca (FM-1 Emulator)" for Felucca,
  Jangada and SLOOP, whose editors look for /felucca/i), so editors run
  unchanged in any browser.
- `/midi` is a WebSocket of binary frames of raw MIDI bytes: from the browser
  any split (running status allowed), to the browser one complete message per
  frame.
- Requests with a Host other than 127.0.0.1, localhost or [::1] on that port,
  and WebSocket Origins other than those, are refused (DNS rebinding).

The bridge is the `web` feature (tungstenite for the WebSocket handshake and
framing, zip for the sidecar); `gui` includes it, and the core library keeps
no dependencies.

The UI reads only the panel's 240×240 framebuffer, gated by display enable,
sleep, and active-low PA2 backlight. There are no symbol-specific drawing hooks
or substituted application functions. SPI1 commands and SRAM DMA implement
software reset, sleep/display enable, RGB565 format, unrotated RGB/BGR, column/
row windows, and pixel writes. Unsupported commands, rotations, and invalid DMA
addresses fault visibly. Completion is synchronous, not cycle-accurate; INVON
is treated as the FM-1 panel's normal electrical drive mode, not an RGB invert.
The CPU runs continuously on a worker thread. The window sends matrix contacts
and receives the latest LCD snapshot; unchanged pixels do not require texture
uploads. Guest time follows the emulated clock, so execution speed depends on
the host. When the guest streams audio and a host output device opens (cpal,
part of the `gui` feature), the guest's ALNK0 frames play through it and the
worker paces execution by the playback queue, about 70 ms ahead: real time
when the host keeps up. The instruction clock defaults to the firmware's
system clock; `--cpu-mhz N` or the toolbar selector issues one instruction per
N MHz of guest time instead (timers, DMA, USB and the watchdog keep their own
clocks), so light firmware can play in real time. The interpreter is not yet fast
enough for every firmware at its own clock: see [PERFORMANCE.md](PERFORMANCE.md)
for measured guest-seconds per host-second, and the audio breaks up where that
is below 1.

The drawn panel comes in the emulator's original Classic colours (the
default) and in colours sampled from the FM-1's editions: Black, Lilac,
Orange, Mint, Cream and Blue. Pick one in the toolbar, or at start with
`--theme NAME` or `FM1_THEME=NAME` (the option wins, for that run only).

### Command line and environment

```sh
fm1-ui [--cpu-mhz N | --cpu-mhz=N] [--ui DIR] [--theme NAME] [--state PATH] [--fresh] [--app-data] [FIRMWARE]
```

Without FIRMWARE the last one opened is used if it still exists, else the
window waits for **Load firmware…**.

`./emulator` (the launcher) forwards only `--cpu-mhz` and `--ui`; run
`rust-emulator/target/release/fm1-ui` directly for the others.

| Option | Meaning |
|---|---|
| `--cpu-mhz N` | One instruction per N MHz of guest time (1..1000); default is the firmware's own system clock. The toolbar also offers 192 and 312 MHz and "Firmware clock". Not remembered between runs. |
| `--ui DIR` | A folder with the firmware's web editor (`index.html` or `editor.html`); see Web editor above |
| `--theme NAME` | Panel colours: Classic, Black, Lilac, Orange, Mint, Cream, Blue (this run only; also `FM1_THEME`) |
| `--state PATH` | Flash state file, or a folder when PATH is one or ends in `/` |
| `--fresh` | Start from the package alone and replace the state |
| `--app-data` | Keep `fm1-ui.toml` and the state in the per-user application-data folder, as the installed app does (ignored with `--state`) |

Environment variables (those the code reads; the headless tools take their own,
listed with them below):

| Variable | Read by | Effect |
|---|---|---|
| `FM1_THEME` | fm1-ui | Panel theme; `--theme` wins |
| `FM1_NESTED_IRQ=1` | fm1-ui, tools | Let interrupts nest by priority (off by default; USB audio builds) |
| `FM1_CPU_MHZ=N` | tools | Instruction clock (a multiple of 24); default is the firmware's |
| `FM1_IDLE_SKIP=0` | tools | Step halted idle slots one by one instead of skipping to the next device event |
| `FM1_SPIN_SKIP=0` | tools | Step every instruction of a core busy-waiting on a device |
| `FM1_BLOCK_CACHE=1` | bench | Execute through the experimental block cache and JIT (off by default) |
| `FM1_QOS=default` | bench | Do not raise the thread to the GUI worker's scheduling class |
| `FM1_SCENARIO=FILE` | bench | Play panel steps from FILE first (`examples/scenarios/busy.steps`) |
| `FM1_HOT=N`, `FM1_HOT_DUMP=FILE`, `FM1_ELF` | play_check | Instruction profile, see Scripted playback |
| `PLAY_TRACE=N` | play_check | Instructions to print after a guest fault (default 40) |

`diagnose` has further debugging variables (`FM1_WATCHDOG_OFF`, `FM1_MEMWATCH`,
`FM1_MMIO`, `FM1_PNG`, ...); read its source header for them.

### Window settings kept between runs

`fm1-ui` remembers the MASTER volume, the panel theme picked in the toolbar,
the LEDs switch, the window size and the recently opened firmware files
(`recent`, `reopen_last`) in `fm1-ui.toml`, a few `key = value` lines beside
the flash state (`state/`, or the `--state` folder; in the application-data
folder for the installed app, see above). Strings are quoted, with `\\` and `\"`
as the only escapes. It is
written a second after a change settles and when the window closes; a file
that cannot be read, or a line with a bad value, falls back to the defaults
with a message on stderr. Delete the file to start from the defaults. The
instruction clock is not remembered (it changes what the guest computes), and
headless tools never read or write the file.

### Flash kept between runs

Like the device across a power cycle, `fm1-ui` keeps what the firmware wrote
to its serial flash: projects, autosave, presets, kits and settings saved in
one session are there at the next start. Every 4 KiB sector the firmware
erases or programs is saved to a state file for its firmware family, about
one second after the last flash write (so a crash or kill loses at most that
second), when the window closes, on Ctrl+C or SIGTERM, and before the Restart
button (a power cycle).

- The family is the package's file name up to its version:
  `optimist-0.1-dev-5379036.fwsc` and `optimist-0.2.fwsc` share `optimist`,
  `felucca-1.0.3.fwsc` is `felucca`, `sloop-2.3.fwsc` is `sloop`. A newer
  build of a firmware so starts with the older one's data.
- The state is `state/FAMILY.nor` (a 1 MiB flash image; sectors the firmware
  never wrote read erased) and `state/FAMILY.index` (the sectors that count),
  in the emulator's directory (`rust-emulator/` for a binary under
  `target/`). `--state PATH` uses another file, or a folder when PATH is one
  or ends in `/`.
- At start the saved sectors are laid over the freshly loaded package, through
  the same path a guest erase and program take (raw flash, encrypted XIP view).
  A saved sector that overlaps the package's boot and application area is
  skipped and a line says so: new code, old data, like an update.
- `--fresh` starts from the package alone and replaces the state; the toolbar's
  Flash menu has **Reset flash state…** (with a confirmation) for the same
  thing while running. Deleting the two files works too.

Headless tools (`play_check`, `bench`, `preset_sweep`, ...) neither read nor
write a state, so their measurements repeat; `play_check --state PATH
[--fresh] FIRMWARE STEP...` opts in, and saves at the end of the run.
`scripts/flash-state-e2e.sh FIRMWARE.fwsc` checks a firmware end to end: it
changes a knob, waits past the autosave, restarts from the state and compares
the screens.

Instruction dispatch uses a shared first-word decode table and a bounded cache
of wide instruction words. A bounded basic-block cache (off by default, `FM1_BLOCK_CACHE=1` in `bench`) can also prepare common
register, arithmetic, shift, memory and short branch operations, including their
operands. Each core retains its own position in the block. Current instruction
words are still checked through the bus, so SRAM changes, flash remapping and
disabled XIP cannot execute stale code. Interrupts, device timing and core
interleaving retain their per-instruction boundaries.

Hot blocks compile common register operations to ARM64 or x86-64 machine code.
Each native entry currently executes one guest instruction before returning to
the device scheduler; memory accesses and complex instructions use the existing
handlers. Cheap uncached operations bypass block lookup. Native code uses owned
pages that become read/execute after emission, and automatically falls back to
prepared interpretation if allocation is denied. Broader translation and native
block batching remain performance work; this initial backend does not yet provide
a substantial whole-firmware speedup.

The [performance TODOs](PERFORMANCE.md) record the current measurements and the
remaining batching, validation and profiling work.

`build/display/firmware.elf` is the 17,056-byte FM-1_981 hardware application.
It uses Felucca-derived startup, watchdog, recovery, input scanning, USB CDC and
MIDI updater code. `firmware/display.S` draws labels, a buffered TIMER4 readout
and key tiles, called by the hardware application's main loop. There is no
emulator-specific application path in this image. Package integrity tests
compare its bytes against the decrypted `build/display/firmware.fwsc` payload.
The same package was flashed successfully to the FM-1; its USB serial status
reports advancing display frames, and all twelve CPU probe words match.

Button transitions pass through guest GPIO scanning and debounce, the CDC
ring, USB endpoint DMA and the emulator host before stdout receives
`KEY <id> down` or `KEY <id> up`. The host performs GET_DESCRIPTOR,
SET_ADDRESS, SET_CONFIGURATION and CDC SET_CONTROL_LINE_STATE requests.
It discovers the CDC interface and IN endpoint from the configuration descriptor.
Terminal stdin reaches the descriptor-selected CDC OUT endpoint through bounded
host queues, SRAM DMA, RX packet/count registers and the receive interrupt latch.
An unread packet stays intact until the guest acknowledges it. Type commands and
press Enter; EOF stops the input reader while guest execution continues.
The unchanged local and published Felucca builds answer `help` through this path.
`Usb::enable_midi_host` (off by default, so the CDC enumeration is unchanged)
also makes the host a USB-MIDI host: it finds the MIDI streaming interface and
its bulk endpoints, skips the CDC line state when a device has no console,
reads the product string, delivers queued `Bus::usb_midi_send` packets (up to
16 events per bulk packet) into the OUT endpoint's RX buffer with the same
NAK rules as the console, and collects the device's MIDI IN packets in
`usb.midi_received`. Felucca 0.9-beta, Jangada 0.1-alpha and SLOOP 2.2 play
a note from a host note-on and answer their editors' INFO SysEx this way.
The hardware firmware retains both its serial console and MIDI updater.

P33 accesses model watchdog arming/feeding and stop with an expiry fault if
feeding ceases. NOR supports the JEDEC/status/read transactions used at startup, write enable,
sector/block erase and page program; the sectors a firmware writes are kept
between runs (see above). CPU write guards
reject protected RAM writes; full guard exception dispatch and stack/PC limit
hardware remain incomplete. Reset requests stop rather than emulate ROM boot.
Full Felucca boots into its UI and renders note samples through ALNK DMA.
FX rendering and continued execution now pass with both the published package
and local source ELF. Support remains partial: the local presets path can stop
on an unsupported instruction. See the [Felucca investigation](FELUCCA.md) for
the verified scope and remaining failure.
Additional engines, CPU forms and peripheral behavior remain incomplete. See
[the measured full-firmware checks](FELUCCA.md).

The loader accepts an FM-1 `.fwsc` package, an application `.bin` mapped at
`0x02000120`, or an executable ELF32-pi32v2. Package loading checks the outer
header/table, complete `flash.bin`, its flash directory, chip key and decrypted
application CRCs. It retains the original flash bytes and supplies the SDK's
SPL boot-device parameter block; encrypted SFC reads include the package's
directory and embedded configuration. Separate outer auxiliary payloads and
the update process are not emulated. ELF flash load addresses reconstruct the exact application `.bin`,
including the initializers for RAM code and data. Startup performs the RAM
copies; the loader does not move them early. Package support does not imply
that every stock CPU or peripheral path is implemented; see the
[stock firmware boot trials](STOCK-FIRMWARE.md).

`rust-probe` calls the embedded probe directly from the unchanged full
`build/fm1-diag.elf`. Its twelve results match the saved physical FM-1 capture.
The existing Python interpreter remains an independent reference. The Rust
probe takes 70 instructions; the Python test image includes five additional
startup/call instructions, for 75.

For a raw application, specify the probe entry from that build's symbol table:

```sh
mise exec -- cargo run --manifest-path rust-emulator/Cargo.toml --offline -- \
  probe build/fm1-diag.bin --entry 0x02002bc2
```

The address above belongs to the current FM-1_980 build, not arbitrary firmware.
Both commands accept `--limit COUNT` and `--trace PATH` (JSONL).

The separate 592-byte foundation firmware boots from `_start`, copies `.data` and
`.ram_text`, clears `.bss`, runs the hardware-verified CPU probe, executes RAM
code, observes TIMER4 progressing, and services a TIMER5 interrupt through a
guest vector and handler. The handler runs on SSP, saves/restores registers,
acknowledges the timer, and returns with `rti` to the application stack. Firmware
also clocks the two 74HC595 registers on PA4/PA3, latches on PA1, and scans all
eleven active-low matrix columns. GPIO direction, digital enable, pull-ups,
outputs, and the physical PA/PB row wiring determine the observed key states.

```sh
mise run build-foundation
mise exec -- cargo run --manifest-path rust-emulator/Cargo.toml --offline -- \
  boot build/foundation/firmware.elf --until foundation_done \
  --inspect foundation_results:10 --press 0:4
```

`--press COLUMN:ROW` injects a physical matrix closure; repeat it for multiple
keys. Columns are 0..10 and packed rows 0..5. `0:4` is OCT-minus; `3:4` is the F3
note key. Debouncing and encoder decoding belong to firmware, not the GPIO
model. The hardware display application runs the inherited full input routine.
Scheduled input events remain future work; `fm1_emu::encoders` plays
encoder clicks against the guest's own scans.

`--until` accepts a breakpoint symbol or numeric address, and `--inspect` accepts
`SYMBOL_OR_ADDRESS:WORDS`. Raw `.bin` boot works with numeric addresses. Successful
boot output reports the reached PC, executed instructions, IRQ entries, and
inspected guest RAM. `--trace PATH` saves executed instructions as JSONL.

This ELF/raw application is for emulator tests. It has no updater or recovery
and must not be installed on the FM-1. It is a historical unit-test fixture;
use the shared hardware display build for device comparisons.

Timers use a deterministic virtual clock: the instruction clock (the firmware's
system clock by default, see [CLOCK.md](CLOCK.md)) issues one instruction per
cycle, not measured CPU cycle timing. The foundation firmware's TIMER4 supports
OSC /1 and TIMER5 supports OSC /4; other clock modes fail explicitly. Interrupt
delivery covers TIMER5/IRQ63 and the other sources the firmwares use, with global and
per-source masking; nesting by priority is optional (`FM1_NESTED_IRQ=1`) and off by default. SPL initial state and the interrupt stack handoff are functional
approximations that still need independent physical validation.

The hardware display image runs from its application entry without host
substitutions for firmware functions. Unknown MMIO and instructions still
fault. This models the application after the SPL handoff, not ROM or SPL.
Parallel instruction pairs execute the following slot first. Conditional-block
state is preserved across IRQ63 entry/return. Compiler-derived encodings have
regression tests and boot coverage; only the probe's forms have individual
physical comparison evidence.

Probe encodings come from the Python reference, vendor disassembly, and the
physical comparison. Startup stack arithmetic, immediate masks, and special
register mappings were checked against the Apache-2.0
[Quarkslab pi32v2 reference](https://github.com/quarkslab/ghidra-jieli/tree/e1bd0707874b77b759401555d24839ad43af1267/data/languages).
New CPU/peripheral behavior needs separate hardware validation.

## Instruction coverage scan

`scripts/op-scan.sh` checks a firmware's code against the decoder without
running it to each instruction. It disassembles the image with the vendor
objdump (the JieLi toolchain in an amd64 Docker container, `-mattr=+fprev1`
for the FPU), then `examples/op_scan` describes every listed instruction with
`fm1_emu::describe` and executes it once on a scratch CPU:

```sh
rust-emulator/scripts/op-scan.sh --out /tmp/scan "$HOME/Downloads/FM-1.fwsc"
rust-emulator/scripts/pi32-objdump.sh e868 12fc   # [r1+-4] += r2
```

The report groups by encoding the instructions the interpreter does not
decode, rejects when executing, or gives another length or conditional skip
length than objdump, and the reverse: words objdump cannot decode but the
interpreter executes. For an ELF, code is what lies inside `STT_FUNC` symbols.
Packages and raw images have no symbols, so the scan follows recursive descent
from the entry, branch and call targets, jump tables and code pointers, and
boots the image briefly to find the code startup copies to RAM. Against the
ELFs of four SLOOP builds, every reachable instruction was an ELF instruction
and reachability covered 99.5% of them. `examples/extract` writes the decoded
application image of a package for offline disassembly.

## Agreed foundation checklist

The completion criterion is five of these ten foundations exercised by booted
guest firmware. Each foundation has equal weight for this milestone. This is
not a percentage of complete instruction-set or musical-feature coverage.

| Foundation | Milestone evidence | Remaining scope |
| --- | --- | --- |
| CPU | Guest executes real vendor machine code; twelve probe words match hardware | Further ISA forms, flags and independent instruction probes |
| Memory/startup | ELF equals raw flash image; guest copies data and RAM code, clears dirty BSS, executes RAM code | ROM/SPL, reset retention, boot parameters |
| Timers | Guest sees TIMER4 progress; TIMER5 produces a periodic event | Other sources/dividers and measured cycle timing |
| Interrupts | IRQ63/ALNK11 vectors, masking, SSP handler frame, acknowledgment, `rti`, priority selection | Nested priorities, other IRQs, physical entry-state validation |
| Controls | Guest scans eleven columns; released/pressed and multiple-key cases agree; UI encoders and MASTER reach the guest | Scheduled events |
| Flash | Startup JEDEC/status/NOR reads; plain XIP shares physical NOR storage | XIP busy behavior (erase, program and persistence between runs are done) |
| LCD | Display guest initializes SPI/DMA, draws RGB565 pixels and live timer/key data | Other controller modes, SPI timing, pixel-exact physical comparison |
| USB serial | Hardware guest enumerates and sends CDC debug bytes through DMA | Broader controller/USB behavior (console input from the terminal reaches the guest; the high-speed controller is not modelled) |
| USB MIDI | Host note-on renders audio; an editor SysEx request gets its reply (Felucca family) | Timing of host USB frames |
| Audio/DMA | Unchanged Felucca renders stereo SRAM, alternates ALNK halves, services audio IRQs; note samples are nonzero; the window plays them | Other clocks/formats, codec analog behavior, cycle timing |

Original foundation evidence: seventeen Rust integration tests passed; a native boot executes
2,371 instructions, services one guest interrupt, and reaches `foundation_done`.
The guest's last result is `0x0050F00D`. See `build/foundation/verification.txt`.

Run the suite with `mise run rust-gui-test`; tests that need firmware are
`#[ignore]`d and read their package from an environment variable (for example
`FELUCCA_FWSC`, `FM1_STOCK_FWSC`, `MIDI_FWSC`, `USB_AUDIO_ELF`). See `build/display/verification.txt` for the display milestone and limitations.

## Scripted playback and profiling

`examples/play_check` replays what a user does at the panel, headless and
deterministic, through `fm1_emu::player` (the same key matrix and encoder
model as the window):

```sh
cargo run --release --example play_check -- FIRMWARE.fwsc \
  run:4 hold:18,20,22 level:1 release turn:PRESETS:2 run:1 png:after.png
```

Steps:

| Step | Does |
|---|---|
| `run:SECONDS` | Run that much guest time |
| `hold:ID,ID` | Press matrix keys (added to those held; notes are 14..40) |
| `release` | Let go of every held key |
| `release:ID,ID` | Let go of those keys only (an error if one is not held) |
| `turn:KNOB:DETENTS` | Turn SELECT, ALGORITHM, PRESETS or KNOB1..KNOB4 |
| `click:KNOB:N` | N settled detents, as a GUI click |
| `master:VALUE` | Set the MASTER potentiometer, 0..1023 as the ADC reads it |
| `align` | Wait for the next audio DMA half, so a following hold or release lands in one audio block |
| `level:SECONDS` | Run and print the RMS and peak of the guest output |
| `wav:SECONDS:PATH` | Record the exact 24-bit samples |
| `png:PATH` | Save the LCD |
| `leds:SECONDS` | Run and print each panel LED's brightness (1 = lit whenever its column is scanned) |
| `words:ADDRESS:N`, `halves:ADDRESS:N` | Read N words; non-zero words per DMA half |
| `cores` | Instructions run per core |
| `hot:on`, `hot:off`, `hot:print`, `peek:SYMBOL:WORDS` | Profile part of a session; read guest variables (below) |

A guest fault prints the last instructions with their registers
(`PLAY_TRACE=N`). `FM1_CPU_MHZ=N` sets the instruction clock;
`FM1_NESTED_IRQ=1` and `FM1_IDLE_SKIP=0` as in the table above.
`play_check [--state PATH [--fresh]] FIRMWARE STEP...` opts in to a flash state.

Other examples, all headless (`cargo run --release --example NAME -- ...`; each
has its usage in the header of its source file): `knob_check FIRMWARE KNOB
DETENTS OUT_PREFIX` (does a turned encoder change the screen?), `diagnose`
(bounded boot report), `latency FWSC MODE` (input-to-audio time for a key,
USB-MIDI or TRS MIDI note, and MIDI-clock scenarios), `extract` (a package's
decoded application image), and `bench FIRMWARE LIMIT [batch|hash|gui]`
(instructions per second, with hashes of state, SRAM, audio and LCD so two
builds can be compared; see [PERFORMANCE.md](PERFORMANCE.md)).

`FM1_HOT=N` counts every primary-core instruction by PC and reports the top
N functions, using the sized `STT_FUNC` symbols of `FM1_ELF` (or an `.elf`
next to the firmware), then the hot address ranges (loop bodies) inside
them; `hot:on`, `hot:off` and `hot:print` limit it to part of a session,
`peek:SYMBOL:WORDS` reads guest variables, and `FM1_HOT_DUMP=FILE` writes
every executed PC with its count. Counts are instructions, not cycles.

`examples/preset_sweep FIRMWARE OUT_DIR` clicks PRESETS through every preset,
plays a chord on each and reports silent presets and guest faults; a fault
reboots and continues from the next preset.

## Bluetooth LE (BLE MIDI) model

The firmware drives the WL82 BLE baseband engine (registers `0x28000-0x28054`,
a per-link column port and control blocks in the baseband RAM window). The
emulator has a **behavioural model** of that engine and a **virtual BLE
central** on a virtual air interface. Nothing here ran against hardware: every
engine behaviour is a hypothesis built from the fact sheet
(`docs/BLE-HW-FACTS.md` in the optimist repository, branch `feat/ble-facts`),
and the code comments cite its sections. Sources: `src/ble_air.rs` (PDUs, CSA
#1), `src/ble_param.rs` (control block and DMA window), `src/ble_engine/`,
`src/ble_central/`, `src/isr_profile.rs`.

### What is modelled

- Advertising (ADV_IND on channels 37/38/39 every interval plus a seeded
  0-10 ms advDelay), SCAN_REQ/SCAN_RSP, CONNECT_IND, and the connected state:
  connection events on CSA #1 channels from the 24-bit link clock, receive
  into the RX buffers, SN/NESN and the TX buffer toggle, empty PDUs, event
  (IRQ 45) and RX (IRQ 29) interrupts, connection-update and channel-map
  instants, supervision timeout.
- Scanning (link state 1, fact sheet §21.2): windows every interval on the
  channel column 6 held when the window opened, the event interrupt at each
  window's end, reports stored the connection-path way (RXTOG buffer,
  RXBUFnCNTL bit0, RSSI2, LASTCHMAP) with the RX interrupt, and the engine's
  own SCAN_REQ (AdvA filled in) answered by **virtual advertisers**
  (`FM1_BLE_ADVERTISERS`, `src/ble_engine/scanning.rs`).
- The central side: scanning, CONNECT_IND, the master clock and hop schedule,
  LL procedures (version, features, connection update, channel map,
  terminate), and an L2CAP/ATT client that does MTU exchange, GATT discovery
  the way CoreBluetooth does, the BLE-MIDI read, CCCD subscribe, writes
  without response and notifications.
- Not modelled: the chip's hardware AES block at `0x41200` (see the stub
  below), encryption, data length extension, radio timing below a microsecond.

### Hypotheses switchable through `FM1_BLE_MODEL`

`FM1_BLE_MODEL=key=value,...` sets engine choices where the fact sheet leaves
the behaviour open (`EngineConfig::apply`). Flags take `0/1`. Defaults in
brackets.

| Key | Values | Meaning |
|---|---|---|
| `scan_rsp` | `auto`, `software` [`auto`] | The engine answers a SCAN_REQ with TX buffer 1 by itself, or only stores the request |
| `scan_req_irq` | flag [0] | Also store an answered SCAN_REQ and raise the RX interrupt |
| `stop_adv` | flag [1] | The engine stops advertising by itself on a CONNECT_IND for it |
| `adv_irq_on_connect` | flag [1] | Event interrupt at the end of an advertising event cut short by a CONNECT_IND |
| `store_empty` | flag [1] | New empty PDUs are stored and signalled (0: acknowledged without a buffer or interrupt) |
| `old_sn` | `drop`, `irq`, `store` [`drop`] | A repeated SN is dropped silently, dropped with IRQ 29, or stored like new data |
| `rx_count_repeats` | flag [1] | A dropped repeat still updates EVTCOUNT |
| `event_irq_every_event` | flag [1] | IRQ 45 after every connection event, not only those with a reception |
| `engine_retransmits` | flag [1] | The engine resends an unacknowledged PDU itself, or sends whatever TXTOG selects |
| `pairs` | number [1] | Request/response pairs per event while MD is set |
| `instant_gating` | flag [1] | Interval/offset changes (columns 1/15/4) take effect only at the column-5 instant |
| `rx_needs_armed` | flag [0] | Store only into an RX buffer whose RXBUFnCNTL bit0 is 0; stock V15 never receives with it on |
| `seed` | number | Seed of the advDelay generator |
| `scan_cntl` | flag [1] | §21.9 C2: while scanning, set RXBUFnCNTL bit0 on the filled buffer (0: leave it 0, RXTOG alone moves) |
| `scan_adva` | flag [1] | §21.9 C1: the engine writes the advertiser's AdvA into the SCAN_REQ (0: the zeros go out, nobody answers) |

### The virtual central and its variables

Used by `examples/diagnose` (and by `Bus::ble_attach_central` in tests).

| Variable | Effect |
|---|---|
| `FM1_BLE_CENTRAL=script\|scan\|off` | `script` runs the central's script, `scan` only scans, unset or `off`: no central (the engine alone) |
| `FM1_BLE_SCRIPT=PATH` | Script file for `script` (default: the built-in script) |
| `FM1_BLE_MODEL=key=value,...` | Engine hypotheses, table above (works without a central too) |
| `FM1_BLE_LOSS=N` | The central misses every Nth peripheral data PDU (retransmission on both sides) |
| `FM1_BLE_PAIRS=N` | Up to N central/peripheral pairs per event while MD is set (default 1) |
| `FM1_BLE_SMP=ignore` | Leave an SMP Security Request unanswered (default: answer with Pairing Failed) |
| `FM1_BLE_WINDOW_DELAY_US=N` | Where in the transmit window the first packet goes |
| `FM1_BLE_LOG=PATH` | Write every packet on the virtual air to PATH (otherwise the first 40 are printed) |
| `FM1_BLE_ISR=1` | Report interrupt-handler timing on CPU 0 (handler length in instructions and guest time, delay of IRQ 29/45 entry, share spent in the audio handler IRQ 11). Nesting is off by default, so it cannot say whether 29/45 could preempt audio |
| `FM1_BLE_ADVERTISERS=default\|SPEC` | Virtual advertisers for a scanning firmware to hear: `default` = KeyStep 37 (BLE-MIDI, name in the scan response), WIDI (name in the ADV_IND), FM-1 B00B, iPad AUM (a resolvable private address), Tile (a beacon), NonConn MIDI (not connectable); or `midi:NAME[:adv][:ms=N][:rssi=W];beacon:NAME;...`. Works without a central (`FM1_BLE_CENTRAL` unset); the air log marks their packets `A->S` |
| `FM1_PRESS=STEP:COLUMN:ROW:LENGTH[,...]` | Hold a panel matrix contact from instruction STEP for LENGTH instructions (a key press, to see notifications) |

A script is one step per line, `#` starts a comment, options are `key=value`
(`interval`, `latency`, `timeout`, `winsize`, `winoffset`, `hop`, `chmap` in
hex). Steps: `scan N`, `connect [opts]`, `wait N` (connection events),
`version`, `features`, `mtu N`, `discover`, `read`, `subscribe`,
`midi BYTE...` (hex), `notifications N`, `update [opts]`, `chmap HEX`,
`terminate`, `vanish` (stop transmitting; link loss). The built-in script scans,
connects, exchanges version/features/MTU, discovers GATT, subscribes, plays a
note on and off, updates the channel map and interval, and terminates.

### Running the stock V15 firmware

Unchanged stock V15 advertises, connects, is discovered and exchanges MIDI both
ways under the model. After the CONNECT_IND it uses the unmodelled block at
`0x41200` (per the fact sheet the chip's hardware AES block), so that block is
stubbed:

```sh
cargo build --release --example diagnose
FM1_BLE_CENTRAL=script FM1_BLE_SMP=ignore FM1_STUB_MMIO=41200-412ff:24 FM1_BLE_LOG=air.txt FM1_BLE_ISR=1 target/release/examples/diagnose FM-1.fwsc 1300000000
```

`FM1_BLE_SMP=ignore` is needed because the stock firmware sends an SMP Security
Request, and if the central answers Pairing Failed it terminates the link
(LL_TERMINATE_IND 0x13) about 300 ms later. The report lists the step results,
engine counters, the air log and the number of non-silent audio frames.

The ignored integration tests in `tests/ble.rs` run unchanged stock V15
against the model (they need the firmware, so they are skipped by default):

```sh
FM1_STOCK_FWSC=/path/to/FM-1.fwsc cargo test --release --test ble -- --ignored
```

### Tracing and experiment variables (`diagnose`)

| Variable | Effect |
|---|---|
| `FM1_MMIO_TRACE=PATH` | Write every MMIO write in `FM1_MMIO_RANGES` to PATH in execution order, one line per access: `step tick pc R\|W address size value [FAULT]`. Needs `FM1_MMIO_RANGES` |
| `FM1_MMIO_RANGES=LOW-HIGH[,...]` | Hex inclusive address ranges to trace |
| `FM1_MMIO_READS=1` | Also trace reads, with the value returned |
| `FM1_MMIO_TRACE_LIMIT=N` | Stop recording after N accesses (default 2,000,000) |
| `FM1_FLASH_DUMP=PATH` | Save the raw serial flash at the end and list the sectors programmed or erased |
| `FM1_FLASH_RESTORE=PATH` | Before the first instruction, lay every 4 KiB sector of a dump that differs from the package over the flash (a second boot of the same device) |
| `FM1_STUB_MMIO=LOW-HIGH[:SET[:CLEAR]][,...]` | Experiments only: unimplemented MMIO in these hex ranges reads back the last word written instead of faulting; SET bits are set and CLEAR bits cleared on every read (`41200-412ff:24` sets bits 2 and 5 on every read). The output lists every stubbed word touched |
| `FM1_STUB_CONSOLE=ADDRESS` | Collect the bytes written to this address (a debug UART data register inside a stubbed range) and print them as the guest's console |

A stub invents a register the firmware polls; a result that depends on one
shows what the firmware does if the block behaves that way, not how the chip
behaves.
