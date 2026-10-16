// Boot intro: when a cart is handed over, the bios UI plays an animation of the logo with a boot
// chime while the launcher mounts the cart (and preloads it, see preload.rs). The launcher
// creates a ready file once the intro may end, and the intro ends once it exists, after a
// minimum time.

use std::path::{Path, PathBuf};
use std::process;

use macroquad::prelude::*;

use crate::launcher::{self, log_event};
use crate::{SoundEffects, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Argument that plays the boot intro on its own, for previewing it
pub const INTRO_ARG: &str = "--intro";
const INTRO_BLACK: f64 = 0.4; // black screen before the logo when started on its own
const INTRO_ENTER: f64 = 1.0; // logo moves to the center with a zoom pulse
const INTRO_PULSE: f32 = 0.12; // extra zoom at the middle of the entrance
const INTRO_COLOR_STEP: f64 = 0.6; // seconds between colors while waiting
const INTRO_MIN: f64 = 3.0; // minimum time before the intro may end
const INTRO_MAX: f64 = 120.0; // safety limit in case the cart media hangs while preloading
const INTRO_EXIT: f64 = 0.8; // fade to black
const INTRO_READY_POLL: f64 = 0.1;
const INTRO_SLOW_FRAME: f64 = 0.5; // frames slower than this are logged
const INTRO_COLORS: [Color; 6] = [
    Color { r: 1.0, g: 0.5, b: 0.5, a: 1.0 },
    Color { r: 1.0, g: 1.0, b: 0.5, a: 1.0 },
    Color { r: 0.5, g: 1.0, b: 0.5, a: 1.0 },
    Color { r: 0.5, g: 1.0, b: 1.0, a: 1.0 },
    Color { r: 0.5, g: 0.5, b: 1.0, a: 1.0 },
    Color { r: 1.0, g: 0.5, b: 1.0, a: 1.0 },
];

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// The logo moves from where it was to the center with a zoom pulse and the boot chime, cycles
/// colors while the launcher prepares the cart and fades to black once the ready file exists
pub struct IntroAnimation {
    start: f64,
    delay: f64,
    from_pos: Vec2,
    from_alpha: f32,
    from_scale: f32,
    sound_played: bool,
    last_frame: f64,
    last_poll: f64,
    ready: bool,
    exit_start: Option<f64>,
}

impl IntroAnimation {
    /// `from_pos` is the top left corner of the unscaled logo when the intro starts
    pub fn new(from_pos: Vec2, from_alpha: f32, from_scale: f32, delay: f64) -> Self {
        IntroAnimation {
            start: get_time(),
            delay,
            from_pos,
            from_alpha,
            from_scale,
            sound_played: false,
            last_frame: 0.0,
            last_poll: f64::MIN,
            ready: false,
            exit_start: None,
        }
    }

    /// Draw one frame; returns true once the intro has faded out
    pub fn draw(&mut self, logo: &Texture2D, sound_effects: &SoundEffects, ready_file: &Path) -> bool {
        let elapsed = get_time() - self.start;
        if elapsed - self.last_frame > INTRO_SLOW_FRAME {
            log_event(&format!("Intro: frame took {:.1} s", elapsed - self.last_frame));
        }
        self.last_frame = elapsed;

        if elapsed >= self.delay {
            if !self.sound_played {
                sound_effects.play_boot();
                self.sound_played = true;
            }

            let enter = ((elapsed - self.delay) / INTRO_ENTER).min(1.0) as f32;
            let eased = 1.0 - (1.0 - enter).powi(3);
            let scale = self.from_scale + (1.0 - self.from_scale) * eased
                + INTRO_PULSE * (std::f32::consts::PI * enter).sin();

            // white during the entrance, then slowly cycle through the colors
            let hold = (elapsed - self.delay - INTRO_ENTER).max(0.0) / INTRO_COLOR_STEP;
            let index = hold as usize;
            let mut color = if hold <= 0.0 {
                WHITE
            } else if index == 0 {
                lerp_color(WHITE, INTRO_COLORS[0], hold as f32)
            } else {
                lerp_color(
                    INTRO_COLORS[(index - 1) % INTRO_COLORS.len()],
                    INTRO_COLORS[index % INTRO_COLORS.len()],
                    hold.fract() as f32,
                )
            };
            color.a = self.from_alpha + (1.0 - self.from_alpha) * eased;

            let center = Vec2::new(
                ((SCREEN_WIDTH as f32 - logo.width()) / 2.0).round(),
                ((SCREEN_HEIGHT as f32 - logo.height()) / 2.0).round(),
            );
            let pos = self.from_pos + (center - self.from_pos) * eased;
            // scale around the middle of the logo
            let w = (logo.width() * scale).round();
            let h = (logo.height() * scale).round();
            let x = (pos.x + (logo.width() - w) / 2.0).round();
            let y = (pos.y + (logo.height() - h) / 2.0).round();
            draw_texture_ex(logo, x, y, color, DrawTextureParams {
                dest_size: Some(Vec2::new(w, h)),
                ..Default::default()
            });
        }

        if self.exit_start.is_none() {
            if !self.ready && elapsed - self.last_poll >= INTRO_READY_POLL {
                self.ready = ready_file.exists();
                self.last_poll = elapsed;
                if self.ready {
                    log_event(&format!("Intro: cart ready after {:.1} s", elapsed));
                }
            }
            if elapsed >= INTRO_MIN && (self.ready || elapsed >= INTRO_MAX) {
                log_event("Intro: fading out");
                self.exit_start = Some(elapsed);
            }
        }

        match self.exit_start {
            Some(exit_time) => {
                let progress = ((elapsed - exit_time) / INTRO_EXIT).min(1.0) as f32;
                draw_rectangle(0.0, 0.0, SCREEN_WIDTH as f32, SCREEN_HEIGHT as f32,
                    Color { r: 0.0, g: 0.0, b: 0.0, a: progress });
                progress >= 1.0
            }
            None => false,
        }
    }
}

/// Play the intro on its own, ending once `ready_file` exists; for previewing it
pub async fn run_intro(ready_file: PathBuf) {
    let logo = Texture2D::from_file_with_format(include_bytes!("../logo.png"), Some(ImageFormat::Png));
    logo.set_filter(FilterMode::Nearest);
    let sound_effects = SoundEffects::new().await;
    let center = Vec2::new(
        ((SCREEN_WIDTH as f32 - logo.width()) / 2.0).round(),
        ((SCREEN_HEIGHT as f32 - logo.height()) / 2.0).round(),
    );
    let mut intro = IntroAnimation::new(center, 0.0, 0.6, INTRO_BLACK);
    loop {
        clear_background(BLACK);
        if intro.draw(&logo, &sound_effects, &ready_file) {
            process::exit(0);
        }
        next_frame().await
    }
}

/// Created by the launcher once the cart is mounted and preloaded, so the intro may end
pub fn ready_file() -> PathBuf {
    launcher::runtime_dir().join("kazeta-intro-ready")
}
