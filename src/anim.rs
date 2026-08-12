//! Full-screen ASCII animations.
//!
//! Everything here paints on the 21x10 character grid that the 6x12 font makes
//! out of the 128x128 panel, using integer math only -- the RV32IMAC core has no
//! FPU, and a soft-float sine per cell per frame would eat the whole budget.
//!
//! Each animation is a plain struct with `step()` (advance one frame) and
//! `render()` (paint the current frame). The caller clears the framebuffer and
//! pushes it, so a screen can compose an animation with other elements -- which
//! is what the splash does.

use bao1x_hal::sh1107::{COLUMN, Mono, ROW};
use ux_api::minigfx::{ColorNative, FrameBuffer, Point};

use crate::gfx::{CHAR_HEIGHT, CHAR_WIDTH, glyph, glyph_transparent};
use crate::util::{Rng, hash3};

/// The character grid: 21 columns x 10 rows, centered on the panel.
pub const COLS: usize = (COLUMN / CHAR_WIDTH) as usize;
pub const ROWS: usize = (ROW / CHAR_HEIGHT) as usize;
const ORIGIN_X: isize = (COLUMN - COLS as isize * CHAR_WIDTH) / 2;
const ORIGIN_Y: isize = (ROW - ROWS as isize * CHAR_HEIGHT) / 2;

/// Top-left pixel of a grid cell.
fn cell(col: usize, row: usize) -> Point {
    Point::new(ORIGIN_X + col as isize * CHAR_WIDTH, ORIGIN_Y + row as isize * CHAR_HEIGHT)
}

fn lit() -> ColorNative { Mono::White.into() }

/// Which animation a demo screen is running.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Demo {
    Matrix,
    Fire,
    Plasma,
}

impl Demo {
    pub const fn title(self) -> &'static str {
        match self {
            Demo::Matrix => "MATRIX RAIN",
            Demo::Fire => "ASCII FIRE",
            Demo::Plasma => "PLASMA",
        }
    }
}

// ---------------------------------------------------------------- matrix rain

/// Glyphs the rain is made of. Deliberately dense and angular; the real thing
/// uses half-width katakana, which this font does not have.
const RAIN_GLYPHS: &[u8] = b"01<>*+-=/\\|#@$%&ABCDEFHKLMNPRSTVXYZ";

#[derive(Copy, Clone)]
struct Drop {
    /// Row of the leading character. Starts negative so drops enter from above.
    head: i16,
    /// How many characters trail behind the head.
    len: u8,
    /// Frames between steps -- the reason the columns fall at different speeds.
    period: u8,
    /// Offset into `period`, so columns do not all step on the same frame.
    phase: u8,
}

pub struct MatrixRain {
    drops: [Drop; COLS],
    rng: Rng,
    frame: u32,
}

impl MatrixRain {
    pub fn new(seed: u32) -> Self {
        let mut rng = Rng::new(seed);
        let mut drops = [Drop { head: 0, len: 4, period: 2, phase: 0 }; COLS];
        for d in drops.iter_mut() {
            respawn(d, &mut rng);
            // Scatter the initial heads over the screen instead of starting
            // every column above the top edge together.
            d.head = rng.below(ROWS as u32 + 8) as i16 - 8;
        }
        MatrixRain { drops, rng, frame: 0 }
    }

    pub fn step(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        for d in self.drops.iter_mut() {
            if (self.frame.wrapping_add(d.phase as u32)) % d.period as u32 != 0 {
                continue;
            }
            d.head += 1;
            if d.head - d.len as i16 > ROWS as i16 {
                respawn(d, &mut self.rng);
            }
        }
    }

    pub fn render(&self, fb: &mut dyn FrameBuffer) {
        let fg = lit();
        let bg: ColorNative = Mono::Black.into();
        for (c, d) in self.drops.iter().enumerate() {
            for i in 0..d.len as i16 {
                let r = d.head - i;
                if r < 0 || r >= ROWS as i16 {
                    continue;
                }
                let pos = cell(c, r as usize);
                if i == 0 {
                    // The leading character, as an inverted cell. This panel has
                    // no greys, so "brighter than the trail" has to mean a solid
                    // block with the glyph knocked out of it.
                    let h = hash3(c as u32, r as u32, self.frame / 2);
                    let ch = RAIN_GLYPHS[(h as usize) % RAIN_GLYPHS.len()] as char;
                    glyph(fb, ch, pos, bg, fg);
                    continue;
                }
                // For the same reason the trail cannot fade by dimming, so it
                // fades by getting sparser: past two thirds of the way back,
                // cells start dropping out at random.
                if i * 3 > d.len as i16 * 2 && hash3(c as u32, r as u32, self.frame / 2) & 1 == 0 {
                    continue;
                }
                // Re-roll the glyph every few frames for the shimmer, but keep
                // it keyed to the cell so it does not churn on every redraw.
                let h = hash3(c as u32, r as u32, self.frame / 4);
                let ch = RAIN_GLYPHS[(h as usize) % RAIN_GLYPHS.len()] as char;
                glyph_transparent(fb, ch, pos, fg);
            }
        }
    }
}

