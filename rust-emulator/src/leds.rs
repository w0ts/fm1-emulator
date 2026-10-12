// SPDX-License-Identifier: GPL-3.0-only
// The panel LEDs, seen from the pins that drive them (fm1_input.h wiring):
// four LED lines (PA9, PA10, PH6, PH9) light the LED of matrix row PA5, PA6,
// PA7 or PA8 on each column the 74HC595 chain holds low. An LED is lit while
// its line is driven high and its column is latched; its brightness is the
// device time it was lit over the time its column was selected, so full on
// is 1 whatever the firmware's scan rate. Only pin states and device time
// are observed: guest execution never sees this.

/// Matrix columns (74HC595 outputs 0..10).
pub const COLUMNS: usize = 11;
/// LED lines, by the matrix row bit they light (1..=4: PA5..PA8).
pub const LINES: usize = 4;
/// (GPIO port, pin) of the LED line for row bit 1 + index.
pub const LINE_PINS: [(usize, u32); LINES] = [(0, 9), (0, 10), (7, 6), (7, 9)];
/// Key id at (row bit 1 + line, column), -1 = none (fm1_input.h FM1_KEYMAP).
pub const KEYMAP: [[i8; COLUMNS]; LINES] = [
    [5, 11, 4, 10, 3, 9, 2, 8, -1, -1, -1],
    [34, 35, 36, 37, 38, 40, 39, 13, 7, 6, 12],
    [23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33],
    [0, 1, 15, 14, 17, 16, 19, 18, 20, 21, 22],
];
/// Buttons 0..13 and note keys 14..40.
pub const KEYS: usize = 41;

/// Brightness per LED, `[column][line]`, 0 (dark) ..= 1 (lit whenever its
/// column was selected).
pub type Brightness = [[f32; LINES]; COLUMNS];

#[derive(Clone, Debug, Default)]
pub struct Leds {
    /// Device time (oscillator ticks), the last pin change, the window's start.
    now: u64,
    since: u64,
    start: u64,
    /// Lit now: bit `column * LINES + line`.
    lit: u64,
    /// Selected now: bit per column.
    selected: u16,
    on: [[u64; LINES]; COLUMNS],
    selected_for: [u64; COLUMNS],
}

impl Leds {
    /// Device time moves on.
    #[inline]
    pub(crate) fn advance(&mut self, ticks: u32) {
        self.now += ticks as u64;
    }

    /// New pin states: `latched` is the 595 chain's output word (low =
    /// column selected), `lines` the LED lines driven high.
    #[inline]
    pub(crate) fn update(&mut self, latched: u16, lines: u8) {
        let selected = !latched & ((1 << COLUMNS) - 1);
        let mut lit = 0u64;
        if lines != 0 {
            let mut columns = selected;
            while columns != 0 {
                let column = columns.trailing_zeros() as usize;
                columns &= columns - 1;
                lit |= (lines as u64 & 0xf) << (column * LINES);
            }
        }
        if lit == self.lit && selected == self.selected {
            return;
        }
        self.accumulate();
        self.lit = lit;
        self.selected = selected;
    }

    fn accumulate(&mut self) {
        let span = self.now - self.since;
        self.since = self.now;
        if span == 0 {
            return;
        }
        let mut lit = self.lit;
        while lit != 0 {
            let bit = lit.trailing_zeros() as usize;
            lit &= lit - 1;
            self.on[bit / LINES][bit % LINES] += span;
        }
        let mut columns = self.selected;
        while columns != 0 {
            let column = columns.trailing_zeros() as usize;
            columns &= columns - 1;
            self.selected_for[column] += span;
        }
    }

    /// Device time (oscillator ticks) since the last `take`.
    pub fn window(&self) -> u64 {
        self.now - self.start
    }

    /// The lit time per LED and the selected time per column (oscillator
    /// ticks) since the last call, and start a new window.
    pub fn take_sums(&mut self) -> Sums {
        self.accumulate();
        let sums = Sums {
            on: self.on,
            selected: self.selected_for,
        };
        self.on = Default::default();
        self.selected_for = Default::default();
        self.start = self.now;
        sums
    }

    /// Brightness since the last call (or since reset), and start a new window.
    pub fn take(&mut self) -> Brightness {
        self.take_sums().brightness()
    }
}

/// One window's raw sums: lit ticks per `[column][line]`, selected ticks per column.
#[derive(Clone, Debug, Default)]
pub struct Sums {
    on: [[u64; LINES]; COLUMNS],
    selected: [u64; COLUMNS],
}

impl Sums {
    /// Lit time over the column's selected time, per LED.
    pub fn brightness(&self) -> Brightness {
        let mut brightness = [[0.; LINES]; COLUMNS];
        for (column, levels) in brightness.iter_mut().enumerate() {
            let selected = self.selected[column];
            if selected != 0 {
                for (line, level) in levels.iter_mut().enumerate() {
                    *level = (self.on[column][line] as f64 / selected as f64) as f32;
                }
            }
        }
        brightness
    }
}

