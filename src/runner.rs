//! Running a script on the badge: the [`Host`] the interpreter talks to.
//!
//! `pycon` knows nothing about this hardware -- it draws through a trait,
//! which is what lets it be tested on a laptop. This is the other side of that
//! trait, and it is where the badge-specific concerns live:
//!
//! * **Keeping everything else alive.** The interpreter calls [`Host::tick`] every couple of thousand steps,
//!   and every blocking call (`sleep`, `wait_key`, `show`) routes through the same place. That was already
//!   how USB stayed serviced while a script ran; it is now also where the script gives up the CPU, so it is
//!   the reason several scripts can run at once. See [`crate::sched`].
//!
//! * **Getting the badge back.** A script is untrusted text that arrived over USB, and `while True: pass` is
//!   a legal program. Holding LEFT and CENTER together stops the script you are looking at. Two keys, because
//!   single keys belong to the script -- `keys()` is part of the API and a game that could not read LEFT
//!   would be a poor one. A script in the background is stopped from the task manager instead, which sets the
//!   same flag and takes the same path out.
//!
//! * **Sharing what there is only one of.** A task draws into its own page rather than onto the panel, and
//!   sees the keys only while it has focus. The mouse and the USB identity cannot be split that way -- there
//!   is one bus and one descriptor set -- so the first task to ask for them keeps them, and the others are
//!   told there is nothing listening, which is an answer the API already had to have.
//!
//! * **Hiding the panel.** Scripts see `on`/`off`, never `ColorNative`, and never the SH1107's inverted
//!   polarity where a cleared bit is a lit pixel. Coordinates are clamped rather than trusted, so `rect(0, 0,
//!   99999, 99999)` costs one screenful of work instead of billions of iterations.

use alloc::string::String;
use alloc::vec::Vec;

use bao1x_hal::sh1107::{COLUMN, Mono, ROW};
use pycon::host::{Abort, Host};
use ux_api::minigfx::{ColorNative, FrameBuffer, Point};

use crate::badgy;
use crate::gfx::{self, Fb};
use crate::input::{self, Key};
use crate::mascot;
use crate::platform;
use crate::sched::{self, Ended, Status};
use crate::usb;
use crate::util::{FmtBuf, Rng};

/// Consecutive [`Host::tick`] calls the kill chord must be held for. The chord
/// is sampled undebounced, so requiring a few in a row rejects contact bounce
/// without adding a timer.
const KILL_HOLD: u8 = 3;

/// Milliseconds between key samples inside `wait_key`.
const WAIT_STEP_MS: u32 = 2;

/// How long to wait for the interrupt endpoint to drain before giving up on a
/// mouse report, in `MOUSE_WAIT_STEP_MS` slices.
///
/// The host polls every 8 to 10 ms, so one report should clear well inside
/// this. The bound exists for the case where it never does -- a host that has
/// stopped polling without dropping the configuration -- where blocking
/// forever would hang the script on a cable someone walked away from.
const MOUSE_WAIT_STEPS: u32 = 32;
const MOUSE_WAIT_STEP_MS: u32 = 2;

/// How long the OS-detection probe waits for the host to echo a lock-LED change
/// after a Caps Lock tap, and how often it looks. 400 ms is well past the
/// round trip a host that is going to answer takes (a few poll intervals), so a
/// full timeout is a real "no echo" rather than an impatient one -- which is the
/// signal that separates macOS from the rest.
const OS_PROBE_MS: u32 = 400;
const OS_PROBE_STEP_MS: u32 = 2;
/// The Caps Lock HID keycode, the one lock key the probe toggles.
const CAPSLOCK_KEYCODE: u8 = 0x39;

/// The task holding the HID mouse and the USB identity, or 0 for nobody.
///
/// These are the two things a task cannot be given a private copy of: there is
/// one interrupt endpoint and one set of descriptors, and changing the identity
/// re-enumerates the whole device. Splitting them would mean two scripts
/// fighting over what the badge *is* to the host, so instead the first script
/// to ask gets them and keeps them until it ends.
static mut USB_OWNER: usize = 0;

