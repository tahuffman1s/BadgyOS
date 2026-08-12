//! Running a script on the badge: the [`Host`] the interpreter talks to.
//!
//! `pycon` knows nothing about this hardware -- it draws through a trait,
//! which is what lets it be tested on a laptop. This is the other side of that
//! trait, and it is where three badge-specific concerns live:
//!
//! * **Keeping USB alive.** The interpreter calls [`Host::tick`] every couple of thousand steps, and every
//!   blocking call (`sleep`, `wait_key`, `show`) routes through the same place. Since the whole firmware is
//!   one loop, if a script did not yield, the drive would stop responding for as long as it ran.
//!
//! * **Getting the badge back.** A script is untrusted text that arrived over USB, and `while True: pass` is
//!   a legal program. Holding LEFT and CENTER together stops it. Two keys, because single keys belong to the
//!   script -- `keys()` is part of the API and a game that could not read LEFT would be a poor one.
//!
//! * **Hiding the panel.** Scripts see `on`/`off`, never `ColorNative`, and never the SH1107's inverted
//!   polarity where a cleared bit is a lit pixel. Coordinates are clamped rather than trusted, so `rect(0, 0,
//!   99999, 99999)` costs one screenful of work instead of billions of iterations.

use bao1x_hal::sh1107::{COLUMN, Mono, Oled128x128, ROW};
use pycon::host::{Abort, Host};
use ux_api::minigfx::{ColorNative, FrameBuffer, Point};

use crate::gfx;
use crate::input::{Key, Keys};
use crate::platform;
use crate::usb;
use crate::util::Rng;

/// Consecutive [`Host::tick`] calls the kill chord must be held for. The chord
/// is sampled undebounced, so requiring a few in a row rejects contact bounce
/// without adding a timer.
const KILL_HOLD: u8 = 3;

/// Passes of the internal wait loop between key samples, in milliseconds.
const WAIT_STEP_MS: usize = 2;

/// How long to wait for the interrupt endpoint to drain before giving up on a
/// mouse report, in `MOUSE_WAIT_STEP_MS` slices.
///
/// The host polls every 8 to 10 ms, so one report should clear well inside
/// this. The bound exists for the case where it never does -- a host that has
/// stopped polling without dropping the configuration -- where blocking
/// forever would hang the script on a cable someone walked away from.
const MOUSE_WAIT_STEPS: u32 = 32;
const MOUSE_WAIT_STEP_MS: usize = 2;

pub struct BadgeHost<'a, 'd> {
    disp: &'a mut Oled128x128<'d>,
    keys: &'a mut Keys,
    rng: Rng,
    /// How many consecutive checks have seen the kill chord.
    kill_streak: u8,
    /// Raw key mask at the previous sample, for edge detection in `wait_key`.
    last_raw: u8,
    /// Set once the chord has fired, so every later call gives up immediately
    /// instead of the script getting a chance to ignore one `Abort`.
    stopped: bool,
    /// Mouse buttons the script is holding down. Kept here rather than in the
    /// USB module because it is script state: a move has to carry the buttons
    /// that were already down, or every drag would drop halfway through.
    buttons: u8,
}

impl<'a, 'd> BadgeHost<'a, 'd> {
    pub fn new(disp: &'a mut Oled128x128<'d>, keys: &'a mut Keys, seed: u32) -> Self {
        BadgeHost { disp, keys, rng: Rng::new(seed), kill_streak: 0, last_raw: 0, stopped: false, buttons: 0 }
    }

    /// Queue one HID report, waiting for the endpoint if it is still busy with
    /// the previous one.
    ///
    /// Keeps ticking while it waits, so USB stays serviced -- which it has to,
    /// because the thing being waited for is a USB completion -- and so the
    /// kill chord still works.
    fn report(&mut self, dx: i8, dy: i8, wheel: i8) -> Result<bool, Abort> {
        for _ in 0..MOUSE_WAIT_STEPS {
            self.service()?;
            if !usb::hid::is_ready() {
                return Ok(false);
            }
            if usb::hid::send(self.buttons, dx, dy, wheel) {
                return Ok(true);
            }
            platform::delay_polled(MOUSE_WAIT_STEP_MS, &mut usb::poll);
        }
        Ok(false)
    }