/// Time constant of the level filter (oscillator ticks, 80 ms).
pub const FILTER_TICKS: f64 = 1_920_000.;
/// A window level this many times above or below the filtered one is a real
/// change (a blink, a button lit or let go), not scan jitter: the filter
/// follows it at once.
const STEP_RATIO: f64 = 3.;

/// A steady brightness out of the pin states. A software-dimmed LED (lit one
/// scan frame in four, or a short pulse each frame) lights a different whole
/// number of frames in each short window, and an interrupt that stretches one
/// frame changes it more: the raw window level wobbles by a third, which
/// shows as flicker. The eye (and the LED's own response) averages; this
/// does the same, an exponential average of the lit and selected time over
/// `FILTER_TICKS`, per LED. A change by more than `STEP_RATIO` (or to and
/// from dark) skips the average, so on/off and blinking stay crisp.
#[derive(Clone, Debug, Default)]
pub struct LevelFilter {
    on: [[f64; LINES]; COLUMNS],
    selected: [[f64; LINES]; COLUMNS],
}

impl LevelFilter {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Add a window of `ticks` device time; the filtered brightness.
    pub fn push(&mut self, sums: &Sums, ticks: u64) -> Brightness {
        let keep = (-(ticks as f64) / FILTER_TICKS).exp();
        for column in 0..COLUMNS {
            let selected = sums.selected[column] as f64;
            if selected == 0. {
                continue;
            }
            for line in 0..LINES {
                let on = sums.on[column][line] as f64;
                let (f_on, f_sel) = (&mut self.on[column][line], &mut self.selected[column][line]);
                let (raw, old) = (on / selected, if *f_sel > 0. { *f_on / *f_sel } else { 0. });
                let step = (raw == 0.) != (old == 0.)
                    || (raw > 0. && (raw > old * STEP_RATIO || raw * STEP_RATIO < old));
                let keep = if step { 0. } else { keep };
                *f_on = *f_on * keep + on;
                *f_sel = *f_sel * keep + selected;
            }
        }
        self.brightness()
    }

    pub fn brightness(&self) -> Brightness {
        let mut brightness = [[0.; LINES]; COLUMNS];
        for column in 0..COLUMNS {
            for line in 0..LINES {
                let selected = self.selected[column][line];
                if selected > 0. {
                    brightness[column][line] = (self.on[column][line] / selected).min(1.) as f32;
                }
            }
        }
        brightness
    }
}

