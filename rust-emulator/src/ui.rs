// SPDX-License-Identifier: GPL-3.0-only
// The Windows GUI subsystem: double-clicking the program opens the window and
// no console. The command line still works from a terminal: ui_console joins
// the parent's console at start, so messages and stdin behave as before.
#![cfg_attr(windows, windows_subsystem = "windows")]
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind};
use fm1_emu::encoders::knob;
#[cfg(test)]
use fm1_emu::{bus::Bus, cpu::Cpu, firmware::Firmware};
mod host_audio;
mod ui_app;
mod ui_console;
mod ui_leds;
mod ui_settings;
mod ui_theme;
mod web_editor;
mod worker;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

// Matrix IDs/wiring from fm1_input.h and the pinned Felucca panel.c defaults.
// Copyright (C) 2026 Leo Kuroshita (@kurogedelic), Hügelton Instruments.
const KEYMAP: [[i8; 11]; 4] = [
    [5, 11, 4, 10, 3, 9, 2, 8, -1, -1, -1],
    [34, 35, 36, 37, 38, 40, 39, 13, 7, 6, 12],
    [23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33],
    [0, 1, 15, 14, 17, 16, 19, 18, 20, 21, 22],
];
const NOTE_KEYS: [egui::Key; 13] = [
    egui::Key::A,
    egui::Key::W,
    egui::Key::S,
    egui::Key::E,
    egui::Key::D,
    egui::Key::R,
    egui::Key::F,
    egui::Key::G,
    egui::Key::T,
    egui::Key::H,
    egui::Key::Y,
    egui::Key::J,
    egui::Key::K,
];
/// Computer keys that hold the fourteen buttons (matrix ids 0..=13): the
/// arrows for OCT, Space for PLAY / STOP, and letters the note and knob keys
/// leave free (mostly from the button's name) for the others, so a layer
/// chord such as SAVE + note is one key held while another is tapped.
/// Modifiers are not used: egui reports them as state, not as keys, without
/// telling left from right.
const BUTTON_KEYS: [egui::Key; 14] = [
    egui::Key::ArrowLeft,  // OCT−
    egui::Key::ArrowRight, // OCT+
    egui::Key::X,          // FX
    egui::Key::B,          // SEL
    egui::Key::V,          // ENV
    egui::Key::L,          // LFO
    egui::Key::I,          // EDIT
    egui::Key::O,          // GLO
    egui::Key::U,          // HOME
    egui::Key::Z,          // SAVE
    egui::Key::P,          // ARP
    egui::Key::Q,          // SEQ
    egui::Key::Space,      // PLAY / STOP
    egui::Key::C,          // REC
];
/// The computer key that holds matrix contact `id`, if any.
fn key_binding(id: usize) -> Option<egui::Key> {
    match id {
        0..=13 => Some(BUTTON_KEYS[id]),
        14..=39 => Some(NOTE_KEYS[(id - 14) % NOTE_KEYS.len()]),
        _ => None,
    }
}
/// First matrix id of the upper octave: the same computer keys, with Shift held.
const UPPER_NOTES: usize = 27;
/// Whether note `id` is played with Shift held. Ids 27..=39 are the 13 bound
/// keys one octave up; id 40 (the topmost key) has no computer key.
fn needs_shift(id: usize) -> bool {
    id >= UPPER_NOTES
}
/// A button's tooltip, ending with its computer key ("PLAY / STOP · hold to
/// press · Space").
fn button_hint(id: usize, label: &str) -> String {
    let label = label.replace('\n', " / ");
    match key_binding(id) {
        Some(key) => format!("{label} · hold to press · {}", key.name()),
        None => format!("{label} · hold to press"),
    }
}
/// A note key's tooltip, ending with its computer key ("Note 53 · hold to
/// press · A"). The matrix id is fixed; the octave keys shift notes in the
/// firmware, so the binding never changes.
fn note_hint(id: usize) -> String {
    let note = id - 14 + 53;
    match key_binding(id) {
        Some(key) if needs_shift(id) => {
            format!("Note {note} · hold to press · Shift+{}", key.name())
        }
        Some(key) => format!("Note {note} · hold to press · {}", key.name()),
        None => format!("Note {note} · hold to press"),
    }
}
/// Where to draw the 240x240 LCD inside `area` (points) so that no guest pixel
/// is dropped: never fewer than 240 physical pixels (nearest-neighbour
/// downscaling skips rows and columns, so "TRACK" read "IRALK"), an integer
/// scale when it fills at least 3/4 of the area, and snapped to the pixel grid.
fn lcd_rect(area: Rect, pixels_per_point: f32) -> Rect {
    const LCD: f32 = 240.;
    let available = area.width().min(area.height()) * pixels_per_point;
    let whole = (available / LCD).floor();
    let pixels = if available < LCD {
        LCD
    } else if whole * LCD >= 0.75 * available {
        whole * LCD
    } else {
        available.floor()
    };
    let size = pixels / pixels_per_point;
    let min =
        ((area.center() - vec2(size, size) / 2.) * pixels_per_point).round() / pixels_per_point;
    Rect::from_min_size(min, vec2(size, size))
}
/// Drawn knobs: position, label, matrix encoder (None: the MASTER pot) and
/// the keys that turn them (counter-clockwise / clockwise).
const KNOBS: [(f32, f32, &str, Option<usize>, &str); 8] = [
    (91., 106., "MASTER", None, "N / M"),
    (208., 106., "SELECT", Some(knob::SELECT), "[ / ]"),
    (91., 224., "PRESETS", Some(knob::PRESETS), "9 / 0"),
    (208., 224., "ALGORITHM", Some(knob::ALGORITHM), "- / ="),
    (632., 106., "KNOB1", Some(knob::KNOB1), "1 / 2"),
    (758., 106., "KNOB2", Some(knob::KNOB1 + 1), "3 / 4"),
    (884., 106., "KNOB3", Some(knob::KNOB1 + 2), "5 / 6"),
    (1010., 106., "KNOB4", Some(knob::KNOB1 + 3), "7 / 8"),
];
const KNOB_KEYS: [(egui::Key, egui::Key); 8] = [
    (egui::Key::N, egui::Key::M),
    (egui::Key::OpenBracket, egui::Key::CloseBracket),
    (egui::Key::Num9, egui::Key::Num0),
    (egui::Key::Minus, egui::Key::Equals),
    (egui::Key::Num1, egui::Key::Num2),
    (egui::Key::Num3, egui::Key::Num4),
    (egui::Key::Num5, egui::Key::Num6),
    (egui::Key::Num7, egui::Key::Num8),
];
/// Pointer travel (points of drag, or half-points of scroll) per encoder click.
const DETENT_PX: f32 = 12.;
/// Encoder clicks per turn as drawn (the pointer moves 15 degrees a click).
const DETENT_ANGLE: f32 = std::f32::consts::TAU / 24.;
/// Rounding slack when a drag's summed angle is turned into whole encoder clicks.
const DETENT_EPS: f32 = 1e-3;
/// Instruction clock choices: None follows the firmware's system clock (the
/// accurate default); a lower rate gives the guest fewer instructions per
/// second of guest time, so a light firmware can play in real time.
const CLOCKS: [(Option<u32>, &str); 6] = [
    (None, "Firmware clock"),
    (Some(24), "24 MHz"),
    (Some(48), "48 MHz"),
    (Some(96), "96 MHz"),
    (Some(192), "192 MHz"),
    (Some(312), "312 MHz"),
];
/// The ADC model's MASTER reading at reset.
const MASTER_DEFAULT: u16 = 512;
/// MASTER pointer: -135..+135 degrees over the ADC range 0..=1023.
fn master_angle(master: u16) -> f32 {
    (master as f32 / 1023. - 0.5) * 1.5 * std::f32::consts::PI
}
/// Longest wait for the flash state to be saved when quitting.
const QUIT_SAVE_WAIT: Duration = Duration::from_secs(5);
const INK: Color32 = Color32::from_rgb(190, 194, 193);
const ACCENT: Color32 = Color32::from_rgb(231, 193, 91);

/// The firmware formats the loader reads: `.fwsc` packages, ELF files and raw
/// application images.
const FIRMWARE_EXTENSIONS: [&str; 3] = ["fwsc", "elf", "bin"];

/// Whether `path` looks like a firmware file (by its extension).
fn is_firmware_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            FIRMWARE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// The window title for the running firmware.
fn window_title(path: Option<&std::path::Path>) -> String {
    match path.and_then(|path| path.file_name()) {
        Some(name) => format!("{} - FM-1 Emulator", name.to_string_lossy()),
        None => "FM-1 Emulator".into(),
    }
}