    /// One sample of the key matrix plus a USB service pass. Everything that
    /// blocks funnels through here.
    fn service(&mut self) -> Result<u8, Abort> {
        usb::poll();
        let raw = self.keys.scan_raw();

        let chord = Key::Left.bit() | Key::Center.bit();
        if raw & chord == chord {
            self.kill_streak = self.kill_streak.saturating_add(1);
            if self.kill_streak >= KILL_HOLD {
                self.stopped = true;
            }
        } else {
            self.kill_streak = 0;
        }

        if self.stopped { Err(Abort) } else { Ok(raw) }
    }

    fn color(on: bool) -> ColorNative {
        // The panel is inverted: `Mono::White` is `ColorNative(0)`, i.e. a
        // cleared bit lights the pixel. Scripts never have to know that.
        if on { Mono::White.into() } else { Mono::Black.into() }
    }

    fn clamp(v: i32, hi: isize) -> isize { (v as isize).clamp(0, hi - 1) }
}

impl Host for BadgeHost<'_, '_> {
    fn tick_interval(&self) -> u32 {
        // Lower than the crate default. A script's inner loop is usually a few
        // dozen steps, so this checks the exit chord several times per frame
        // while adding a rounding error's worth of overhead to real work.
        512
    }

    fn tick(&mut self) -> Result<(), Abort> { self.service().map(|_| ()) }

    fn heap_pressure(&self) -> bool {
        // Every individual limit in the interpreter -- list length, string
        // length, recursion depth -- bounds one shape of runaway. This bounds
        // all of them at once, and catches the shape none of them do: a script
        // that just keeps a great many small things alive. Stopping here turns
        // an allocation failure (a panic, which on this device spins forever)
        // into a line of text on the panel.
        platform::heap_free() < platform::HEAP_RESERVE
    }

    fn print_line(&mut self, s: &str) {
        crate::println!("{}", s);
    }

    fn gfx_clear(&mut self) {
        let fb: &mut dyn FrameBuffer = self.disp;
        fb.clear();
    }

    fn gfx_pixel(&mut self, x: i32, y: i32, on: bool) {
        // `put_pixel` clips on its own, so out-of-range values are simply
        // dropped rather than clamped onto the edge -- a clamped pixel would
        // draw a spurious line down the side of the screen.
        let fb: &mut dyn FrameBuffer = self.disp;
        fb.put_pixel(Point::new(x as isize, y as isize), Self::color(on));
    }

    fn gfx_text(&mut self, x: i32, y: i32, s: &str, on: bool) {
        let fg = Self::color(on);
        let fb: &mut dyn FrameBuffer = self.disp;
        // Drawn transparently, so text over an existing shape does not punch a
        // rectangular hole in it. Scripts that want a background can draw one.
        let mut cursor = Point::new(x as isize, y as isize);
        for c in s.chars() {
            match c {
                '\n' => {
                    cursor.x = x as isize;
                    cursor.y += gfx::CHAR_HEIGHT;
                }
                '\r' => cursor.x = x as isize,
                _ => {
                    // Skip cells that are entirely off-screen instead of
                    // blitting 72 clipped pixels each.
                    if cursor.x > -gfx::CHAR_WIDTH
                        && cursor.x < COLUMN
                        && cursor.y > -gfx::CHAR_HEIGHT
                        && cursor.y < ROW
                    {
                        gfx::glyph_transparent(fb, c, cursor, fg);
                    }
                    cursor.x += gfx::CHAR_WIDTH;
                }
            }
        }
    }

