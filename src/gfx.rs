//! Bitmap text rendering, vendored from
//! `xous-core/bao1x-boot/boot1/src/platform/bao1x/gfx.rs`.
//!
//! Deliberately self-contained: it walks a 1bpp font atlas and calls `put_pixel`
//! on any `FrameBuffer`. No allocator, no Xous graphics server, no blitstr2 --
//! which is what makes text possible at all in a baremetal image.
//!
//! Reworked from the original into a single-glyph blit ([`glyph`]) plus thin
//! string wrappers, because the animations draw cell by cell rather than by
//! line. Line breaks are also handled properly here; boot1's version resets the
//! cursor to a fixed left margin but keeps advancing the per-string glyph index,
//! so its `\r`/`\n` never actually land where they claim to.

#![allow(dead_code)]

use bao1x_hal::sh1107::{COLUMN, ROW};
use ux_api::minigfx::*;

// This font is from the embedded-graphics crate,
// https://docs.rs/embedded-graphics/0.7.1/embedded_graphics/
const FONT_IMAGE: &'static [u8] = include_bytes!("font6x12_1bpp.raw");
pub const CHAR_HEIGHT: isize = 12;
pub const CHAR_WIDTH: isize = 6;
const FONT_IMAGE_WIDTH: isize = 96;

fn char_offset(c: char) -> isize {
    let fallback = ' ' as isize - ' ' as isize;
    if c < ' ' {
        return fallback;
    }
    if c <= '~' {
        return c as isize - ' ' as isize;
    }
    fallback
}

/// Blit a single character cell with its top-left corner at `pos`.
///
/// `fg`/`bg` are native colors; on the SH1107 use
/// `bao1x_hal::sh1107::Mono::White.into()` / `Mono::Black.into()`. Note the
/// panel's inverted polarity: `Mono::White` is `ColorNative(0)`, i.e. a *cleared*
/// bit is a lit pixel.
pub fn glyph(fb: &mut dyn FrameBuffer, c: char, pos: Point, fg: ColorNative, bg: ColorNative) {
    // adapted from the embedded-graphics crate
    let char_per_row = FONT_IMAGE_WIDTH / CHAR_WIDTH;
    let offset = char_offset(c);
    let row = offset / char_per_row;
    // top left corner of the character within the atlas, in pixels
    let char_x = (offset - (row * char_per_row)) * CHAR_WIDTH;
    let char_y = row * CHAR_HEIGHT;

    for dy in 0..CHAR_HEIGHT {
        for dx in 0..CHAR_WIDTH {
            let bit_index = char_x + dx + FONT_IMAGE_WIDTH * (char_y + dy);
            let byte = FONT_IMAGE[(bit_index / 8) as usize];
            let lit = byte & (1 << (7 - (bit_index % 8))) != 0;
            fb.put_pixel(Point::new(pos.x + dx, pos.y + dy), if lit { fg } else { bg });
        }
    }
}

/// Like [`glyph`], but leaves the background untouched. Used by the animations,
/// which paint onto an already-cleared screen and would otherwise spend most of
/// their time blitting blank cells.
pub fn glyph_transparent(fb: &mut dyn FrameBuffer, c: char, pos: Point, fg: ColorNative) {
    let char_per_row = FONT_IMAGE_WIDTH / CHAR_WIDTH;
    let offset = char_offset(c);
    let row = offset / char_per_row;
    let char_x = (offset - (row * char_per_row)) * CHAR_WIDTH;
    let char_y = row * CHAR_HEIGHT;

    for dy in 0..CHAR_HEIGHT {
        for dx in 0..CHAR_WIDTH {
            let bit_index = char_x + dx + FONT_IMAGE_WIDTH * (char_y + dy);
            let byte = FONT_IMAGE[(bit_index / 8) as usize];
            if byte & (1 << (7 - (bit_index % 8))) != 0 {
                fb.put_pixel(Point::new(pos.x + dx, pos.y + dy), fg);
            }
        }
    }
}

/// Draw `text` with its top-left corner at `top_left`. `\n` and `\r` return the
/// cursor to `top_left.x`; `\n` also advances a line.
pub fn msg(fb: &mut dyn FrameBuffer, text: &str, top_left: Point, fg: ColorNative, bg: ColorNative) {
    let mut cursor = top_left;
    for c in text.chars() {
        match c {
            '\r' => cursor.x = top_left.x,
            '\n' => {
                cursor.x = top_left.x;
                cursor.y += CHAR_HEIGHT;
            }
            _ => {
                glyph(fb, c, cursor, fg, bg);
                cursor.x += CHAR_WIDTH;
            }
        }
    }
}