struct Emulator {
    /// The firmware that runs; None until one is loaded.
    path: Option<PathBuf>,
    /// `--ui DIR`: web editor files for every firmware, if given.
    ui_dir: Option<PathBuf>,
    /// Firmware files opened, the latest first, and whether to reopen the
    /// latest at start (both kept in the settings).
    recent: Vec<PathBuf>,
    reopen_last: bool,
    /// "Load firmware…" clicked: open the file dialog at the next frame.
    pick_requested: bool,
    /// The last thing that went wrong with a firmware file, for the status line.
    notice: Option<String>,
    /// The window title last sent to the window system.
    title: String,
    worker: worker::Worker,
    generation: u64,
    loaded: bool,
    steps: u64,
    fault: Option<String>,
    paused: bool,
    texture: Option<egui::TextureHandle>,
    pressed: [bool; 41],
    pulse: [Instant; 41],
    pulse_steps: [u64; 41],
    /// For each note computer key, the octave it was pressed in (`true` =
    /// Shift held at key-down) until it is released, so letting go of Shift
    /// first releases the key that was pressed, not another one.
    note_layer: [Option<bool>; 13],
    /// MASTER potentiometer as the ADC reads it (0..=1023).
    master: u16,
    /// Pointer angle per drawn knob (radians, 0 = up), unspent drag/scroll
    /// and the knob centres of the last drawn frame.
    knob_angle: [f32; 8],
    knob_accum: [f32; 8],
    knob_centre: [egui::Pos2; 8],
    /// Instruction clock in MHz; None follows the firmware.
    clock_mhz: Option<u32>,
    /// Host playback; None in tests or when no device opens (`audio_error`).
    audio: Option<host_audio::HostAudio>,
    audio_error: Option<String>,
    /// Guest audio against real time, sampled about once a second.
    speed: Option<f64>,
    speed_mark: (Instant, u64),
    /// The firmware's web editor and its MIDI bridge (off without one).
    web: web_editor::WebEditor,
    /// The panel LEDs as the guest lights them.
    leds: ui_leds::LedView,
    /// The panel's colours (an index into `ui_theme::THEMES`).
    theme: usize,
    /// The flash is kept between runs (`--state`; off in tests).
    keeps_flash: bool,
    /// The flash state's last event, for the status line.
    state_status: Option<String>,
    /// "Reset flash state" asked; waiting for the confirmation.
    confirm_reset: bool,
    /// The window's settings kept between runs (None in tests).
    settings: Option<ui_settings::Saver>,
    /// The saved theme and the one the run started with: a `--theme` for
    /// one run is not remembered unless the toolbar changes it.
    theme_saved: Option<String>,
    theme_start: usize,
    /// The window's inner size (points) as of the last frame.
    window_size: Option<[u32; 2]>,
}
impl Emulator {
    fn new(path: Option<PathBuf>) -> Self {
        let mut app = Self {
            title: window_title(path.as_deref()),
            path,
            ui_dir: None,
            recent: Vec::new(),
            reopen_last: true,
            pick_requested: false,
            notice: None,
            worker: worker::Worker::new(),
            generation: 0,
            loaded: false,
            steps: 0,
            fault: None,
            paused: false,
            texture: None,
            pressed: [false; 41],
            pulse: [Instant::now(); 41],
            pulse_steps: [0; 41],
            note_layer: [None; 13],
            master: MASTER_DEFAULT,
            knob_angle: [0.; 8],
            knob_accum: [0.; 8],
            knob_centre: [pos2(0., 0.); 8],
            clock_mhz: None,
            audio: None,
            audio_error: None,
            speed: None,
            speed_mark: (Instant::now(), 0),
            web: web_editor::WebEditor::disabled("No web editor"),
            leds: ui_leds::LedView::new(),
            theme: 0,
            keeps_flash: false,
            state_status: None,
            confirm_reset: false,
            settings: None,
            theme_saved: None,
            theme_start: 0,
            window_size: None,
        };
        app.knob_angle[0] = master_angle(app.master);
        app.reset();
        app
    }
    /// Take the remembered MASTER, theme and LEDs switch; `theme` (from
    /// `--theme` or FM1_THEME) wins over the saved theme for this run.
    fn apply_settings(&mut self, settings: &ui_settings::Settings, theme: Option<usize>) {
        self.set_master(settings.master);
        self.leds.enabled = settings.leds;
        self.theme_saved = settings.theme.clone();
        let saved = settings.theme.as_deref().and_then(ui_theme::find);
        if let (Some(name), None) = (&settings.theme, saved) {
            eprintln!(
                "saved theme {name:?} is unknown; themes: {}",
                ui_theme::names()
            );
        }
        self.theme = theme.or(saved).unwrap_or(0);
        self.theme_start = self.theme;
        self.recent = settings.recent.clone();
        self.reopen_last = settings.reopen_last;
    }
    fn set_master(&mut self, master: u16) {
        self.master = master.min(1023);
        self.knob_accum[0] = 0.;
        self.knob_angle[0] = master_angle(self.master);
        self.worker.master(self.master);
    }
    /// The settings as the window has them now.
    fn current_settings(&self, window: Option<[u32; 2]>) -> ui_settings::Settings {
        let theme = if self.theme == self.theme_start {
            self.theme_saved.clone()
        } else {
            Some(ui_theme::THEMES[self.theme].name.to_string())
        };
        ui_settings::Settings {
            master: self.master,
            theme,
            leds: self.leds.enabled,
            window,
            recent: self.recent.clone(),
            reopen_last: self.reopen_last,
        }
    }
    /// Save the settings once they have settled (no-op without a saver).
    fn keep_settings(&mut self, ctx: &egui::Context) {
        if self.settings.is_none() {
            return;
        }
        let window = ctx.input(|i| {
            i.viewport()
                .inner_rect
                .map(|rect| [rect.width().round() as u32, rect.height().round() as u32])
        });
        self.window_size = window.or(self.window_size);
        let current = self.current_settings(self.window_size);
        if let Some(Err(error)) = self
            .settings
            .as_mut()
            .and_then(|saver| saver.update(&current, Instant::now()))
        {
            eprintln!("window settings not saved: {error}");
        }
    }
    fn reset(&mut self) {
        self.clear();
        if let Some(path) = &self.path {
            self.worker.restart(self.generation, path.clone());
        }
    }
    /// Boot `path` as a power cycle: the running firmware's flash state is
    /// saved first (the worker does that when it restarts), the CPU and
    /// devices are rebuilt from the new package, and that firmware's own flash
    /// state (its family) is restored. The editor follows the new firmware.
    fn load_firmware(&mut self, path: PathBuf) {
        self.notice = None;
        // Free the editor's port before the next firmware's editor takes it.
        self.web = web_editor::WebEditor::disabled("No web editor");
        self.web = web_editor::WebEditor::new(&path, self.ui_dir.as_deref(), fm1_emu::web::ADDRESS);
        self.worker.web(self.web.hub());
        match self.web.url() {
            Some(url) => eprintln!("web editor at {url}"),
            None => eprintln!("{}", self.web.unavailable().unwrap_or_default()),
        }
        ui_settings::remember(&mut self.recent, &path);
        self.path = Some(path);
        self.reset();
    }
    /// Load a firmware file the user named (dialog, recent list, drop).
    fn open_firmware(&mut self, path: PathBuf) {
        if is_firmware_file(&path) {
            self.load_firmware(path);
        } else {
            self.notice = Some(format!(
                "{} is not a firmware file (.fwsc, .elf or .bin)",
                path.display()
            ));
        }
    }
    /// Ask for a firmware file with the system's own dialog (blocks the
    /// window while it is open; it is modal anyway) and load it.
    fn pick_firmware(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Load firmware")
            .add_filter("FM-1 firmware", &FIRMWARE_EXTENSIONS)
            .add_filter("All files", &["*"]);
        if let Some(folder) = self
            .path
            .as_deref()
            .or(self.recent.first().map(PathBuf::as_path))
            .and_then(std::path::Path::parent)
            .filter(|folder| folder.is_dir())
        {
            dialog = dialog.set_directory(folder);
        }
        if let Some(path) = dialog.pick_file() {
            self.open_firmware(path);
        }
    }
    /// The toolbar's firmware controls: the load button and the recent list.
    fn firmware_controls(&mut self, ui: &mut egui::Ui) {
        if ui
            .button("Load firmware…")
            .on_hover_text("Pick a .fwsc, .elf or .bin file. The emulator reboots into it; the running firmware's flash state is saved first and the new one's is restored. You can also drop a file on the window.")
            .clicked()
        {
            self.pick_requested = true;
        }
        let mut chosen = None;
        ui.menu_button("Recent", |ui| {
            if self.recent.is_empty() {
                ui.weak("No firmware opened yet");
            }
            for path in &self.recent {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if ui
                    .button(name)
                    .on_hover_text(path.display().to_string())
                    .clicked()
                {
                    chosen = Some(path.clone());
                    ui.close_menu();
                }
            }
            ui.separator();
            ui.checkbox(&mut self.reopen_last, "Reopen the last firmware at start")
                .on_hover_text("Without a firmware on the command line, start with the latest one if it still exists");
        });
        if let Some(path) = chosen {
            self.open_firmware(path);
        }
    }
    /// What the window shows before any firmware is loaded.
    fn idle_prompt(&mut self, ctx: &egui::Context) {
        egui::Area::new(egui::Id::new("open_firmware_prompt"))
            .anchor(Align2::CENTER_CENTER, vec2(0., 0.))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(24.)
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading("No firmware loaded");
                            ui.label(
                                "Load a .fwsc package (or an .elf / .bin image) to start the FM-1.",
                            );
                            ui.add_space(8.);
                            if ui
                                .add(egui::Button::new(
                                    egui::RichText::new("Load firmware…").size(18.),
                                ))
                                .clicked()
                            {
                                self.pick_requested = true;
                            }
                            ui.weak("or drop a firmware file on this window");
                        });
                    });
            });
    }
    /// Firmware files dropped on the window: the first one is loaded.
    fn dropped_firmware(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if let Some(path) = dropped.into_iter().find_map(|file| file.path) {
            self.open_firmware(path);
        }
    }
    /// The view of a machine about to start (a new generation).
    fn clear(&mut self) {
        self.pressed.fill(false);
        self.pulse.fill(Instant::now());
        self.pulse_steps.fill(0);
        self.paused = false;
        self.texture = None;
        self.leds.reset();
        self.generation += 1;
        self.loaded = false;
        self.steps = 0;
        self.fault = None;
    }
    /// Forget what the firmware saved to flash and start from the package
    /// alone, as a device fresh from an update with its data erased.
    fn reset_flash(&mut self) {
        self.clear();
        self.state_status = None;
        self.worker.reset_state(self.generation);
    }
    /// The flash menu and its confirmation window.
    fn flash_menu(&mut self, ui: &mut egui::Ui) {
        ui.add_enabled_ui(self.keeps_flash, |ui| {
            ui.menu_button("Flash", |ui| {
                if ui.button("Reset flash state…").clicked() {
                    self.confirm_reset = true;
                    ui.close_menu();
                }
            })
            .response
            .on_hover_text("What the firmware wrote to flash (projects, presets, settings) is kept between runs (--state; --fresh starts clean)")
            .on_disabled_hover_text("The flash is not kept between runs");
        });
        if !self.confirm_reset {
            return;
        }
        let mut open = true;
        egui::Window::new("Reset flash state")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0., 0.))
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label("Erase everything this firmware saved to flash (projects, autosave, presets, settings) and restart from the package alone?");
                ui.horizontal(|ui| {
                    if ui.button("Reset and restart").clicked() {
                        self.confirm_reset = false;
                        self.reset_flash();
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_reset = false;
                    }
                });
            });
        if !open {
            self.confirm_reset = false;
        }
    }
    fn refresh(&mut self, ctx: &egui::Context) {
        self.worker.input(self.pressed);
        self.worker.pause(self.paused);
        let Some(snapshot) = self.worker.snapshot() else {
            return;
        };
        if snapshot.generation != self.generation {
            return;
        }
        self.loaded = snapshot.loaded;
        self.steps = snapshot.steps;
        if snapshot.state.is_some() {
            self.state_status = snapshot.state;
        }
        let (since, frames) = self.speed_mark;
        let elapsed = since.elapsed().as_secs_f64();
        if snapshot.frames < frames || self.paused {
            self.speed = None;
            self.speed_mark = (Instant::now(), snapshot.frames);
        } else if elapsed >= 1.0 {
            let guest = (snapshot.frames - frames) as f64 / fm1_emu::audio::SAMPLE_RATE as f64;
            self.speed = (snapshot.frames > 0).then_some(guest / elapsed);
            self.speed_mark = (Instant::now(), snapshot.frames);
        }
        self.fault = snapshot.fault;
        if let Some(leds) = snapshot.leds {
            self.leds.receive(leds);
        }
        let Some(pixels) = snapshot.pixels else {
            return;
        };
        let image = egui::ColorImage {
            size: [240, 240],
            pixels: pixels
                .into_iter()
                .map(|rgb| Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
                .collect(),
        };
        if let Some(texture) = &mut self.texture {
            texture.set(image, egui::TextureOptions::NEAREST);
        } else {
            self.texture =
                Some(ctx.load_texture("Guest LCD", image, egui::TextureOptions::NEAREST));
        }
    }
    /// Steps that make at least 100 ms of guest time for a click: two
    /// issued core instructions per shared clock step at the selected clock,
    /// or 360 MHz (the firmware's own clock is not known here). A fixed
    /// 72 M steps held a click 0.4-0.75 s of guest time at 96 MHz.
    fn pulse_len(&self) -> u64 {
        u64::from(self.clock_mhz.unwrap_or(360)) * 200_000
    }
    /// Whether the computer key bound to contact `id` holds it, and whether it
    /// was just struck. Buttons follow their key; a note key plays the octave
    /// chosen by Shift at key-down and keeps it until key-up.
    fn keyboard_state(
        &mut self,
        ui: &egui::Ui,
        id: usize,
        binding: Option<egui::Key>,
    ) -> (bool, bool) {
        let Some(key) = binding else {
            return (false, false);
        };
        if id < 14 {
            return ui.input(|i| (i.key_down(key), i.key_pressed(key)));
        }
        let slot = (id - 14) % NOTE_KEYS.len();
        let (down, struck) = ui.input(|i| {
            let strike = i.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key: k,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } if *k == key => Some(modifiers.shift),
                _ => None,
            });
            (i.key_down(key), strike)
        });
        if !down && struck.is_none() {
            self.note_layer[slot] = None;
        } else if let Some(shift) = struck {
            self.note_layer[slot] = Some(shift);
        } else if self.note_layer[slot].is_none() {
            self.note_layer[slot] = Some(ui.input(|i| i.modifiers.shift));
        }
        let mine = self.note_layer[slot] == Some(needs_shift(id));
        (mine && down, mine && struck.is_some())
    }
    fn key(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &Canvas,
        id: usize,
        rect: Rect,
        label: &str,
        note: bool,
    ) {
        let response = ui.interact(rect, egui::Id::new(("key", id)), Sense::click_and_drag());
        let down = response.is_pointer_button_down_on();
        if down || response.clicked() {
            self.pulse[id] = Instant::now() + Duration::from_millis(100);
            self.pulse_steps[id] = self.steps + self.pulse_len();
        }
        let focused = ui.input(|i| i.focused);
        let binding = key_binding(id);
        let (keyboard, key_hit) = self.keyboard_state(ui, id, binding);
        if key_hit {
            self.pulse[id] = Instant::now() + Duration::from_millis(100);
            self.pulse_steps[id] = self.steps + self.pulse_len();
        }
        if !focused {
            self.pulse[id] = Instant::now();
            self.pulse_steps[id] = 0;
        }
        // A slow host gives the guest at least 100 ms at up to 360 MHz,
        // including two issued core instructions per shared clock step, to scan
        // and debounce a click; the wall-clock pulse keeps visual feedback.
        let guest_pulse = self.steps < self.pulse_steps[id];
        let pressed =
            focused && (down || keyboard || Instant::now() < self.pulse[id] || guest_pulse);
        self.pressed[id] = pressed;
        let theme = canvas.theme;
        let fill = if pressed {
            theme.key_pressed
        } else if response.hovered() {
            theme.key_hover
        } else {
            theme.key
        };
        let ink = if pressed {
            theme.pressed
        } else {
            theme.key_label
        };
        let radius = if note { 21. } else { 7. } * canvas.scale;
        canvas.painter.rect_filled(
            rect.translate(vec2(0., 3.) * canvas.scale),
            radius,
            theme.key_shadow,
        );
        self.leds
            .halo(&canvas.painter, rect, radius, id, canvas.scale);
        canvas.painter.rect(
            rect,
            radius,
            fill,
            Stroke::new(canvas.scale, theme.key_edge),
            StrokeKind::Inside,
        );
        self.leds.tint(&canvas.painter, rect, radius, id);
        let inset = rect.shrink(4. * canvas.scale);
        canvas.painter.rect_stroke(
            inset,
            radius,
            Stroke::new(canvas.scale, theme.key_inset),
            StrokeKind::Inside,
        );
        if note {
            let y = if label.is_empty() {
                rect.center().y
            } else {
                rect.center().y - 9. * canvas.scale
            };
            canvas.painter.rect_filled(
                Rect::from_center_size(pos2(rect.center().x, y), vec2(6., 31.) * canvas.scale),
                3.,
                self.leds
                    .ink(id, if pressed { theme.pressed } else { theme.slot }),
            );
            if !label.is_empty() {
                canvas.painter.text(
                    pos2(rect.center().x, rect.bottom() - 19. * canvas.scale),
                    Align2::CENTER_CENTER,
                    label,
                    FontId::proportional(11. * canvas.scale),
                    self.leds.ink(id, theme.key_label),
                );
            }
        } else {
            canvas.painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                label,
                FontId::proportional(12. * canvas.scale),
                self.leds.ink(id, ink),
            );
        }
        response.on_hover_text(if note {
            note_hint(id)
        } else {
            button_hint(id, label)
        });
    }
    /// Pointer movement on drawn knob `index`: whole encoder clicks are queued
    /// for the guest and the rest is kept; MASTER moves the ADC value.
    fn turn_knob(&mut self, index: usize, encoder: Option<usize>, amount: f32) {
        if amount == 0. {
            return;
        }
        match encoder {
            None => {
                // A pot: the pointer follows the turn and stops at both ends.
                let turn = amount / DETENT_PX * DETENT_ANGLE;
                let value = self.master as f32
                    + self.knob_accum[index]
                    + turn / (1.5 * std::f32::consts::PI) * 1023.;
                let value = value.clamp(0., 1023.);
                self.master = value as u16;
                self.knob_accum[index] = value - self.master as f32; // keep slow drags
                self.knob_angle[index] = master_angle(self.master);
                self.worker.master(self.master);
            }
            Some(e) => {
                self.knob_accum[index] += amount;
                // A drag sums many small angle steps: six detents can add up to 5.9999999 and
                // must still click six times on every platform's float rounding.
                let steps = self.knob_accum[index] / DETENT_PX;
                let detents = (steps + DETENT_EPS.copysign(steps)).trunc();
                if detents != 0. {
                    self.knob_accum[index] -= detents * DETENT_PX;
                    self.worker.turn(e, detents as i32);
                    self.knob_angle[index] += detents * DETENT_ANGLE;
                }
            }
        }
    }
    fn panel(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        let scale = (available.x / 1160.).min(available.y / 700.).max(0.1);
        let size = vec2(1160., 700.) * scale;
        let (area, _) = ui.allocate_exact_size(vec2(available.x, size.y), Sense::hover());
        let canvas = Canvas {
            painter: ui.painter().clone(),
            origin: pos2(
                area.center().x - size.x / 2. + 20. * scale,
                area.top() + 12. * scale,
            ),
            scale,
            theme: &ui_theme::THEMES[self.theme],
        };
        let c = &canvas;
        let theme = c.theme;
        c.box_at([8., 14., 1120., 662.], 65., theme.shadow);
        c.box_at([0., 0., 1120., 660.], 65., theme.rim);
        c.box_at([3., 3., 1114., 651.], 62., theme.edge);
        c.box_at([7., 6., 1106., 640.], 58., theme.body);
        c.painter.rect_stroke(
            c.rect([11., 10., 1098., 631.]),
            53. * scale,
            Stroke::new(scale, theme.body_line),
            StrokeKind::Inside,
        );
        for (index, (x, y, name, encoder, keys)) in KNOBS.iter().enumerate() {
            let hint = match encoder {
                Some(_) => format!("{name} · drag around or scroll to turn · keys {keys}"),
                None => format!("{name} volume · drag around or scroll · keys {keys}"),
            };
            let response = c.knob(ui, *x, *y, name, self.knob_angle[index], &hint);
            // Circling the knob turns it by the pointer's angle around its
            // centre (clockwise +); scroll up or the right-hand key: clockwise.
            let mut amount = 0.;
            let centre = c.origin + vec2(*x, *y) * c.scale;
            self.knob_centre[index] = centre;
            if let Some(now) = response
                .interact_pointer_pos()
                .filter(|_| response.dragged())
            {
                let (from, to) = (now - response.drag_delta() - centre, now - centre);
                if from.length() > 6. * c.scale && to.length() > 6. * c.scale {
                    let mut turn = to.x.atan2(-to.y) - from.x.atan2(-from.y);
                    if turn > std::f32::consts::PI {
                        turn -= std::f32::consts::TAU;
                    } else if turn < -std::f32::consts::PI {
                        turn += std::f32::consts::TAU;
                    }
                    amount += turn / DETENT_ANGLE * DETENT_PX;
                }
            }
            if response.hovered() {
                amount += ui.input(|i| i.raw_scroll_delta.y) * 0.5;
            }
            let (down, up) = KNOB_KEYS[index];
            amount += ui.input(|i| {
                (i.key_pressed(up) as i32 - i.key_pressed(down) as i32) as f32 * DETENT_PX
            });
            self.turn_knob(index, *encoder, amount);
        }
        c.box_at([290., 52., 270., 272.], 34., theme.bezel);
        c.box_at([310., 72., 230., 230.], 3., theme.glass);
        if let Some(texture) = &self.texture {
            c.painter.image(
                texture.id(),
                lcd_rect(c.rect([315., 77., 220., 220.]), ui.ctx().pixels_per_point()),
                Rect::from_min_max(pos2(0., 0.), pos2(1., 1.)),
                Color32::WHITE,
            );
        }
        c.box_at([65., 286., 181., 56.], 12., theme.oct_recess);
        for (id, label) in [(0, "OCT−"), (1, "OCT+")] {
            self.key(
                ui,
                c,
                id,
                c.rect([75. + id as f32 * 82., 296., 70., 35.]),
                label,
                false,
            );
        }
        c.box_at([598., 186., 448., 157.], 23., theme.button_recess);
        for (index, label) in [
            "FX",
            "SEL",
            "ENV",
            "LFO",
            "EDIT",
            "GLO",
            "HOME",
            "SAVE",
            "ARP",
            "SEQ",
            "PLAY\nSTOP",
            "REC",
        ]
        .iter()
        .enumerate()
        {
            self.key(
                ui,
                c,
                index + 2,
                c.rect([
                    615. + (index % 6) as f32 * 71.,
                    201. + (index / 6) as f32 * 70.,
                    57.,
                    55.,
                ]),
                label,
                false,
            );
        }
        c.box_at([40., 384., 1040., 238.], 40., theme.keybed_rim);
        c.box_at([43., 387., 1034., 232.], 38., theme.keybed);
        let mut white_index = 0;
        let mut black_index = 0;
        let labels = [
            "OP1", "OP2", "OP3", "OP4", "OP5", "OP6", "PIT", "GLO", "MONO", "POLY", "",
        ];
        for note in 0..27 {
            let black = matches!(note, 1 | 3 | 5 | 8 | 10 | 13 | 15 | 17 | 20 | 22 | 25);
            let (x, y, label) = if black {
                let x = 58. + (white_index as f32 - 0.5) * 62.5;
                let label = labels[black_index];
                black_index += 1;
                (x, 405., label)
            } else {
                let x = 58. + white_index as f32 * 62.5;
                white_index += 1;
                (x, 510., "")
            };
            self.key(ui, c, note + 14, c.rect([x, y, 49., 94.]), label, true);
        }
    }
}

