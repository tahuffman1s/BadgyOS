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

/// Blit a [`crate::sprites::Sprite`] with its top-left corner at `at`.
///
/// Three states per pixel, which is what lets Badgy sit on top of a live
/// animation: `#` draws lit, `.` draws dark, and a space is left alone. A
/// two-state sprite would either have to be drawn on a cleared screen or leak
/// the background through the middle of the badger.
///
/// Rows shorter than `w` are treated as trailing spaces, so the art may be
/// right-trimmed in the source without changing what it draws.
pub fn sprite(
    fb: &mut dyn FrameBuffer,
    s: &crate::sprites::Sprite,
    at: Point,
    ink: ColorNative,
    dark: ColorNative,
) {
    for y in 0..s.h as usize {
        for x in 0..s.w as usize {
            let color = match s.at(x, y) {
                b'#' => ink,
                b'.' => dark,
                _ => continue,
            };
            fb.put_pixel(Point::new(at.x + x as isize, at.y + y as isize), color);
        }
    }
}

/// Blit a sprite centred horizontally on a `width`-wide display.
pub fn sprite_centered(
    fb: &mut dyn FrameBuffer,
    s: &crate::sprites::Sprite,
    width: isize,
    y: isize,
    ink: ColorNative,
    dark: ColorNative,
) {
    let x = (width - s.w as isize) / 2;
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