/// Width of `text` in pixels, for layout.
pub fn text_width(text: &str) -> isize { text.chars().count() as isize * CHAR_WIDTH }

/// Horizontally center `text` on a `width`-wide display at vertical offset `y`.
pub fn msg_centered(
    fb: &mut dyn FrameBuffer,
    text: &str,
    width: isize,
    y: isize,
    fg: ColorNative,
    bg: ColorNative,
) {
    let w = text_width(text);
    let x = if w >= width { 0 } else { (width - w) / 2 };
    msg(fb, text, Point::new(x, y), fg, bg);
}

/// Fill the inclusive rectangle `tl..=br` with `color`.
///
/// `ux_api::minigfx::op::rectangle` does the same thing, but by way of an
/// iterator that re-evaluates the style per pixel; screens redraw these often
/// enough that the direct loop is worth it.
pub fn fill_rect(fb: &mut dyn FrameBuffer, tl: Point, br: Point, color: ColorNative) {
    for y in tl.y..=br.y {
        for x in tl.x..=br.x {
            fb.put_pixel(Point::new(x, y), color);
        }
    }
}

/// A one-pixel line between two points, by Bresenham's algorithm.
///
/// Integer-only and branch-cheap, which matters because scripts draw these one
/// at a time from the interpreter -- the per-pixel cost here is dwarfed by the
/// evaluator, but the per-call cost is not.
pub fn line(fb: &mut dyn FrameBuffer, from: Point, to: Point, color: ColorNative) {
    let dx = (to.x - from.x).abs();
    let dy = -(to.y - from.y).abs();
    let sx = if from.x < to.x { 1 } else { -1 };
    let sy = if from.y < to.y { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (from.x, from.y);

    loop {
        fb.put_pixel(Point::new(x, y), color);
        if x == to.x && y == to.y {
            break;
        }
        // Doubling the error before comparing keeps everything in integers;
        // the two independent tests are what make the diagonal case work.
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

// ---------------------------------------------------------------- sprites
//
// Three states per pixel, which is what lets Badgy sit on top of a live
// animation: lit draws white, dark draws black, and clear is left alone. A
// two-state sprite would either have to be drawn on a cleared screen or leak
// the background through the middle of the badger.

pub const CLEAR: u8 = 0;
pub const INK: u8 = 1;
pub const DARK: u8 = 2;

/// Anything the sprite blitter can read pixels out of.
///
/// There are two, and they are stored differently for good reasons that have
/// nothing to do with each other: the sheet in [`crate::sprites`] is `&'static
/// str` rows in FLASH, because art that never changes should cost image bytes
/// and no RAM; a script's injected frame is a byte grid in `.bss`, because it
/// arrives at runtime and has to be all-zeroes at boot. This trait is the one
/// line where that stops mattering.
pub trait Pixels {
    fn width(&self) -> u16;
    fn height(&self) -> u16;
    /// [`CLEAR`], [`INK`] or [`DARK`] at `(x, y)`.
    fn pixel(&self, x: usize, y: usize) -> u8;
}

impl Pixels for crate::sprites::Sprite {
    fn width(&self) -> u16 { self.w }

    fn height(&self) -> u16 { self.h }

    /// Rows shorter than `w` read as trailing spaces, so the art may be
    /// right-trimmed in the source without changing what it draws.
    fn pixel(&self, x: usize, y: usize) -> u8 {
        match self.at(x, y) {
            b'#' => INK,
            b'.' => DARK,
            _ => CLEAR,
        }
    }
}

/// Blit a sprite with its top-left corner at `at`.
pub fn sprite(fb: &mut dyn FrameBuffer, s: &dyn Pixels, at: Point, ink: ColorNative, dark: ColorNative) {
    for y in 0..s.height() as usize {
        for x in 0..s.width() as usize {
            let color = match s.pixel(x, y) {
                INK => ink,
                DARK => dark,
                _ => continue,
            };
            fb.put_pixel(Point::new(at.x + x as isize, at.y + y as isize), color);
        }
    }
}

/// Blit a sprite centred horizontally on a `width`-wide display.
pub fn sprite_centered(
    fb: &mut dyn FrameBuffer,
    s: &dyn Pixels,
    width: isize,
    y: isize,
    ink: ColorNative,
    dark: ColorNative,
) {
    let x = (width - s.width() as isize) / 2;
    sprite(fb, s, Point::new(x.max(0), y), ink, dark);
}

/// A one-pixel outline around the inclusive rectangle `tl..=br`.
pub fn stroke_rect(fb: &mut dyn FrameBuffer, tl: Point, br: Point, color: ColorNative) {
    for x in tl.x..=br.x {
        fb.put_pixel(Point::new(x, tl.y), color);
        fb.put_pixel(Point::new(x, br.y), color);
    }
    for y in tl.y..=br.y {
        fb.put_pixel(Point::new(tl.x, y), color);
        fb.put_pixel(Point::new(br.x, y), color);
    }
}

// ------------------------------------------------------------ off-screen page

/// Words in one 128x128 1bpp page. The panel is 2 KiB, which is small enough
/// that every task can have its own.
pub const FB_WORDS: usize = (COLUMN * ROW) as usize / 32;

/// An off-screen copy of the panel.
///
/// # Why every task draws into one of these
///
/// There is one OLED and there can be several scripts. Handing them all a
/// `&mut Oled128x128` is not expressible and would not be right anyway: two
/// scripts drawing into the same page would interleave into garbage. So nobody
/// draws to the panel. Each task draws here, and the compositor -- the UI task,
/// which is the only thing that ever touches the hardware -- copies whichever
/// page is in focus onto glass with [`Oled128x128::blit_screen`].
///
/// The bit layout is deliberately the driver's own (`bitnum = x + y * COLUMN`,
/// LSB first, and the panel's inverted polarity where a *cleared* bit is lit),
/// so presenting a page is a 2 KiB `copy_from_slice` and not a conversion.
///
/// This costs a memcpy per presented frame -- about 15 us against the 14 ms the
/// panel refresh itself takes, so it is not a cost worth avoiding.
pub struct Fb {
    words: [u32; FB_WORDS],
}

impl Fb {
    /// An *unusable* page, for building the static table only.
    ///
    /// Zeros rather than the all-ones a blank page actually is, and that is not
    /// a bug: every non-zero word of a `static` becomes an entry in the image's
    /// poke table, which holds 40 and would need 1536 for three all-ones pages.
    /// Zero keeps the table in `.bss`, where it costs nothing in the image and
    /// nothing at boot. Pages are blanked with `clear()` when a task takes one,
    /// which is the only point at which the contents could matter -- and on
    /// this panel a zero bit is a *lit* pixel, so a page that skipped that step
    /// would come up white.
    pub const NEW: Fb = Fb { words: [0; FB_WORDS] };

    pub fn words(&self) -> &[u32; FB_WORDS] { &self.words }
}

impl FrameBuffer for Fb {
    fn put_pixel(&mut self, p: Point, color: ColorNative) {
        if p.x >= COLUMN || p.y >= ROW || p.x < 0 || p.y < 0 {
            return;
        }
        // safety: the bounds check above is exactly `put_pixel_unchecked`'s
        // requirement.
        unsafe { self.put_pixel_unchecked(p, color) }
    }

    #[inline(always)]
    unsafe fn put_pixel_unchecked(&mut self, p: Point, color: ColorNative) {
        let bitnum = (p.x + p.y * COLUMN) as usize;
        if color.0 != 0 {
            self.words[bitnum >> 5] |= 1 << (bitnum & 0x1f);
        } else {
            self.words[bitnum >> 5] &= !(1 << (bitnum & 0x1f));
        }
    }

    fn get_pixel(&self, p: Point) -> Option<ColorNative> {
        if p.x >= COLUMN || p.y >= ROW || p.x < 0 || p.y < 0 {
            return None;
        }
        let bitnum = (p.x + p.y * COLUMN) as usize;
        if self.words[bitnum >> 5] & 1 << (bitnum & 0x1f) != 0 { Some(1.into()) } else { Some(0.into()) }
    }

    fn xor_pixel(&mut self, p: Point) {
        if let Some(px) = self.get_pixel(p) {
            self.put_pixel(p, ColorNative(if px.0 != 0 { 0 } else { 1 }));
        }
    }

    /// A no-op, and not an oversight: a page reaches the panel when the
    /// compositor presents it, not when its owner asks. `Host::gfx_show` is
    /// what marks the page ready -- see [`crate::sched::present`].
    fn draw(&mut self) -> Result<(), xous::Error> { Ok(()) }

    fn clear(&mut self) { self.words.fill(0xffff_ffff); }

    fn dimensions(&self) -> Point { Point::new(COLUMN, ROW) }

    unsafe fn raw_mut(&mut self) -> &mut ux_api::platform::FbRaw { &mut self.words }
}