    fn gfx_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, fill: bool) {
        // Clamped, not clipped: `fill_rect` iterates the range it is given, and
        // an unclamped `rect(0, 0, 2000000000, 2000000000)` would run for hours
        // with the screen frozen. The visible result is the same.
        let (ax, ay) = (Self::clamp(x0.min(x1), COLUMN), Self::clamp(y0.min(y1), ROW));
        let (bx, by) = (Self::clamp(x0.max(x1), COLUMN), Self::clamp(y0.max(y1), ROW));
        let color = Self::color(true);
        let fb: &mut dyn FrameBuffer = self.disp;
        if fill {
            gfx::fill_rect(fb, Point::new(ax, ay), Point::new(bx, by), color);
        } else {
            gfx::stroke_rect(fb, Point::new(ax, ay), Point::new(bx, by), color);
        }
    }

    fn gfx_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let color = Self::color(true);
        let fb: &mut dyn FrameBuffer = self.disp;
        gfx::line(
            fb,
            Point::new(Self::clamp(x0, COLUMN), Self::clamp(y0, ROW)),
            Point::new(Self::clamp(x1, COLUMN), Self::clamp(y1, ROW)),
            color,
        );
    }

    fn gfx_show(&mut self) -> Result<(), Abort> {
        // The panel refresh is ~14 ms of blocking SPI and cannot be broken up
        // from here, so bracket it: this is the one unavoidable gap in USB
        // service, and the host will simply retry anything NAKed during it.
        usb::poll();
        self.disp.draw().ok();
        self.service().map(|_| ())
    }

    fn keys(&mut self) -> u32 {
        // Ignore an error here: a script asking for the key state during the
        // kill chord will be stopped at its next tick anyway, and returning 0
        // is more useful than pretending nothing is pressed.
        self.service().unwrap_or(0) as u32
    }

    fn wait_key(&mut self) -> Result<u32, Abort> {
        // Start from whatever is held now, so a key still down from the menu
        // selection that launched the script does not count as a press.
        self.last_raw = self.service()?;
        loop {
            platform::delay_polled(WAIT_STEP_MS, &mut usb::poll);
            let raw = self.service()?;
            let pressed = raw & !self.last_raw;
            self.last_raw = raw;
            if pressed != 0 {
                return Ok(pressed as u32);
            }
        }
    }

    fn sleep_ms(&mut self, ms: u32) -> Result<(), Abort> {
        // Broken into short slices so a `sleep(10000)` is still interruptible
        // and still services the drive.
        let mut left = ms as usize;
        while left > 0 {
            let slice = left.min(4);
            platform::delay_polled(slice, &mut usb::poll);
            self.service()?;
            left -= slice;
        }
        Ok(())
    }

    fn random(&mut self) -> u32 { self.rng.next() }

    fn mouse_ready(&mut self) -> bool {
        usb::poll();
        usb::hid::is_ready()
    }

    fn mouse_move(&mut self, dx: i8, dy: i8, wheel: i8) -> Result<bool, Abort> { self.report(dx, dy, wheel) }

    fn mouse_buttons(&mut self, mask: u8) -> Result<bool, Abort> {
        // Recorded before the send, not after: if the report is dropped because
        // nothing is listening, the script's idea of what is held should still
        // be what it asked for, so that the next successful report carries it.
        self.buttons = mask & usb::hid::BUTTON_MASK;
        self.report(0, 0, 0)
    }

    fn usb_ids(&mut self) -> (u16, u16) { usb::ids() }

    fn usb_set_identity(&mut self, vid: u16, pid: u16) -> Result<bool, Abort> {
        // Give the kill chord a look before a re-enumeration takes the bus down
        // for the better part of a second, and pump the loop once after so the
        // fresh controller is serviced before the script runs on.
        self.service()?;
        let ok = usb::set_identity(vid, pid);
        self.service()?;
        Ok(ok)
    }

    fn usb_set_name(&mut self, name: &str) -> Result<bool, Abort> {
        self.service()?;
        usb::set_product_name(name);
        self.service()?;
        Ok(true)
    }
}

/// Let go of any mouse button the script was holding when it ended.
///
/// A script killed with the exit chord halfway through a drag would otherwise
/// leave the host believing the button is still down -- and nothing else ever
/// tells it otherwise, so the user's next click lands as the end of a selection
/// they did not start. The release is best-effort: if there is no host, there
/// is nothing holding anything.
impl Drop for BadgeHost<'_, '_> {
    fn drop(&mut self) {
        if self.buttons == 0 {
            return;
        }
        self.buttons = 0;
        for _ in 0..MOUSE_WAIT_STEPS {
            if !usb::hid::is_ready() || usb::hid::send(0, 0, 0, 0) {
                return;
            }
            platform::delay_polled(MOUSE_WAIT_STEP_MS, &mut usb::poll);
        }
    }
}
