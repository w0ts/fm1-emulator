// SPDX-License-Identifier: GPL-3.0-only
// The panel LEDs in the window: each key and button glows by the brightness
// the guest's LED pins gave it (fm1_emu::leds), measured over guest time.
use eframe::egui::{self, Color32, Rect, Stroke, StrokeKind};
use fm1_emu::leds::KEYS;

/// Guest time a brightness sample spans at least (oscillator ticks, 20 ms).
/// The samples go through `fm1_emu::leds::LevelFilter` (80 ms average, steps
/// followed at once): a few whole dim cycles do not add up in so short a
/// window, and the filter averages the rest, so a dimmed LED reads steady
/// while blinking (a few Hz) still shows.
pub(super) const WINDOW_TICKS: u64 = 480_000;
/// Below this duty an LED reads as dark.
const DARK: f32 = 0.001;
/// LED light (a soft white).
const LIGHT: (u8, u8, u8) = (255, 244, 222);
/// How opaque the light through a fully lit face is, and the label ink there.
const TINT: f32 = 0.55;
const INK_LIT: Color32 = Color32::from_gray(28);

pub(super) struct LedView {
    pub enabled: bool,
    level: [f32; KEYS],
}

impl LedView {
    pub fn new() -> Self {
        Self {
            enabled: true,
            level: [0.; KEYS],
        }
    }
    pub fn reset(&mut self) {
        self.level = [0.; KEYS];
    }
    /// A new sample: brightness per key id (1 = lit whenever scanned).
    pub fn receive(&mut self, level: [f32; KEYS]) {
        self.level = level;
    }
    /// How strongly key `id` glows, 0..=1. The eye sees duty roughly on a
    /// log scale: three decades of duty map onto the glow, so a 2 % backlight
    /// shows, and a dim LED (1/4 duty) stays visibly below a full one.
    pub fn glow(&self, id: usize) -> f32 {
        let duty = self.level.get(id).copied().unwrap_or(0.);
        if !self.enabled || duty < DARK {
            return 0.;
        }
        (1. + duty.min(1.).log10() / 3.).clamp(0., 1.).powf(1.5)
    }
    fn light(alpha: f32) -> Color32 {
        let (r, g, b) = LIGHT;
        Color32::from_rgba_unmultiplied(r, g, b, (alpha.clamp(0., 1.) * 255.) as u8)
    }
    /// Light around the key, drawn before it.
    pub fn halo(&self, painter: &egui::Painter, rect: Rect, radius: f32, id: usize, scale: f32) {
        let glow = self.glow(id);
        if glow == 0. {
            return;
        }
        for ring in 1..=4 {
            let width = 2.5 * scale;
            painter.rect_stroke(
                rect.expand(width * (ring as f32 - 0.5)),
                radius + width * ring as f32,
                Stroke::new(width, Self::light(glow * 0.32 / ring as f32)),
                StrokeKind::Middle,
            );
        }
    }
    /// Light through the key's face, drawn over its fill.
    pub fn tint(&self, painter: &egui::Painter, rect: Rect, radius: f32, id: usize) {
        let glow = self.glow(id);
        if glow > 0. {
            painter.rect_filled(rect, radius, Self::light(glow * TINT));
        }
    }
    /// Label colour on key `id`: darker as its face lights, so it stays legible.
    pub fn ink(&self, id: usize, ink: Color32) -> Color32 {
        let t = self.glow(id);
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
        Color32::from_rgb(
            mix(ink.r(), INK_LIT.r()),
            mix(ink.g(), INK_LIT.g()),
            mix(ink.b(), INK_LIT.b()),
        )
    }
    /// The window's on/off switch.
    pub fn toggle(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.enabled, "LEDs")
            .on_hover_text("Light the keys and buttons as the guest drives the panel LEDs");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glow_orders_backlight_dim_and_full_and_turns_off() {
        let mut view = LedView::new();
        let mut level = [0.; KEYS];
        level[2] = 0.0055; // SLOOP 2.3 LIGHTS LOW
        level[3] = 0.017; // HIGH
        level[4] = 0.25; // a dim LED: one frame in four
        level[5] = 0.98; // full
        level[6] = 0.0004;
        view.receive(level);
        let glow: Vec<_> = (2..=6).map(|id| view.glow(id)).collect();
        assert!(glow[0] > 0. && glow[0] < glow[1] && glow[1] < glow[2] && glow[2] < glow[3]);
        assert!(glow[3] - glow[2] > 0.2, "dim vs full: {glow:?}");
        assert_eq!(glow[4], 0.);
        view.enabled = false;
        assert_eq!(view.glow(5), 0.);
        view.enabled = true;
        view.reset();
        assert_eq!(view.glow(5), 0.);
    }
}