struct Canvas {
    painter: egui::Painter,
    origin: egui::Pos2,
    scale: f32,
    theme: &'static ui_theme::Theme,
}
impl Canvas {
    fn rect(&self, [x, y, w, h]: [f32; 4]) -> Rect {
        Rect::from_min_size(
            self.origin + vec2(x, y) * self.scale,
            vec2(w, h) * self.scale,
        )
    }
    fn box_at(&self, rect: [f32; 4], radius: f32, color: Color32) {
        self.painter
            .rect_filled(self.rect(rect), radius * self.scale, color);
    }
    fn knob(
        &self,
        ui: &mut egui::Ui,
        x: f32,
        y: f32,
        name: &str,
        angle: f32,
        hint: &str,
    ) -> egui::Response {
        let p = self.origin + vec2(x, y) * self.scale;
        self.painter.text(
            p - vec2(0., 53.) * self.scale,
            Align2::CENTER_CENTER,
            name,
            FontId::proportional(14. * self.scale),
            self.theme.label,
        );
        self.painter.circle_filled(
            p + vec2(2., 4.) * self.scale,
            28. * self.scale,
            self.theme.knob_shadow,
        );
        self.painter
            .circle_filled(p, 26. * self.scale, self.theme.knob_rim);
        self.painter
            .circle_filled(p, 22. * self.scale, self.theme.knob_skirt);
        for i in 0..24 {
            let angle = i as f32 * std::f32::consts::TAU / 24.;
            let d = vec2(angle.sin(), angle.cos()) * self.scale;
            self.painter.line_segment(
                [p + d * 22., p + d * 25.],
                Stroke::new(self.scale, self.theme.knob_tick),
            );
        }
        self.painter
            .circle_filled(p, 19. * self.scale, self.theme.knob_cap);
        let pointer = vec2(angle.sin(), -angle.cos()) * self.scale;
        self.painter.line_segment(
            [p + pointer * 11., p + pointer * 18.],
            Stroke::new(3. * self.scale, self.theme.knob_pointer),
        );
        ui.interact(
            Rect::from_center_size(p, vec2(56., 56.) * self.scale),
            egui::Id::new(name),
            Sense::drag(),
        )
        .on_hover_text(hint)
    }
}