/// Claim the mouse and identity for `tid`, or report that someone else has them.
fn claim_usb(tid: usize) -> bool {
    // safety: single hart, and no switch happens inside this function.
    unsafe {
        let owner = USB_OWNER;
        if owner == 0 || owner == tid || !sched::used(owner) {
            USB_OWNER = tid;
            true
        } else {
            false
        }
    }
}

fn release_usb(tid: usize) {
    // safety: as `claim_usb`.
    unsafe {
        if USB_OWNER == tid {
            USB_OWNER = 0;
        }
    }
}

/// Run a compiled script to completion and record how it went.
///
/// Called on the task's own stack by [`crate::sched::spawn`]'s trampoline; the
/// program was parsed by whoever asked for the spawn.
pub fn run_task(tid: usize, script: &pycon::Script, seed: u32) {
    let mut host = BadgeHost::new(tid, seed);
    // Formatted into a fixed buffer rather than a `String`, because the error
    // most worth reading is the one raised by `heap_pressure` -- and asking the
    // allocator for a message about running out of memory is a poor plan.
    let mut msg = FmtBuf::<96>::new();
    let ended = match script.run(&mut host) {
        Ok(pycon::Completion::Finished) => Ended::Finished,
        Ok(pycon::Completion::Aborted) => Ended::Stopped,
        Err(e) => {
            let _ = msg.format(format_args!("{}", e));
            Ended::Failed
        }
    };
    // Before recording the outcome, so the mouse button a killed script was
    // holding is released while the slot still looks alive.
    drop(host);
    sched::finish(tid, ended, msg.as_str());
}

pub struct BadgeHost {
    /// Which task this is. Everything shared is decided by it: whose page to
    /// draw into, whether the keys are ours, whether the mouse is ours.
    tid: usize,
    /// This task's off-screen page, borrowed from the scheduler for the life of
    /// the task. Raw because it is reached from two stacks -- this one, and the
    /// compositor's, which only ever reads it while this task is suspended.
    page: *mut Fb,
    rng: Rng,
    /// How many consecutive checks have seen the kill chord.
    kill_streak: u8,
    /// Raw key mask at the previous sample, for edge detection in `wait_key`.
    last_raw: u8,
    /// Set once the task has been told to stop, so every later call gives up
    /// immediately instead of the script getting a chance to ignore one
    /// `Abort`.
    stopped: bool,
    /// Mouse buttons the script is holding down. Kept here rather than in the
    /// USB module because it is script state: a move has to carry the buttons
    /// that were already down, or every drag would drop halfway through.
    buttons: u8,
    /// Whether this task has pressed any key. Unlike the mouse buttons, the
    /// keyboard's held-key state lives in the USB module (the report is a
    /// bitmap, not a byte), so this is just the flag that says "there may be
    /// something to release on the way out".
    kbd_active: bool,
}

impl BadgeHost {
    pub fn new(tid: usize, seed: u32) -> Self {
        BadgeHost {
            tid,
            page: sched::fb_ptr(tid),
            rng: Rng::new(seed),
            kill_streak: 0,
            last_raw: 0,
            stopped: false,
            buttons: 0,
            kbd_active: false,
        }
    }

    /// The page this task draws into.
    #[inline]
    fn fb(&mut self) -> &mut dyn FrameBuffer {
        // safety: the pointer came from the scheduler's table for this task's
        // own slot, which outlives the task, and no other task writes it.
        unsafe { &mut *self.page }
    }