/// Brightness per key id (buttons 0..13, note keys 14..40).
pub fn by_key(brightness: &Brightness) -> [f32; KEYS] {
    let mut keys = [0.; KEYS];
    for (line, ids) in KEYMAP.iter().enumerate() {
        for (column, &id) in ids.iter().enumerate() {
            if id >= 0 {
                keys[id as usize] = brightness[column][line];
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLUMN_TICKS: u32 = 240; // 10 us per column at 24 MHz.

    /// One scan frame: each column selected for COLUMN_TICKS, its lines lit
    /// for `lit_ticks` of that.
    fn frame(leds: &mut Leds, lines: impl Fn(usize) -> u8, lit_ticks: u32) {
        for column in 0..COLUMNS {
            leds.update(!(1 << column), 0);
            leds.advance(COLUMN_TICKS - lit_ticks);
            leds.update(!(1 << column), lines(column));
            leds.advance(lit_ticks);
        }
        leds.update(u16::MAX, 0);
    }

    #[test]
    fn brightness_is_on_time_over_the_column_time() {
        let mut leds = Leds::default();
        // Column 3 line 2 full on, column 5 line 0 lit one frame in four.
        for n in 0..8 {
            frame(
                &mut leds,
                |c| match c {
                    3 => 4,
                    5 if n % 4 == 0 => 1,
                    _ => 0,
                },
                COLUMN_TICKS,
            );
        }
        let b = leds.take();
        assert_eq!(b[3][2], 1.);
        assert_eq!(b[5][0], 0.25);
        assert_eq!(b[0][0], 0.);
        let keys = by_key(&b);
        assert_eq!(keys[KEYMAP[2][3] as usize], 1.);
        assert_eq!(keys[KEYMAP[0][5] as usize], 0.25);
        // A new window starts dark.
        frame(&mut leds, |_| 0, COLUMN_TICKS);
        assert_eq!(leds.take(), [[0.; LINES]; COLUMNS]);
    }

    #[test]
    fn lines_off_while_the_rows_are_read_dim_the_full_level() {
        let mut leds = Leds::default();
        for _ in 0..4 {
            frame(&mut leds, |_| 0xf, COLUMN_TICKS * 3 / 4);
        }
        assert!(leds.take().iter().flatten().all(|&level| level == 0.75));
    }

    #[test]
    fn an_unscanned_column_or_an_undriven_line_stays_dark() {
        let mut leds = Leds::default();
        leds.update(u16::MAX, 0xf);
        leds.advance(100);
        leds.update(!1, 0);
        leds.advance(900);
        assert_eq!(leds.take(), [[0.; LINES]; COLUMNS]);
        leds.advance(500);
        assert_eq!(leds.window(), 500);
    }

    /// A panel scanned like the firmware at ~900 Hz (columns of 100 us), one
    /// column's LED per `lit(frame)`, frames stretched now and then as an
    /// audio interrupt does; fed to the filter in 20 ms windows.
    struct Rig {
        leds: Leds,
        filter: LevelFilter,
        frame: u64,
        window: u64,
        raw: Vec<f32>,
        level: Vec<f32>,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                leds: Leds::default(),
                filter: LevelFilter::default(),
                frame: 0,
                window: 0,
                raw: vec![],
                level: vec![],
            }
        }

        /// Run `ms` of device time; `duty(frame)` is the lit part of column 5
        /// line 0's selection, 0..=1.
        fn run(&mut self, ms: u64, duty: impl Fn(u64) -> f32) {
            let until = self.leds.now + ms * 24_000;
            while self.leds.now < until {
                // Every 7th frame an interrupt makes one column 5 times longer.
                let stretch = if self.frame % 7 == 3 { 5 } else { 1 };
                for column in 0..COLUMNS {
                    let ticks = 2400 * if column == 5 { stretch } else { 1 };
                    let lit = (ticks as f32 * duty(self.frame)) as u32;
                    self.leds.update(!(1 << column), 0);
                    self.leds.advance(ticks as u32 - lit);
                    self.leds.update(!(1 << column), if column == 5 { 1 } else { 0 });
                    self.leds.advance(lit);
                }
                self.leds.update(u16::MAX, 0);
                self.frame += 1;
                if self.leds.window() >= 480_000 {
                    let ticks = self.leds.window();
                    let sums = self.leds.take_sums();
                    self.raw.push(sums.brightness()[5][0]);
                    self.level.push(self.filter.push(&sums, ticks)[5][0]);
                }
            }
            self.window = self.level.len() as u64;
        }
    }

    fn spread(levels: &[f32]) -> f32 {
        let high = levels.iter().cloned().fold(0., f32::max);
        let low = levels.iter().cloned().fold(1., f32::min);
        high - low
    }

    #[test]
    fn a_dimmed_led_reads_steady_where_the_window_level_wobbles() {
        // Lit every other frame at full column time: 50 % duty.
        let mut rig = Rig::new();
        rig.run(1000, |frame| if frame % 2 == 0 { 1. } else { 0. });
        let (raw, level) = (&rig.raw[10..], &rig.level[10..]);
        assert!(level.iter().all(|l| (l - 0.5).abs() < 0.04), "{level:?}");
        assert!(spread(raw) > 2. * spread(level), "{raw:?} vs {level:?}");
    }

    #[test]
    fn a_frame_in_four_reads_a_quarter_and_full_on_reads_one() {
        let mut rig = Rig::new();
        rig.run(1000, |frame| if frame % 4 == 0 { 1. } else { 0. });
        assert!(rig.level[10..].iter().all(|l| (l - 0.25).abs() < 0.04), "{:?}", rig.level);
        let mut rig = Rig::new();
        rig.run(500, |_| 1.);
        assert!(rig.level.iter().all(|&l| l == 1.), "{:?}", rig.level);
    }

    #[test]
    fn a_slow_blink_still_blinks_and_on_off_is_crisp() {
        let mut rig = Rig::new();
        // 4 Hz: 125 ms lit, 125 ms dark, lit every frame while on; then a dim LED.
        for _ in 0..4 {
            rig.run(125, |_| 1.);
            rig.run(125, |_| 0.);
        }
        let high = rig.level.iter().filter(|&&l| l > 0.95).count();
        let dark = rig.level.iter().filter(|&&l| l == 0.).count();
        assert!(high >= 4 * 5 && dark >= 4 * 5, "{:?}", rig.level);
        // Every edge is followed at once: the first window of a lit stretch is lit.
        let mut rig = Rig::new();
        rig.run(200, |frame| if frame % 4 == 0 { 1. } else { 0. });
        rig.run(100, |_| 0.);
        assert_eq!(*rig.level.last().unwrap(), 0.);
        rig.run(100, |_| 1.);
        assert!(*rig.level.last().unwrap() > 0.95);
        // (Full to dim is followed by the average: settled to a quarter within half a second.)
        rig.run(500, |frame| if frame % 4 == 0 { 1. } else { 0. });
        assert!((*rig.level.last().unwrap() - 0.25).abs() < 0.04, "{:?}", rig.level);
    }
}