impl eframe::App for Emulator {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let current = self.current_settings(self.window_size);
        if let Some(Err(error)) = self.settings.as_mut().map(|saver| saver.flush(&current)) {
            eprintln!("window settings not saved: {error}");
        }
        self.worker.flusher().flush(QUIT_SAVE_WAIT);
    }
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(22, 25, 27))
                    .inner_margin(16.),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("FM-1").size(22.).strong().color(INK));
                    ui.add_space(12.);
                    let (name, hover) = match &self.path {
                        Some(path) => (
                            path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                            path.display().to_string(),
                        ),
                        None => ("no firmware".into(), "Load a firmware to start".into()),
                    };
                    ui.label(egui::RichText::new(name).color(Color32::from_gray(150)))
                        .on_hover_text(hover);
                    ui.add_space(8.);
                    self.firmware_controls(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(self.path.is_some(), egui::Button::new("Restart"))
                            .clicked()
                        {
                            self.reset();
                        }
                        self.flash_menu(ui);
                        let editor =
                            ui.add_enabled(self.web.active(), egui::Button::new("Open editor"));
                        let editor = match (self.web.url(), self.web.unavailable()) {
                            (Some(url), _) => editor
                                .on_hover_text(format!("Open {url} in the default browser")),
                            (None, reason) => {
                                editor.on_disabled_hover_text(reason.unwrap_or_default())
                            }
                        };
                        if editor.clicked() {
                            if let Err(error) = self.web.open_in_browser() {
                                eprintln!("{error}");
                            }
                        }
                        self.leds.toggle(ui);
                        egui::ComboBox::from_id_salt("theme")
                            .selected_text(ui_theme::THEMES[self.theme].name)
                            .show_ui(ui, |ui| {
                                for (index, theme) in ui_theme::THEMES.iter().enumerate() {
                                    ui.selectable_value(&mut self.theme, index, theme.name);
                                }
                            })
                            .response
                            .on_hover_text("The panel's colours (--theme NAME or FM1_THEME=NAME at start)");
                        let before = self.clock_mhz;
                        let selected = CLOCKS
                            .iter()
                            .find(|(mhz, _)| *mhz == self.clock_mhz)
                            .map_or("Custom clock".into(), |(_, label)| label.to_string());
                        egui::ComboBox::from_id_salt("clock")
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                for (mhz, label) in CLOCKS {
                                    ui.selectable_value(&mut self.clock_mhz, mhz, label);
                                }
                            })
                            .response
                            .on_hover_text("Instructions per second of guest time. Lower rates let light firmware play in real time; timers, audio and USB keep their own clocks.");
                        if self.clock_mhz != before {
                            self.worker.clock(self.clock_mhz.map(|mhz| mhz * 1_000_000));
                        }
                        if ui
                            .add_enabled(
                                self.loaded && self.fault.is_none(),
                                egui::Button::new(if self.paused { "Resume" } else { "Pause" }),
                            )
                            .clicked()
                        {
                            self.paused = !self.paused;
                        }
                        let (label, color) = if self.path.is_none() {
                            ("No firmware", Color32::from_gray(150))
                        } else if self.fault.is_some() {
                            ("Stopped", Color32::LIGHT_RED)
                        } else if self.paused {
                            ("Paused", ACCENT)
                        } else {
                            ("Running", Color32::from_rgb(122, 192, 159))
                        };
                        ui.colored_label(color, label);
                    });
                });
            });
        egui::TopBottomPanel::bottom("status").frame(egui::Frame::new().fill(Color32::from_rgb(22, 25, 27)).inner_margin(16.)).show(ctx, |ui| {
            if let Some(notice) = &self.notice {
                ui.colored_label(Color32::LIGHT_RED, notice);
            }
            if let Some(error) = &self.fault {
                ui.colored_label(Color32::LIGHT_RED, error);
                ui.label("This firmware needs additional emulation support. The LCD retains its last guest-written pixels.");
            } else {
                ui.label("Hold a key or button to press it · Arrow keys: octave · A W S E D R F G T H Y J K: notes · Knobs: drag around, scroll, or 1–0 - = [ ] (MASTER: N / M)");
                let speed = self
                    .speed
                    .map_or(String::from("no guest audio yet"), |s| {
                        format!("guest audio at {:.0}% of real time", s * 100.)
                    });
                let audio = match (&self.audio, &self.audio_error) {
                    (Some(audio), _) => format!(
                        "output {} Hz, {} dropouts",
                        audio.device_rate,
                        audio.underruns()
                    ),
                    (None, Some(error)) => format!("no audio output: {error}"),
                    (None, None) => String::from("no audio output"),
                };
                ui.weak(format!("{speed} · {audio} · below 100% the sound breaks up"));
                self.web.refresh();
                ui.horizontal(|ui| {
                    if self.web.active() {
                        let lit = if self.web.busy() {
                            ACCENT
                        } else {
                            Color32::from_gray(70)
                        };
                        ui.colored_label(lit, "●").on_hover_text("MIDI traffic");
                    }
                    ui.weak(self.web.status());
                    if let Some(state) = &self.state_status {
                        ui.weak(format!("· {state}"));
                    }
                });
            }
        });
        // File dialogs and drops are handled between frames' widgets, never
        // inside a menu callback, so the menu has closed.
        if std::mem::take(&mut self.pick_requested) {
            self.pick_firmware();
        }
        self.dropped_firmware(ctx);
        let title = window_title(self.path.as_deref());
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        // Receive worker snapshots, then send this frame's input below.
        // Losing focus immediately releases every matrix contact.
        if !ctx.input(|i| i.focused) {
            self.pressed.fill(false);
            self.pulse.fill(Instant::now());
            self.pulse_steps.fill(0);
        }
        self.refresh(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(31, 35, 37))
                    .inner_margin(10.),
            )
            .show(ctx, |ui| self.panel(ui));
        if self.path.is_none() {
            self.idle_prompt(ctx);
        }
        self.worker.input(self.pressed);
        self.keep_settings(ctx);
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
/// The command line.
#[derive(Debug, PartialEq)]
struct Args {
    /// The firmware on the command line; None: the saved one, or the picker.
    path: Option<PathBuf>,
    clock_mhz: Option<u32>,
    /// Web editor files to serve instead of FIRMWARE-ui.zip.
    ui: Option<PathBuf>,
    /// Panel colours (an index into `ui_theme::THEMES`); None: FM1_THEME or Classic.
    theme: Option<usize>,
    /// Flash state file or folder (`--state`); None: `flash_state::default_dir`.
    state: Option<PathBuf>,
    /// `--fresh`: start from the package alone and replace the flash state.
    fresh: bool,
    /// `--app-data`: keep the settings and flash state in the per-user
    /// application-data folder, as an installed app does.
    app_data: bool,
    /// `--sd IMAGE`: an SD card on the LCD's bus holding that raw image (sd.rs; FM1_SD too).
    sd: Option<PathBuf>,
}
const USAGE: &str =
    "usage: emulator [--cpu-mhz N] [--ui DIR] [--theme NAME] [--state PATH] [--fresh] [--app-data] [--sd IMAGE] [firmware]";