    /// Queue one HID report, waiting for the endpoint if it is still busy with
    /// the previous one.
    ///
    /// Keeps ticking while it waits, so USB stays serviced -- which it has to,
    /// because the thing being waited for is a USB completion -- and so the
    /// kill chord still works.
    fn report(&mut self, dx: i8, dy: i8, wheel: i8) -> Result<bool, Abort> {
        if !claim_usb(self.tid) {
            return Ok(false);
        }
        for _ in 0..MOUSE_WAIT_STEPS {
            self.service()?;
            if !usb::hid::is_ready() {
                return Ok(false);
            }
            if usb::hid::send(self.buttons, dx, dy, wheel) {
                return Ok(true);
            }
            sched::nap(MOUSE_WAIT_STEP_MS);
        }
        Ok(false)
    }

    /// Queue the keyboard's current report, waiting for the interrupt endpoint
    /// to drain first, exactly as [`Self::report`] does for the mouse.
    ///
    /// The wait is load-bearing for typing: the release is not queued until the
    /// host has collected the press, so no two keystrokes are ever coalesced
    /// into a single poll and every character registers.
    fn kbd_send(&mut self) -> Result<bool, Abort> {
        if !claim_usb(self.tid) {
            return Ok(false);
        }
        self.kbd_active = true;
        for _ in 0..MOUSE_WAIT_STEPS {
            self.service()?;
            if !usb::kbd::is_ready() {
                return Ok(false);
            }
            if usb::kbd::send() {
                return Ok(true);
            }
            sched::nap(MOUSE_WAIT_STEP_MS);
        }
        Ok(false)
    }

    /// Best-effort OS fingerprint: the Caps Lock LED trick for macOS, and what
    /// the host asked for while enumerating for Windows.
    ///
    /// The mechanisms are sound; the classification is a heuristic, and the two
    /// deserve to be told apart:
    ///
    /// * **Mechanism.** When a keyboard sends a lock-key press, the host toggles that lock's global state and
    ///   pushes the new LED state back to every keyboard as an output report -- which `kbd::on_host_leds`
    ///   records and `kbd::led_events` counts. Separately, a host that asked for string descriptor 0xEE
    ///   during enumeration is remembered by `proto`. Both are directly observable from the device.
    ///
    /// * **Heuristic.** What they *mean* is inferred. No echo to a brief Caps Lock tap is macOS's tell: it
    ///   does not toggle Caps Lock on a momentary HID tap the way Windows and Linux do. Among hosts that
    ///   echo, asking for 0xEE -- the Microsoft OS string descriptor -- is Windows's tell, since no other
    ///   host has a reason to request it.
    ///
    /// The 0xEE question replaces the NumLock-at-enumeration guess this used to
    /// make. That guess was really "is NumLock on", not "which OS": Linux hosts
    /// push their lock state down to a keyboard the moment it binds, so any
    /// Linux desktop that keeps NumLock on -- most of them -- read as Windows.
    ///
    /// The one thing 0xEE gives up is the repeat visit. Windows caches the
    /// answer per vendor/product/revision and does not ask twice, so a badge
    /// replugged into a PC that has already catalogued it falls through to
    /// `OS_LINUX`. `usb_id()` is the way out: a vendor/product pair that host
    /// has not seen makes it ask again.
    fn detect_os_impl(&mut self) -> Result<i32, Abort> {
        if !claim_usb(self.tid) || !usb::kbd::is_ready() {
            return Ok(pycon::host::OS_UNKNOWN);
        }
        self.kbd_active = true;

        // Sampled from this enumeration, before anything is perturbed.
        let ms_os_probed = usb::ms_os_probed();

        // Tap Caps Lock and watch for the host to echo a lock-LED report.
        let before = usb::kbd::led_events();
        self.kbd_key(CAPSLOCK_KEYCODE, true)?;
        self.kbd_key(CAPSLOCK_KEYCODE, false)?;
        let echoed = self.wait_led_change(before, OS_PROBE_MS)?;

        // Put Caps Lock back the way it was found, so the probe leaves no trace
        // on the user's session -- but only if it actually moved.
        if echoed {
            let restore_from = usb::kbd::led_events();
            self.kbd_key(CAPSLOCK_KEYCODE, true)?;
            self.kbd_key(CAPSLOCK_KEYCODE, false)?;
            let _ = self.wait_led_change(restore_from, OS_PROBE_MS)?;
        }

        Ok(if !echoed {
            pycon::host::OS_MAC
        } else if ms_os_probed {
            pycon::host::OS_WINDOWS
        } else {
            pycon::host::OS_LINUX
        })
    }