fn respawn(d: &mut Drop, rng: &mut Rng) {
    d.head = -(rng.below(10) as i16);
    d.len = 5 + rng.below(6) as u8;
    d.period = 1 + rng.below(3) as u8;
    d.phase = rng.below(d.period as u32) as u8;
}

// ------------------------------------------------------------------ intensity

/// Density ramp: index 0 is empty, the last entry is as solid as this font gets.
/// Both `Fire` and `Plasma` map an intensity onto it.
const RAMP: &[u8] = b" .:-=+ox*#@";

fn ramp_glyph(level: usize) -> char { RAMP[level.min(RAMP.len() - 1)] as char }

// ------------------------------------------------------------------ doom fire

/// Heat of the bottom row. Nine rows of decay have to take this to zero for the
/// flames to have a tip, which is what sets the ratio against `MAX_DECAY`.
const MAX_HEAT: u8 = 27;
const MAX_DECAY: u32 = 6;

pub struct Fire {
    heat: [[u8; COLS]; ROWS],
    rng: Rng,
}

impl Fire {
    pub fn new(seed: u32) -> Self { Fire { heat: [[0; COLS]; ROWS], rng: Rng::new(seed) } }

    pub fn step(&mut self) {
        // Bottom row is the fuel bed, always at full heat.
        for c in 0..COLS {
            self.heat[ROWS - 1][c] = MAX_HEAT;
        }
        // Propagate upwards, cooling and drifting sideways by a random column.
        // Classic "doom fire": the sideways drift is what makes it flicker.
        for r in (1..ROWS).rev() {
            for c in 0..COLS {
                let src = self.heat[r][c];
                let drift = self.rng.below(3); // 0, 1 or 2 -> left, none, right
                let dst = (c + COLS + 1 - drift as usize) % COLS;
                let decay = self.rng.below(MAX_DECAY) as u8;
                self.heat[r - 1][dst] = src.saturating_sub(decay);
            }
        }
    }

    pub fn render(&self, fb: &mut dyn FrameBuffer) {
        let fg = lit();
        for r in 0..ROWS {
            for c in 0..COLS {
                let level = self.heat[r][c] as usize * (RAMP.len() - 1) / MAX_HEAT as usize;
                if level == 0 {
                    continue;
                }
                glyph_transparent(fb, ramp_glyph(level), cell(c, r), fg);
            }
        }
    }
}

// -------------------------------------------------------------------- plasma

/// One period of a sine, scaled to +/-120, in 64 steps. Three of these summed
/// stay inside +/-360, which fits an i32 with room to spare.
#[rustfmt::skip]
const SINE: [i8; 64] = [
       0,   12,   23,   35,   46,   57,   67,   76,
      85,   93,  100,  106,  111,  115,  118,  119,
     120,  119,  118,  115,  111,  106,  100,   93,
      85,   76,   67,   57,   46,   35,   23,   12,
       0,  -12,  -23,  -35,  -46,  -57,  -67,  -76,
     -85,  -93, -100, -106, -111, -115, -118, -119,
    -120, -119, -118, -115, -111, -106, -100,  -93,
     -85,  -76,  -67,  -57,  -46,  -35,  -23,  -12,
];

fn sine(phase: i32) -> i32 { SINE[(phase & 63) as usize] as i32 }

pub struct Plasma {
    t: i32,
}

impl Plasma {
    pub fn new() -> Self { Plasma { t: 0 } }

    pub fn step(&mut self) { self.t = self.t.wrapping_add(1); }

    pub fn render(&self, fb: &mut dyn FrameBuffer) {
        let fg = lit();
        for r in 0..ROWS {
            let r = r as i32;
            for c in 0..COLS {
                let c = c as i32;
                // Three interfering waves: one along x, one along y, one
                // diagonal and slower. The classic demoscene plasma.
                let v = sine(c * 5 + self.t) + sine(r * 7 - self.t * 2) + sine((c * 3 + r * 4) + self.t / 2);
                // v spans -360..360; fold that onto the density ramp.
                let level = ((v + 360) * (RAMP.len() as i32 - 1) / 720) as usize;
                if level == 0 {
                    continue;
                }
                glyph_transparent(fb, ramp_glyph(level), cell(c as usize, r as usize), fg);
            }
        }
    }
}