/// The theme called `name`, or an error listing them.
fn theme_named(name: &str) -> Result<usize, String> {
    ui_theme::find(name)
        .ok_or_else(|| format!("unknown theme {name:?}; themes: {}", ui_theme::names()))
}
/// `[--cpu-mhz N] [--ui DIR] [--theme NAME] [--state PATH] [--fresh] [--app-data] [FIRMWARE]`.
fn parse_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Args, String> {
    let mut path = None;
    let mut clock = None;
    let mut ui = None;
    let mut theme = None;
    let mut state = None;
    let mut fresh = false;
    let mut app_data = false;
    let mut sd = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--state" {
            state = Some(PathBuf::from(
                args.next().ok_or("--state needs a file or folder")?,
            ));
        } else if arg == "--fresh" {
            fresh = true;
        } else if arg == "--sd" {
            sd = Some(PathBuf::from(args.next().ok_or("--sd needs a card image")?));
        } else if arg == "--app-data" {
            app_data = true;
        } else if arg == "--ui" {
            ui = Some(PathBuf::from(args.next().ok_or("--ui needs a directory")?));
        } else if arg == "--theme" {
            let name = args.next().ok_or("--theme needs a name")?;
            theme = Some(theme_named(&name.to_string_lossy())?);
        } else if arg == "--cpu-mhz" || arg.to_str().is_some_and(|a| a.starts_with("--cpu-mhz=")) {
            // `--cpu-mhz N`, or `--cpu-mhz=N` as the fork's launcher passes it.
            let value = match arg.to_str().and_then(|a| a.strip_prefix("--cpu-mhz=")) {
                Some(value) => value.into(),
                None => args.next().ok_or("--cpu-mhz needs a value")?,
            };
            let mhz = value
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|mhz| (1..=1000).contains(mhz))
                .ok_or("--cpu-mhz takes 1..1000")?;
            clock = Some(mhz);
        } else if path.replace(PathBuf::from(arg)).is_some() {
            return Err("expected one firmware path".into());
        }
    }
    Ok(Args {
        path,
        clock_mhz: clock,
        ui,
        theme,
        state,
        fresh,
        app_data,
        sd,
    })
}
fn main() -> eframe::Result {
    ui_console::attach_parent();
    let fail = |error: String| -> ! {
        eprintln!("{error}");
        std::process::exit(2);
    };
    if std::env::args_os().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}\nWithout a firmware the window opens with a Load firmware button.");
        return Ok(());
    }
    let bare = std::env::args_os().len() == 1;
    let args = parse_args(std::env::args_os().skip(1)).unwrap_or_else(|error| fail(error));
    if let Some(image) = args.sd.clone() {
        let _ = fm1_emu::sd::IMAGE.set(image);
    }
    let theme = match (args.theme, std::env::var("FM1_THEME")) {
        (Some(theme), _) => Some(theme),
        (None, Ok(name)) => Some(theme_named(&name).unwrap_or_else(|error| fail(error))),
        (None, Err(_)) => None,
    };
    // Where the settings and the flash state go: beside the emulator or at
    // --state, except for an installed app (see ui_app).
    let exe = std::env::current_exe().unwrap_or_default();
    let launch = ui_app::Launch {
        bare,
        state_given: args.state.is_some(),
        forced: args.app_data,
    };
    let (settings_path, state) = if ui_app::is_app_mode(&exe, launch) {
        match ui_app::data_dir(ui_app::Os::HOST, &ui_app::Env::from_process()) {
            Some(data) => {
                let places = ui_app::places_in(&data);
                (places.settings, Some(places.state))
            }
            None => {
                eprintln!(
                    "no per-user data folder (HOME or APPDATA unset): using the emulator's folder"
                );
                (
                    ui_settings::path_for(None, &fm1_emu::flash_state::default_dir()),
                    None,
                )
            }
        }
    } else {
        (
            ui_settings::path_for(args.state.as_deref(), &fm1_emu::flash_state::default_dir()),
            args.state.clone(),
        )
    };
    let (settings, problems) =
        ui_settings::load(&settings_path, ui_settings::Settings::new(MASTER_DEFAULT));
    for problem in problems {
        eprintln!("window settings: {problem}");
    }
    // The command line wins; else the latest firmware, if it is still there.
    let first_firmware = args
        .path
        .clone()
        .or_else(|| settings.startup_firmware().map(PathBuf::from));
    let [width, height] = settings.window.unwrap_or([1180, 830]);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([width as f32, height as f32])
            .with_min_inner_size([800., 600.]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "FM-1 Emulator",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let mut app = Emulator::new(None);
            app.ui_dir = args.ui.clone();
            app.clock_mhz = args.clock_mhz;
            app.apply_settings(&settings, theme);
            app.window_size = settings.window;
            app.settings = Some(ui_settings::Saver::new(settings_path, settings));
            app.worker.clock(args.clock_mhz.map(|mhz| mhz * 1_000_000));
            match host_audio::HostAudio::open() {
                Ok(audio) => {
                    app.worker.audio(Some(audio.queue.clone()));
                    app.audio = Some(audio);
                }
                Err(error) => app.audio_error = Some(error),
            }
            // What the firmware writes to flash stays, as on the device;
            // Ctrl+C and SIGTERM save it before quitting, as closing does.
            app.keeps_flash = true;
            app.worker.keep_flash(worker::StateConfig {
                path: state.clone(),
                fresh: args.fresh,
            });
            let flusher = app.worker.flusher();
            if let Err(error) = ctrlc::set_handler(move || {
                flusher.flush(QUIT_SAVE_WAIT);
                std::process::exit(130);
            }) {
                eprintln!(
                    "no Ctrl+C handler ({error}): only closing the window saves the flash state"
                );
            }
            // Apply the clock, audio and editor from the first instruction.
            match first_firmware.clone() {
                Some(path) => app.load_firmware(path),
                None => app.worker.web(None),
            }
            app.worker.read_stdin();
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn demo() -> Emulator {
        Emulator::new(Some(fixture()))
    }
    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/display/firmware.elf")
    }
    fn draw(app: &mut Emulator, ctx: &egui::Context, events: Vec<egui::Event>, focused: bool) {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0., 0.), vec2(1180., 830.))),
                focused,
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.panel(ui));
            },
        );
        app.worker.input(app.pressed);
    }
    #[test]
    fn keyboard_events_reach_guest_pixels_and_focus_loss_releases_keys() {
        let mut app = demo();
        let ctx = egui::Context::default();
        draw(
            &mut app,
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::ArrowLeft,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
        );
        assert!(app.pressed[0]);
        app.paused = true;
        app.refresh(&ctx);
        let stop =
            Firmware::load(app.path.as_ref().unwrap()).unwrap().symbols["display_frame_done"];
        app.worker.inspect(move |machine| {
            let cpu = machine.cpu.as_mut().unwrap();
            cpu.run(Some(stop), cpu.steps + 200_000, None).unwrap();
            assert_eq!(cpu.bus.lcd.pixels[202 * 240 + 24], 0xf7cb00);
        });
        draw(&mut app, &ctx, vec![], false);
        assert!(app.pressed.iter().all(|&value| !value));
        app.refresh(&ctx);
        app.worker.inspect(move |machine| {
            let cpu = machine.cpu.as_mut().unwrap();
            cpu.step().unwrap(); // Leave the previous frame's stop address.
            cpu.run(Some(stop), cpu.steps + 200_000, None).unwrap();
            assert_eq!(cpu.bus.lcd.pixels[202 * 240 + 24], 0x313031);
        });
    }
    #[test]
    fn every_button_has_its_own_key_clear_of_note_and_knob_keys() {
        let mut keys: Vec<egui::Key> = (0..27).filter_map(key_binding).collect();
        assert_eq!(keys.len(), 14 + 13, "all buttons and the first 13 notes");
        keys.extend(KNOB_KEYS.iter().flat_map(|&(down, up)| [down, up]));
        let mut unique = keys.clone();
        unique.sort_by_key(|key| key.name());
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "a key holds two contacts");
        assert_eq!(key_binding(12), Some(egui::Key::Space));
        assert_eq!(key_binding(9), Some(egui::Key::Z)); // SAVE
        assert_eq!(
            button_hint(12, "PLAY\nSTOP"),
            "PLAY / STOP · hold to press · Space"
        );
        assert_eq!(button_hint(9, "SAVE"), "SAVE · hold to press · Z");
        for id in 14..=26 {
            let key = key_binding(id).expect("every note key is bound");
            let hint = note_hint(id);
            assert!(hint.ends_with(&format!("· {}", key.name())), "{hint}");
        }
        assert_eq!(note_hint(14), "Note 53 · hold to press · A");
    }
    #[test]
    fn shift_plus_a_note_key_names_every_upper_note_but_the_topmost() {
        for id in 27..=39 {
            let key = key_binding(id).expect("upper note has a combo");
            assert_eq!(Some(key), key_binding(id - 13), "same key, one octave up");
            assert!(needs_shift(id) && !needs_shift(id - 13));
            let hint = note_hint(id);
            assert!(hint.ends_with(&format!("· Shift+{}", key.name())), "{hint}");
        }
        assert_eq!(note_hint(27), "Note 66 · hold to press · Shift+A");
        assert_eq!(key_binding(40), None, "the 27th key has no computer key");
        assert_eq!(note_hint(40), "Note 79 · hold to press");
        let chars: Vec<egui::Key> = (0..40).filter_map(key_binding).collect();
        for button in 0..14 {
            let key = key_binding(button).unwrap();
            assert!(!NOTE_KEYS.contains(&key), "a button letter plays a note");
        }
        assert_eq!(chars.len(), 40, "ids 0..=39 are all bound");
    }
    fn key_event(key: egui::Key, pressed: bool, shift: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers {
                shift,
                ..Default::default()
            },
        }
    }
    fn expire_all(app: &mut Emulator) {
        app.pulse.fill(Instant::now());
        app.pulse_steps.fill(0);
    }
    #[test]
    fn shift_selects_the_octave_at_key_down_and_never_leaves_a_key_stuck() {
        let mut app = demo();
        let ctx = egui::Context::default();
        let (low, high) = (14, 27); // A and Shift+A
        draw(
            &mut app,
            &ctx,
            vec![key_event(egui::Key::A, true, false)],
            true,
        );
        assert!(app.pressed[low] && !app.pressed[high]);
        draw(
            &mut app,
            &ctx,
            vec![key_event(egui::Key::A, false, false)],
            true,
        );
        expire_all(&mut app);
        draw(&mut app, &ctx, vec![], true);
        assert!(!app.pressed[low] && !app.pressed[high]);
        draw(
            &mut app,
            &ctx,
            vec![key_event(egui::Key::A, true, true)],
            true,
        );
        assert!(app.pressed[high] && !app.pressed[low]);
        // Shift is let go first: the held key stays in the octave it started in.
        draw(&mut app, &ctx, vec![], true);
        expire_all(&mut app);
        draw(&mut app, &ctx, vec![], true);
        assert!(app.pressed[high] && !app.pressed[low]);
        draw(
            &mut app,
            &ctx,
            vec![key_event(egui::Key::A, false, false)],
            true,
        );
        expire_all(&mut app);
        draw(&mut app, &ctx, vec![], true);
        assert!(app.pressed.iter().all(|&down| !down), "nothing stuck");
        assert!(app.note_layer.iter().all(Option::is_none));
    }
    #[test]
    fn shift_with_a_button_letter_plays_no_note() {
        let mut app = demo();
        let ctx = egui::Context::default();
        draw(
            &mut app,
            &ctx,
            vec![key_event(egui::Key::Z, true, true)],
            true,
        );
        assert!(app.pressed[9], "SAVE still follows Z");
        assert!(
            app.pressed[14..].iter().all(|&down| !down),
            "no note played"
        );
    }
    #[test]
    fn a_held_button_key_holds_the_button_through_note_taps() {
        let mut app = demo();
        let ctx = egui::Context::default();
        let key = |key, pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let (save, note) = (9, 21); // SAVE and the G key's note
        let expire = |app: &mut Emulator| {
            for id in [save, note] {
                app.pulse[id] = Instant::now();
                app.pulse_steps[id] = 0;
            }
        };
        draw(&mut app, &ctx, vec![key(egui::Key::Z, true)], true);
        assert!(app.pressed[save]);
        for _ in 0..2 {
            // Tap the note twice (a "press again" confirmation).
            draw(&mut app, &ctx, vec![key(egui::Key::G, true)], true);
            assert!(app.pressed[save] && app.pressed[note]);
            draw(&mut app, &ctx, vec![key(egui::Key::G, false)], true);
            expire(&mut app);
            draw(&mut app, &ctx, vec![], true);
            assert!(app.pressed[save], "SAVE must stay held while Z is down");
            assert!(!app.pressed[note], "the note must be let go between taps");
        }
        draw(&mut app, &ctx, vec![key(egui::Key::Z, false)], true);
        expire(&mut app);
        draw(&mut app, &ctx, vec![], true);
        assert!(!app.pressed[save], "releasing Z releases SAVE");
    }
    #[test]
    fn pause_and_restart_control_the_actual_cpu() {
        let mut app = demo();
        let ctx = egui::Context::default();
        app.worker.inspect(|_| ()); // Wait for loading without advancing on the UI.
        std::thread::sleep(Duration::from_millis(20));
        let steps = app
            .worker
            .inspect(|machine| machine.cpu.as_ref().unwrap().steps);
        assert!(
            steps > 0,
            "guest execution must continue without drawing frames"
        );
        app.paused = true;
        app.refresh(&ctx);
        let paused = app.worker.inspect(|machine| {
            assert!(machine.paused);
            machine.cpu.as_ref().unwrap().steps
        });
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            app.worker
                .inspect(|machine| machine.cpu.as_ref().unwrap().steps),
            paused
        );
        app.reset();
        assert_eq!(app.steps, 0);
        assert!(!app.paused);
        assert!(!app.loaded);
        assert!(app.texture.is_none());
        app.worker.inspect(|machine| {
            assert!(!machine.paused);
            assert!(machine.fault.is_none());
            assert!(machine.cpu.is_some());
        });
    }
    #[test]
    fn a_short_keypress_survives_a_slow_host_until_guest_debounce_can_run() {
        let mut app = demo();
        let ctx = egui::Context::default();
        let key = |pressed| egui::Event::Key {
            key: egui::Key::ArrowLeft,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        draw(&mut app, &ctx, vec![key(true)], true);
        app.pulse[0] = Instant::now(); // Wall-clock pulse expired on a slow host.
        draw(&mut app, &ctx, vec![key(false)], true);
        assert!(app.pressed[0]);
        app.worker
            .inspect(|machine| machine.cpu.as_mut().unwrap().steps = 72_000_000);
        app.refresh(&ctx);
        draw(&mut app, &ctx, vec![], true);
        assert!(!app.pressed[0]);
    }
    #[test]
    fn unsupported_firmware_stops_without_fabricating_a_screen() {
        let mut app = demo();
        app.worker.inspect(|machine| {
            machine.cpu = Some(Cpu::new(Bus::new(vec![0xff, 0x00]).unwrap(), 0x02000120));
            machine.fault = None;
        });
        // Wait for execution rather than relying on host scheduling speed.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !app.worker.inspect(|machine| machine.fault.is_some()) {
            assert!(Instant::now() < deadline, "worker did not report the fault");
            std::thread::sleep(Duration::from_millis(1));
        }
        let ctx = egui::Context::default();
        app.refresh(&ctx);
        assert!(app
            .fault
            .as_ref()
            .unwrap()
            .contains("unsupported instruction"));
        let steps = app.worker.inspect(|machine| {
            let cpu = machine.cpu.as_ref().unwrap();
            assert!(cpu.bus.lcd.pixels.iter().all(|&pixel| pixel == 0));
            cpu.steps
        });
        app.refresh(&ctx);
        assert_eq!(
            app.worker
                .inspect(|machine| machine.cpu.as_ref().unwrap().steps),
            steps
        );
    }
    #[test]
    fn a_drag_summing_just_under_whole_detents_still_clicks_them() {
        // Six detents' worth of small steps can sum to 5.9999999 detents in f32 (it did on
        // Linux and Windows CI): they must give six clicks, not five.
        let mut app = demo();
        app.paused = true;
        let step = 6. * DETENT_PX / 18.;
        for _ in 0..18 {
            app.turn_knob(4, Some(0), step * (1. - 1e-6));
        }
        assert!(
            (app.knob_angle[4] - 6. * DETENT_ANGLE).abs() < 1e-4,
            "{}",
            app.knob_angle[4]
        );
        assert!(app.knob_accum[4].abs() < 1e-3, "{}", app.knob_accum[4]);
    }

    #[test]
    fn circling_a_knob_clicks_its_encoder_and_master_stops_at_its_ends() {
        let mut app = demo();
        let ctx = egui::Context::default();
        // Paused, the guest cannot play the queued clicks out before the check.
        app.paused = true;
        app.refresh(&ctx);
        draw(&mut app, &ctx, vec![], true);
        let drag = |app: &mut Emulator, index: usize, degrees: i32| {
            let centre = app.knob_centre[index];
            let at = |d: i32| {
                let a = (d as f32).to_radians();
                centre + vec2(a.sin(), -a.cos()) * 22.
            };
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            draw(app, &ctx, vec![egui::Event::PointerMoved(at(0))], true);
            draw(app, &ctx, vec![button(at(0), true)], true);
            let step = degrees.signum() * 5;
            for d in (step..=degrees)
                .step_by(5)
                .chain((degrees..=step).rev().step_by(5))
            {
                draw(app, &ctx, vec![egui::Event::PointerMoved(at(d))], true);
            }
            draw(app, &ctx, vec![button(at(degrees), false)], true);
        };
        // A quarter turn clockwise around KNOB1 is six 15-degree clicks.
        drag(&mut app, 4, 90);
        assert!(
            (app.knob_angle[4] - 6. * DETENT_ANGLE).abs() < 1e-4,
            "{}",
            app.knob_angle[4]
        );
        // Endless: two full turns back keep counting.
        drag(&mut app, 4, -720);
        assert!(
            (app.knob_angle[4] + 42. * DETENT_ANGLE).abs() < 1e-4,
            "{}",
            app.knob_angle[4]
        );
        let pending = app.worker.inspect(|machine| machine.encoders.clone());
        assert!(pending.busy(), "the worker received the clicks");
        // MASTER is a pot: a big clockwise turn pins it at full scale.
        drag(&mut app, 0, 300);
        assert_eq!(app.master, 1023);
        let master = app
            .worker
            .inspect(|machine| machine.cpu.as_ref().unwrap().bus.devices.adc.master);
        assert_eq!(master, 1023);
    }
    #[test]
    fn the_command_line_takes_a_firmware_and_an_instruction_clock() {
        let parse = |args: &[&str]| parse_args(args.iter().map(std::ffi::OsString::from));
        let args = |clock_mhz, ui: Option<&str>| Args {
            path: Some(PathBuf::from("a.fwsc")),
            clock_mhz,
            ui: ui.map(PathBuf::from),
            theme: None,
            state: None,
            fresh: false,
            app_data: false,
            sd: None,
        };
        assert_eq!(parse(&["a.fwsc"]), Ok(args(None, None)));
        assert_eq!(
            parse(&["--sd", "card.img", "a.fwsc"]).unwrap().sd,
            Some(PathBuf::from("card.img"))
        );
        assert!(parse(&["a.fwsc", "--sd"]).is_err());
        assert_eq!(
            parse(&["--cpu-mhz", "24", "a.fwsc"]),
            Ok(args(Some(24), None))
        );
        assert_eq!(
            parse(&["a.fwsc", "--ui", "web"]),
            Ok(args(None, Some("web")))
        );
        assert!(parse(&["a.fwsc", "--ui"]).is_err());
        assert!(parse(&["--cpu-mhz", "0", "a.fwsc"]).is_err());
        assert_eq!(parse(&["--cpu-mhz=96", "a.fwsc"]), Ok(args(Some(96), None)));
        assert!(parse(&["a.fwsc", "b.fwsc"]).is_err());
        // No firmware is fine: the window opens with the picker.
        assert_eq!(parse(&[]).unwrap().path, None);
        assert!(parse(&["--app-data"]).unwrap().app_data);
        let state = parse(&["--state", "st/", "--fresh", "a.fwsc"]).unwrap();
        assert_eq!(
            (state.state, state.fresh),
            (Some(PathBuf::from("st/")), true)
        );
        assert!(parse(&["a.fwsc", "--state"]).is_err());
    }
    #[test]
    fn the_command_line_picks_a_theme_by_name() {
        let parse = |args: &[&str]| parse_args(args.iter().map(std::ffi::OsString::from));
        let theme = |args: &[&str]| parse(args).map(|args| args.theme);
        assert_eq!(theme(&["a.fwsc"]), Ok(None));
        assert_eq!(
            theme(&["--theme", "mint", "a.fwsc"]),
            Ok(ui_theme::find("Mint"))
        );
        assert!(ui_theme::find("Mint").is_some());
        let unknown = parse(&["--theme", "plaid", "a.fwsc"]).unwrap_err();
        assert!(unknown.contains("Classic, Black"), "{unknown}");
        assert!(parse(&["a.fwsc", "--theme"]).is_err());
    }
    #[test]
    fn remembered_settings_reach_the_panel_and_a_one_run_theme_is_not_saved() {
        let mut app = demo();
        assert!(
            app.settings.is_none(),
            "tests never touch the settings file"
        );
        let mint = ui_theme::find("Mint").unwrap();
        let saved = ui_settings::Settings {
            master: 300,
            theme: Some("Mint".into()),
            leds: false,
            window: Some([1200, 800]),
            recent: vec![PathBuf::from("/fw/a.fwsc"), PathBuf::from("/fw/b.fwsc")],
            reopen_last: false,
        };
        app.apply_settings(&saved, None);
        assert_eq!(
            (app.master, app.theme, app.leds.enabled),
            (300, mint, false)
        );
        assert!((app.knob_angle[0] - master_angle(300)).abs() < 1e-6);
        let adc = app
            .worker
            .inspect(|machine| machine.cpu.as_ref().unwrap().bus.devices.adc.master);
        assert_eq!(adc, 300, "the guest reads the remembered MASTER");
        assert_eq!(app.current_settings(saved.window), saved);
        // --theme Black for this run: the file keeps Mint until the toolbar
        // picks a theme.
        let black = ui_theme::find("Black").unwrap();
        app.apply_settings(&saved, Some(black));
        assert_eq!(app.theme, black);
        assert_eq!(app.current_settings(None).theme.as_deref(), Some("Mint"));
        app.theme = ui_theme::find("Blue").unwrap();
        assert_eq!(app.current_settings(None).theme.as_deref(), Some("Blue"));
        // An unknown saved theme falls back to Classic.
        let unknown = ui_settings::Settings {
            theme: Some("Plaid".into()),
            ..ui_settings::Settings::new(MASTER_DEFAULT)
        };
        app.apply_settings(&unknown, None);
        assert_eq!(app.theme, 0);
    }
    #[test]
    fn the_worker_paces_the_guest_by_the_audio_queue() {
        let mut app = demo();
        let queue = host_audio::AudioQueue::default();
        app.worker.audio(Some(queue.clone()));
        app.reset();
        app.worker.inspect(|machine| {
            let cpu = machine.cpu.as_mut().unwrap();
            cpu.bus.audio.frames = 1; // The guest has started streaming.
        });
        queue.push(std::iter::repeat_n([0, 0], host_audio::TARGET_FRAMES));
        let steps = app
            .worker
            .inspect(|machine| machine.cpu.as_ref().unwrap().steps);
        std::thread::sleep(Duration::from_millis(30));
        let paced = app
            .worker
            .inspect(|machine| machine.cpu.as_ref().unwrap().steps);
        assert_eq!(paced, steps, "a full queue holds the guest");
        queue.clear();
        std::thread::sleep(Duration::from_millis(30));
        let resumed = app
            .worker
            .inspect(|machine| machine.cpu.as_ref().unwrap().steps);
        assert!(resumed > paced, "an emptied queue lets it run");
    }
    #[test]
    fn the_lcd_is_never_downscaled_and_sits_on_the_pixel_grid() {
        let area = |side: f32| Rect::from_min_size(pos2(10.3, 20.7), vec2(side, side));
        // 1x display, 220-point box: drawn at 240 pixels, not squeezed to 220.
        assert_eq!(lcd_rect(area(220.), 1.).width(), 240.);
        // Retina, 220 points = 440 px: 1x would fill only 55%, so use all 440.
        assert_eq!(lcd_rect(area(220.), 2.).width(), 220.);
        // Retina, 250 points = 500 px: 2x (480 px) fills 96%: integer scale.
        assert_eq!(lcd_rect(area(250.), 2.).width(), 240.);
        for ppp in [1., 1.5, 2.] {
            let r = lcd_rect(area(233.), ppp);
            assert!(r.width() * ppp >= 240.);
            assert_eq!((r.min.x * ppp).fract(), 0.);
            assert_eq!((r.min.y * ppp).fract(), 0.);
        }
    }
    #[test]
    #[ignore = "requires FM1_STOCK_FWSC; measures GUI worker latency in release mode"]
    fn stock_click_reaches_a_new_guest_frame_through_the_gui_worker() {
        let path = std::env::var_os("FM1_STOCK_FWSC").expect("set FM1_STOCK_FWSC");
        let mut app = Emulator::new(Some(PathBuf::from(path)));
        let ctx = egui::Context::default();
        let deadline = Instant::now() + Duration::from_secs(240);
        while app.steps < 3_000_000_000 {
            app.refresh(&ctx);
            assert!(app.fault.is_none(), "{:?}", app.fault);
            assert!(
                Instant::now() < deadline,
                "stock boot exceeded the host-time budget"
            );
            std::thread::sleep(Duration::from_millis(16));
        }
        app.paused = true;
        app.refresh(&ctx);
        let before = app.worker.inspect(|machine| {
            let cpu = machine.cpu.as_ref().unwrap();
            assert!(cpu.bus.screen_visible());
            cpu.bus.lcd.pixels.clone()
        });
        app.paused = false;
        app.pressed[2] = true; // FX, sent through the GUI's matrix mapping.
        let release = app.steps + 72_000_000;
        let start = Instant::now();
        app.refresh(&ctx);
        loop {
            std::thread::sleep(Duration::from_millis(16));
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "FX redraw exceeded ten seconds"
            );
            let Some(snapshot) = app.worker.snapshot() else {
                continue;
            };
            assert!(snapshot.fault.is_none(), "{:?}", snapshot.fault);
            app.steps = snapshot.steps;
            app.pressed[2] = app.steps < release;
            app.worker.input(app.pressed);
            if snapshot.pixels.is_some_and(|pixels| pixels != before) {
                eprintln!(
                    "Stock FX click to guest LCD snapshot: {:.3} seconds",
                    start.elapsed().as_secs_f64()
                );
                break;
            }
        }
    }
    #[test]
    fn without_a_firmware_the_panel_idles_and_offers_to_load_one() {
        let mut app = Emulator::new(None);
        assert!(app.path.is_none() && !app.loaded);
        let ctx = egui::Context::default();
        let frame = |app: &mut Emulator| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0., 0.), vec2(1180., 830.))),
                    focused: true,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.panel(ui));
                    if app.path.is_none() {
                        app.idle_prompt(ctx);
                    }
                },
            );
        };
        for _ in 0..3 {
            frame(&mut app);
        }
        let prompt = egui::Id::new("open_firmware_prompt");
        assert!(
            ctx.memory(|memory| memory.area_rect(prompt)).is_some(),
            "the open-firmware prompt is drawn"
        );
        // Restart and the worker are harmless with nothing to run.
        app.reset();
        app.refresh(&ctx);
        assert!(!app.loaded && app.fault.is_none() && app.texture.is_none());
        assert_eq!(window_title(None), "FM-1 Emulator");
        // A non-firmware file is refused with a message, not loaded.
        app.open_firmware(PathBuf::from("/tmp/notes.txt"));
        assert!(
            app.path.is_none()
                && app
                    .notice
                    .as_deref()
                    .is_some_and(|n| n.contains("notes.txt"))
        );
    }
    #[test]
    fn firmware_files_are_recognised_by_extension() {
        for name in ["a.fwsc", "b.ELF", "c.bin", "/x y/z.Fwsc"] {
            assert!(is_firmware_file(std::path::Path::new(name)), "{name}");
        }
        for name in ["a.txt", "fwsc", "a.fwsc.zip", ""] {
            assert!(!is_firmware_file(std::path::Path::new(name)), "{name}");
        }
        assert_eq!(
            window_title(Some(std::path::Path::new("/fw/optimist-0.2.fwsc"))),
            "optimist-0.2.fwsc - FM-1 Emulator"
        );
    }
    fn nor_transaction(bus: &mut Bus, bytes: &[u8]) {
        bus.write(0x500c0, 0, 4).unwrap();
        for &byte in bytes {
            bus.write(0x11c08, u32::from(byte), 4).unwrap();
        }
        bus.write(0x500c0, 1, 4).unwrap();
    }
    /// The first byte of the flash at `offset`, once the worker has caught up.
    fn flash_byte(app: &Emulator, offset: usize) -> u8 {
        app.worker
            .inspect(move |machine| machine.cpu.as_ref().unwrap().bus.nor_bytes()[offset])
    }
    #[test]
    fn loading_a_firmware_reboots_into_it_and_keeps_each_ones_flash_state() {
        let dir = std::env::temp_dir().join(format!("fm1-ui-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Two firmware families from the one fixture.
        let (alpha, beta) = (dir.join("alpha-1.0.elf"), dir.join("beta-1.0.elf"));
        std::fs::copy(fixture(), &alpha).unwrap();
        std::fs::copy(fixture(), &beta).unwrap();
        let state = dir.join("state").join("");
        let mut app = Emulator::new(None);
        app.keeps_flash = true;
        app.worker.keep_flash(worker::StateConfig {
            path: Some(state.clone()),
            fresh: false,
        });
        let ctx = egui::Context::default();
        let wait_for = |app: &mut Emulator, what: &str, done: &dyn Fn(&Emulator) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !done(app) {
                app.refresh(&ctx);
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        // Load A: it boots, and the guest programs a flash byte.
        app.load_firmware(alpha.clone());
        assert_eq!(app.path.as_deref(), Some(alpha.as_path()));
        assert_eq!(
            window_title(app.path.as_deref()),
            "alpha-1.0.elf - FM-1 Emulator"
        );
        wait_for(&mut app, "A to boot", &|app| app.loaded);
        let gen_a = app.generation;
        app.worker.inspect(|machine| {
            let bus = &mut machine.cpu.as_mut().unwrap().bus;
            nor_transaction(bus, &[0x06]);
            nor_transaction(bus, &[0x02, 0x0f, 0x00, 0x00, 0xa5]);
        });
        // The guest's program finishes after a few emulated milliseconds.
        let written = |app: &Emulator| {
            app.worker
                .inspect(|m| m.cpu.as_ref().unwrap().bus.nor_writes())
                > 0
        };
        wait_for(&mut app, "A's flash write", &written);
        assert_eq!(flash_byte(&app, 0x0f0000), 0xa5);
        // Load B: a power cycle into the other firmware (a new generation).
        app.load_firmware(beta.clone());
        assert!(app.generation > gen_a, "the machine was rebuilt");
        assert!(!app.loaded && app.fault.is_none());
        wait_for(&mut app, "B to boot", &|app| app.loaded);
        assert_eq!(app.path.as_deref(), Some(beta.as_path()));
        assert_eq!(
            flash_byte(&app, 0x0f0000),
            0xff,
            "B has its own, erased flash"
        );
        // A's flash state was saved on the way out; B has none yet.
        assert!(state.join("alpha.nor").is_file(), "A's state is saved");
        assert!(!state.join("beta.nor").exists());
        assert_eq!(app.recent, [beta.clone(), alpha.clone()]);
        // Back to A: its flash comes back.
        app.load_firmware(alpha.clone());
        wait_for(&mut app, "A to reboot", &|app| app.loaded);
        assert_eq!(
            flash_byte(&app, 0x0f0000),
            0xa5,
            "A's flash state is restored"
        );
        assert_eq!(app.recent, [alpha, beta]);
        drop(app);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