    /// Wait up to `timeout_ms` for the host's LED-report counter to move past
    /// `since`, servicing USB throughout. Returns whether it moved.
    fn wait_led_change(&mut self, since: u32, timeout_ms: u32) -> Result<bool, Abort> {
        let start = platform::now_ms();
        loop {
            self.service()?;
            if usb::kbd::led_events() != since {
                return Ok(true);
            }
            if platform::elapsed(start, timeout_ms) {
                return Ok(false);
            }
            sched::nap(OS_PROBE_STEP_MS);
        }
    }

    /// One scheduling point: hand the CPU around, service USB, sample the keys,
    /// and decide whether this task has been told to stop.
    ///
    /// Everything that blocks funnels through here, which is what makes the
    /// yield unconditional -- a script cannot arrange to skip it.
    fn service(&mut self) -> Result<u8, Abort> {
        // Gives USB a pass and advances the clock even when nothing else is
        // runnable, so this is still the full "keep the badge alive" call it
        // was before there was anything to switch to.
        sched::yield_now();

        if self.stopped || sched::killed(self.tid) {
            self.stopped = true;
            return Err(Abort);
        }

        // The keys belong to whoever is on screen. A background task reading
        // them would mean two scripts reacting to one press -- and the exit
        // chord, held to stop the script you are looking at, would stop every
        // script at once.
        if !sched::is_focused(self.tid) {
            self.kill_streak = 0;
            return Ok(0);
        }

        let raw = input::scan_raw();
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

impl Host for BadgeHost {
    fn tick_interval(&self) -> u32 {
        // Lower than the crate default. A script's inner loop is usually a few
        // dozen steps, so this checks the exit chord several times per frame
        // while adding a rounding error's worth of overhead to real work. It is
        // also the scheduling quantum: a script that does nothing but compute
        // still hands the badge around this often.
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
        //
        // With several scripts sharing one heap this is also how they are held
        // apart: the reserve is not divided between them, so whichever one asks
        // for the allocation that would cross the line is the one that fails.
        // That is rough justice -- it need not be the greedy one -- but it is
        // the only rule that does not require a per-task quota nobody could
        // pick a number for.
        platform::heap_free() < platform::HEAP_RESERVE
    }

    fn print_line(&mut self, s: &str) {
        // Prefixed, because with several scripts running the console is shared
        // and unlabelled output is ambiguous.
        crate::println!("{}| {}", sched::name(self.tid), s);
    }

    fn gfx_clear(&mut self) { self.fb().clear(); }

    fn gfx_pixel(&mut self, x: i32, y: i32, on: bool) {
        // `put_pixel` clips on its own, so out-of-range values are simply
        // dropped rather than clamped onto the edge -- a clamped pixel would
        // draw a spurious line down the side of the screen.
        let color = Self::color(on);
        self.fb().put_pixel(Point::new(x as isize, y as isize), color);
    }

    fn gfx_text(&mut self, x: i32, y: i32, s: &str, on: bool) {
        let fg = Self::color(on);
        let fb = self.fb();
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
        let fb = self.fb();
        if fill {
            gfx::fill_rect(fb, Point::new(ax, ay), Point::new(bx, by), color);
        } else {
            gfx::stroke_rect(fb, Point::new(ax, ay), Point::new(bx, by), color);
        }
    }

    fn gfx_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let color = Self::color(true);
        let (a, b) = (
            Point::new(Self::clamp(x0, COLUMN), Self::clamp(y0, ROW)),
            Point::new(Self::clamp(x1, COLUMN), Self::clamp(y1, ROW)),
        );
        gfx::line(self.fb(), a, b, color);
    }

    fn gfx_show(&mut self) -> Result<(), Abort> {
        // Nothing here touches the panel: the page is offered to the compositor
        // and this task waits its turn. The wait is the point -- it is where a
        // script's frame rate comes from, and where the other tasks get the
        // milliseconds this one used to spend blocking on the SPI bus.
        sched::show(self.tid);
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
        sched::set_status(self.tid, Status::Key);
        let out = loop {
            sched::nap(WAIT_STEP_MS);
            let raw = self.service()?;
            let pressed = raw & !self.last_raw;
            self.last_raw = raw;
            if pressed != 0 {
                break pressed as u32;
            }
            // An unfocused task sees an empty matrix, so this is where a
            // background script waiting on a key parks: it costs one scan every
            // couple of milliseconds until someone brings it to the front.
        };
        sched::set_status(self.tid, Status::Run);
        Ok(out)
    }

    fn sleep_ms(&mut self, ms: u32) -> Result<(), Abort> {
        // Broken into short slices so a `sleep(10000)` is still interruptible,
        // and measured against the clock rather than by counting delays --
        // several tasks cannot each spin on the timer's one sticky flag. See
        // `platform::tick_clock`.
        let start = platform::now_ms();
        sched::set_status(self.tid, Status::Sleep);
        let out = loop {
            if let Err(e) = self.service() {
                break Err(e);
            }
            if platform::elapsed(start, ms) {
                break Ok(());
            }
            sched::nap(1);
        };
        sched::set_status(self.tid, Status::Run);
        out
    }

    fn random(&mut self) -> u32 { self.rng.next() }

    fn mouse_ready(&mut self) -> bool {
        usb::poll();
        claim_usb(self.tid) && usb::hid::is_ready()
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
        if !claim_usb(self.tid) {
            return Ok(false);
        }
        // Give the kill chord a look before a re-enumeration takes the bus down
        // for the better part of a second, and pump the loop once after so the
        // fresh controller is serviced before the script runs on. Every other
        // task loses the drive for that time too, which is the honest cost of
        // one script deciding what the badge is.
        self.service()?;
        let ok = usb::set_identity(vid, pid);
        self.service()?;
        Ok(ok)
    }

    fn usb_set_name(&mut self, name: &str) -> Result<bool, Abort> {
        if !claim_usb(self.tid) {
            return Ok(false);
        }
        self.service()?;
        usb::set_product_name(name);
        self.service()?;
        Ok(true)
    }

    fn kbd_ready(&mut self) -> bool {
        usb::poll();
        claim_usb(self.tid) && usb::kbd::is_ready()
    }

    fn kbd_key(&mut self, code: u8, down: bool) -> Result<bool, Abort> {
        if !claim_usb(self.tid) {
            return Ok(false);
        }
        // Edit the held-key set before sending, not after: if the report is
        // dropped because nothing is listening, the script's idea of what is
        // held should still be what it asked for, so the next report that does
        // go out carries it -- the same rule the mouse buttons follow.
        if down {
            usb::kbd::key_down(code);
        } else {
            usb::kbd::key_up(code);
        }
        self.kbd_send()
    }

    fn kbd_modifiers(&mut self, mask: u8) -> Result<bool, Abort> {
        if !claim_usb(self.tid) {
            return Ok(false);
        }
        usb::kbd::set_modifiers(mask);
        self.kbd_send()
    }

    fn kbd_release_all(&mut self) -> Result<bool, Abort> {
        if !claim_usb(self.tid) {
            return Ok(false);
        }
        usb::kbd::release_all();
        self.kbd_send()
    }

    fn kbd_leds(&mut self) -> u32 {
        usb::poll();
        usb::kbd::host_leds() as u32
    }

    fn detect_os(&mut self) -> Result<i32, Abort> { self.detect_os_impl() }

    fn badgy_art(&mut self, frame: i32) -> Option<Vec<String>> {
        let art = badgy::frame_art(frame, badgy_tick())?;
        // Rendered back out as the same `#`/`.`/space rows a script would write
        // by hand, rather than as some packed form: what comes out of here goes
        // straight back into `sprite()` after the script has painted on it, and
        // a format that only round-trips through the firmware would make that a
        // trick rather than an obvious thing to do.
        let (w, h) = (art.width() as usize, art.height() as usize);
        let mut rows = Vec::with_capacity(h);
        for y in 0..h {
            let mut row = String::with_capacity(w);
            for x in 0..w {
                row.push(match art.pixel(x, y) {
                    gfx::INK => pycon::host::SPRITE_INK as char,
                    gfx::DARK => pycon::host::SPRITE_DARK as char,
                    _ => pycon::host::SPRITE_CLEAR as char,
                });
            }
            rows.push(row);
        }
        Some(rows)
    }

    fn badgy_define(&mut self, rows: &[&str]) -> i32 { mascot::define(self.tid, rows, None) }

    fn badgy_redefine(&mut self, slot: i32, rows: &[&str]) -> i32 {
        mascot::define(self.tid, rows, Some(slot))
    }

    fn badgy_draw(&mut self, x: i32, y: i32, frame: i32) -> bool {
        let Some(art) = badgy::frame_art(frame, badgy_tick()) else {
            return false;
        };
        let (lit, dark) = (Self::color(true), Self::color(false));
        // `put_pixel` clips, so an off-screen sprite costs one pass over its own
        // pixels and draws nothing -- the same deal `pixel()` gets.
        gfx::sprite(self.fb(), art, Point::new(x as isize, y as isize), lit, dark);
        true
    }

    fn badgy_mood(&mut self, a: i32, b: i32) -> bool { mascot::hold(self.tid, a, b) }

    fn badgy_say(&mut self, s: &str) -> bool { mascot::say(self.tid, s) }
}

/// The compositor's animation frame counter, near enough, from the clock.
///
/// A script's `badgy()` should breathe at the rate the home screen does, and the
/// counter that drives that lives in the UI task's `Badgy` -- on another stack,
/// and not advancing at all while a script is the thing on screen. Deriving it
/// from the millisecond clock instead costs a division and is right in both
/// cases.
fn badgy_tick() -> u32 { platform::now_ms() / FRAME_MS }

/// Roughly how long one of the compositor's frames takes, once the panel
/// refresh is paid for. See [`crate::app`] for where the number comes from.
const FRAME_MS: u32 = 45;

/// Let go of anything the script was still holding when it ended.
///
/// A script killed with the exit chord halfway through a drag would otherwise
/// leave the host believing the button is still down -- and nothing else ever
/// tells it otherwise, so the user's next click lands as the end of a selection
/// they did not start. The release is best-effort: if there is no host, there
/// is nothing holding anything.
impl Drop for BadgeHost {
    fn drop(&mut self) {
        release_usb(self.tid);
        // And the badger, for the same reason: a script that pinned him and then
        // hit the exit chord would otherwise leave the home screen holding a
        // pose nothing is maintaining.
        mascot::release(self.tid);

        // Let go of any keyboard keys the script was still holding, for the same
        // reason as the mouse buttons: a script killed mid-chord would otherwise
        // leave the host believing Ctrl (or any key) is still down, and nothing
        // else would ever tell it otherwise. Best-effort -- if there is no host,
        // there is nothing holding anything.
        if self.kbd_active && usb::kbd::any_down() {
            usb::kbd::release_all();
            for _ in 0..MOUSE_WAIT_STEPS {
                if !usb::kbd::is_ready() || usb::kbd::send() {
                    break;
                }
                sched::nap(MOUSE_WAIT_STEP_MS);
            }
        }

        if self.buttons == 0 {
            return;
        }
        self.buttons = 0;
        for _ in 0..MOUSE_WAIT_STEPS {
            if !usb::hid::is_ready() || usb::hid::send(0, 0, 0, 0) {
                return;
            }
            sched::nap(MOUSE_WAIT_STEP_MS);
        }
    }
}
