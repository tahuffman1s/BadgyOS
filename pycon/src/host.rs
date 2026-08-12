//! The seam between the interpreter and the badge.
//!
//! Everything a script can observe or affect goes through [`Host`]. The
//! firmware implements it over the OLED framebuffer and the key matrix; the
//! tests implement it over a `Vec<String>` of printed lines. That split is the
//! reason this crate builds and tests on a laptop.

use alloc::string::String;
use alloc::vec::Vec;

/// The script asked to stop, or the user asked for it to stop.
///
/// This is not an error in the script -- it is the firmware taking the badge
/// back -- so the interpreter unwinds without reporting a line number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Abort;

/// Width and height of the badge panel, in pixels. Scripts see these as the
/// `WIDTH` and `HEIGHT` builtins.
pub const SCREEN_W: i32 = 128;
pub const SCREEN_H: i32 = 128;

/// Key bits, matching the firmware's `input::Key` ordering so the two can be
/// cast to each other without a translation table.
pub const KEY_DOWN: u32 = 1 << 0;
pub const KEY_SELECT: u32 = 1 << 1;
pub const KEY_UP: u32 = 1 << 2;
pub const KEY_LEFT: u32 = 1 << 3;
pub const KEY_RIGHT: u32 = 1 << 4;
pub const KEY_CENTER: u32 = 1 << 5;

/// Mouse button bits, matching the HID report's button byte so the firmware can
/// pass them through without a translation table.
pub const MOUSE_LEFT: u32 = 1 << 0;
pub const MOUSE_RIGHT: u32 = 1 << 1;
pub const MOUSE_MIDDLE: u32 = 1 << 2;

/// The largest movement a single HID report can carry, in either direction.
///
/// Scripts are clamped to this rather than refused: a report is a relative
/// nudge, and "move 5000 pixels" is a request the transport cannot express at
/// all, so there is no correct number to fail with. Scripts that want a long
/// travel loop over short ones, which is also what a real mouse produces.
pub const MOUSE_MAX_STEP: i32 = 127;

/// The USB vendor and product id the badge presents before any script changes
/// it. Exposed to scripts as `USB_VID` / `USB_PID` so a script can read the
/// defaults and put the identity back the way it found it.
pub const USB_VID_DEFAULT: u16 = 0x1d50;
pub const USB_PID_DEFAULT: u16 = 0x6199;

// -------------------------------------------------------------------- the badger
//
// The badge has a mascot, and a script can both draw him and take him over. One
// integer names every frame there is: a mood the firmware already knows how to
// animate, or a slot the script filled in itself with `sprite()`. Keeping it to
// one `int` is what lets `badgy()`, `badgy_mood()` and `badgy_art()` all take
// "a frame" without the language needing a type to say it with.

/// Whatever the firmware would show right now. Also the value that *releases* a
/// script's hold on the mascot, which is why it is the zero: `badgy_mood(0)` is
/// "not mine any more", and a script that never asks for him is already there.
pub const BADGY_AUTO: i32 = 0;
pub const BADGY_IDLE: i32 = 1;
pub const BADGY_BLINK: i32 = 2;
pub const BADGY_SLEEP: i32 = 3;
pub const BADGY_DIG: i32 = 4;
pub const BADGY_PLUG: i32 = 5;
pub const BADGY_OOPS: i32 = 6;
/// Highest built-in mood id. Ids above this and below [`SPRITE_SLOT_BASE`] are
/// unused, so a mood added later does not renumber anybody's slots.
pub const BADGY_MOOD_MAX: i32 = BADGY_OOPS;

/// First id handed out by `sprite()`. Slots are `SPRITE_SLOT_BASE ..
/// SPRITE_SLOT_BASE + SPRITE_SLOTS`.
pub const SPRITE_SLOT_BASE: i32 = 16;

/// What `sprite()` returns when there was nowhere to put the art: every slot is
/// taken, or this host has no badger at all. Not an error, for the same reason
/// [`Host::mouse_move`] returning `false` is not one -- a script that draws a
/// frame nobody can show is still a script that runs.
pub const SPRITE_NONE: i32 = -1;

/// How many script-supplied frames the badge holds at once. Four, because two
/// is the smallest animation and a script that wants a cycle of them should not
/// have to choose between animating and having a spare.
pub const SPRITE_SLOTS: usize = 4;

/// Largest script-supplied frame, in pixels. Badgy himself is 72x74, and the
/// panel is 128 wide with a title band above and a caption band below, so a
/// frame bigger than this could not be shown where he stands anyway.
pub const SPRITE_MAX_W: usize = 80;
pub const SPRITE_MAX_H: usize = 80;

/// The three pixel states a sprite row is written with: lit, black, and leave
/// alone. The third is what lets the badger sit on top of a live background.
pub const SPRITE_INK: u8 = b'#';
pub const SPRITE_DARK: u8 = b'.';
pub const SPRITE_CLEAR: u8 = b' ';

/// Longest caption a script can put under the badger, in characters -- the 6x12
/// font gives 21 across a 128-pixel panel.
pub const BADGY_CAPTION_MAX: usize = 21;

pub trait Host {
    /// How many interpreter steps to run between [`Host::tick`] calls.
    ///
    /// Small enough that a tight `while True: pass` still notices the user
    /// holding the exit button within a few milliseconds; large enough that the
    /// check does not dominate the run time of real work. A method rather than
    /// an associated const because the interpreter holds a `dyn Host`.
    fn tick_interval(&self) -> u32 { 2048 }

    /// Called periodically while a script runs. This is where the firmware
    /// services USB and samples the keys, and where it decides to stop a
    /// runaway script.
    fn tick(&mut self) -> Result<(), Abort>;

    /// Is the heap nearly gone?
    ///
    /// Individual limits (list length, string length, recursion depth) each
    /// bound one way of using memory, and between them they miss the general
    /// case: a script that simply keeps a lot of small things alive. The
    /// interpreter asks this alongside every `tick` and turns a `true` into an
    /// ordinary script error, which is a message on the panel rather than an
    /// allocation failure -- and an allocation failure on this device is a
    /// panic, and a panic spins until someone pulls the power.
    ///
    /// The default is `false`, for hosts with a real operating system under
    /// them. The firmware asks its allocator.
    fn heap_pressure(&self) -> bool { false }

    /// `print(...)` -- one complete line, without the trailing newline.
    fn print_line(&mut self, s: &str);

    // ------------------------------------------------------------------ drawing
    //
    // Coordinates are in pixels with the origin top-left. Implementations are
    // expected to clip rather than panic: a script that draws off-screen is
    // making a mistake, not an attack, and killing it would be unfriendly.

    /// Blank the framebuffer.
    fn gfx_clear(&mut self);

    /// Set or clear a single pixel. `on == false` erases.
    fn gfx_pixel(&mut self, x: i32, y: i32, on: bool);

    /// Draw text with its top-left corner at `(x, y)`. `on == false` draws it
    /// in the background colour, which is how a script writes over a filled
    /// rectangle -- a title bar, a selected row.
    fn gfx_text(&mut self, x: i32, y: i32, s: &str, on: bool);

    /// A rectangle from `(x0, y0)` to `(x1, y1)` inclusive, outlined or filled.
    fn gfx_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, fill: bool);

    /// A straight line between two points.
    fn gfx_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32);

    /// Push the framebuffer to the panel. Nothing a script draws is visible
    /// until it calls this -- the panel update is the single most expensive
    /// thing the badge does, so it is never implicit.
    fn gfx_show(&mut self) -> Result<(), Abort>;

    // ------------------------------------------------------------------- input

    /// Keys held right now, as a bitmask of the `KEY_*` constants.
    fn keys(&mut self) -> u32;

    /// Block until a key is pressed, and return it. Must keep ticking while it
    /// waits, so USB stays alive and the script stays killable.
    fn wait_key(&mut self) -> Result<u32, Abort>;

    /// Pause for roughly `ms` milliseconds, ticking throughout.
    fn sleep_ms(&mut self, ms: u32) -> Result<(), Abort>;

    /// A pseudo-random 32-bit word. Not cryptographic.
    fn random(&mut self) -> u32;

    // ------------------------------------------------------------------- mouse
    //
    // The badge can present itself to whatever it is plugged into as a USB
    // mouse. These three are the whole of it, and they all default to "there is
    // no mouse here" so that a host without one -- the test bench, a future
    // build without the HID interface -- needs no code to say so.

    /// Is there a host that would receive a mouse report? False before the
    /// badge is plugged in, or while whatever it is plugged into has not
    /// configured the HID interface.
    fn mouse_ready(&mut self) -> bool { false }

    /// Move the pointer by `(dx, dy)` and the wheel by `wheel`, all relative,
    /// all already clamped to [`MOUSE_MAX_STEP`]. `dy` is positive downward,
    /// which is the HID convention.
    ///
    /// Returns whether the report reached the host. `Ok(false)` is the ordinary
    /// answer when nothing is listening; it is not an error, because a jiggler
    /// left running while the cable is out is doing exactly what it should.
    fn mouse_move(&mut self, dx: i8, dy: i8, wheel: i8) -> Result<bool, Abort> {
        let _ = (dx, dy, wheel);
        Ok(false)
    }

    /// Set which buttons are held, as a mask of the `MOUSE_*` constants, and
    /// report the change. Holding is separate from clicking because a drag
    /// needs the button to stay down across several moves.
    fn mouse_buttons(&mut self, mask: u8) -> Result<bool, Abort> {
        let _ = mask;
        Ok(false)
    }

    // ----------------------------------------------------------- usb identity
    //
    // What the badge looks like to whatever it is plugged into: its USB vendor
    // id, product id and product name. A script reads them with `usb_vid()` /
    // `usb_pid()` and changes them with `usb_id()` / `usb_name()`. Changing an
    // identity re-presents the device to the host, which is the only way a host
    // that already enumerated will notice -- so these are the heaviest calls in
    // the API, and a script sets them once at startup, not in a loop.
    //
    // The defaults report the badge's own identity and refuse every change, so
    // a host with no USB (the test bench) needs no code to say "not supported".

    /// The vendor and product id the device is presenting right now.
    fn usb_ids(&mut self) -> (u16, u16) { (USB_VID_DEFAULT, USB_PID_DEFAULT) }

    /// Present the device under vendor id `vid` and product id `pid`, and
    /// re-enumerate so a host sees it. Returns whether the change was applied;
    /// false means it was refused (an id reserved for the bootloader) or there
    /// is no USB to change.
    fn usb_set_identity(&mut self, vid: u16, pid: u16) -> Result<bool, Abort> {
        let _ = (vid, pid);
        Ok(false)
    }

    /// Set the product name the host shows for the device, and re-enumerate.
    /// An empty name restores the default. Returns whether it was applied.
    fn usb_set_name(&mut self, name: &str) -> Result<bool, Abort> {
        let _ = name;
        Ok(false)
    }

    // ------------------------------------------------------------- the badger
    //
    // Badgy is the one part of the firmware that is *for* being looked at, so a
    // script that can drive the screen but not him can only ever borrow the
    // badge -- it cannot leave a mark on the thing the badge shows when nobody
    // is doing anything. These five calls are that mark: read a frame, make a
    // frame, draw a frame, pin the mascot to one, and give him something to
    // say.
    //
    // Like the mouse, they all default to "there is no badger here", so the
    // test bench needs no code to say so and a script written against them
    // still runs to the end on a host that has none.

    /// The rows of `frame`, in the `#`/`.`/space form `badgy_define` takes.
    ///
    /// This exists so a script can *start from* the badger rather than draw one:
    /// 72x74 of hand-typed art is more than fits in a script, and a mascot that
    /// can only be replaced wholesale is one nobody will touch. Read the idle
    /// frame, paint something into it, hand it back.
    ///
    /// `None` for a frame this host does not have, including an empty slot.
    fn badgy_art(&mut self, frame: i32) -> Option<Vec<String>> {
        let _ = frame;
        None
    }

    /// Take a copy of `rows` and return the frame id it can be reached by, or
    /// [`SPRITE_NONE`] if there was no room for it.
    ///
    /// The copy is the point: the rows a script passes are on the interpreter's
    /// heap and go away with the script, and the firmware needs the art to
    /// outlive the call that drew it -- the mascot is composited by something
    /// else entirely, long after the script has gone back to sleep.
    ///
    /// Rows are already known to be well-formed (three legal characters, inside
    /// [`SPRITE_MAX_W`] by [`SPRITE_MAX_H`]) by the time they get here; that is
    /// a script error and is caught where a line number is still available.
    fn badgy_define(&mut self, rows: &[&str]) -> i32 {
        let _ = rows;
        SPRITE_NONE
    }

    /// Overwrite a slot this script already owns, returning its id or
    /// [`SPRITE_NONE`] if it belongs to someone else.
    ///
    /// Without this an animation would be a slot leak: a loop that builds a
    /// fresh frame each pass would take a new slot each pass and run out on the
    /// fifth, having only ever needed one.
    fn badgy_redefine(&mut self, slot: i32, rows: &[&str]) -> i32 {
        let _ = (slot, rows);
        SPRITE_NONE
    }

    /// Draw `frame` into this script's page with its top-left at `(x, y)`.
    /// Returns whether there was such a frame to draw.
    fn badgy_draw(&mut self, x: i32, y: i32, frame: i32) -> bool {
        let _ = (x, y, frame);
        false
    }

    /// Hold the mascot on `a`, alternating with `b` if the two differ, until the
    /// script releases him with [`BADGY_AUTO`] or ends.
    ///
    /// Returns whether the hold took. `false` means another script has him --
    /// there is one badger and, as with the mouse, the first to ask keeps him.
    fn badgy_mood(&mut self, a: i32, b: i32) -> bool {
        let _ = (a, b);
        false
    }

    /// Put `s` under the badger, or restore his own lines if it is empty.
    /// Truncated to [`BADGY_CAPTION_MAX`] rather than refused: a caption one
    /// character too long is a layout problem, not a program that should stop.
    fn badgy_say(&mut self, s: &str) -> bool {
        let _ = s;
        false
    }
}

/// A [`Host`] that draws nowhere and records what was printed. Useful for tests
/// and for a dry-run of a script's syntax and control flow.
#[derive(Default)]
pub struct NullHost {
    pub output: alloc::vec::Vec<String>,
    /// Set to make the next `tick` abort, to exercise the cancel path.
    pub stop: bool,
    /// Counts `tick` calls, so a test can assert the interpreter really does
    /// yield during a long loop.
    pub ticks: u32,
    seed: u32,
}

impl Host for NullHost {
    fn tick(&mut self) -> Result<(), Abort> {
        self.ticks += 1;
        if self.stop { Err(Abort) } else { Ok(()) }
    }

    fn print_line(&mut self, s: &str) { self.output.push(String::from(s)); }

    fn gfx_clear(&mut self) {}

    fn gfx_pixel(&mut self, _x: i32, _y: i32, _on: bool) {}

    fn gfx_text(&mut self, _x: i32, _y: i32, _s: &str, _on: bool) {}

    fn gfx_rect(&mut self, _x0: i32, _y0: i32, _x1: i32, _y1: i32, _fill: bool) {}

    fn gfx_line(&mut self, _x0: i32, _y0: i32, _x1: i32, _y1: i32) {}

    fn gfx_show(&mut self) -> Result<(), Abort> { self.tick() }

    fn keys(&mut self) -> u32 { 0 }

    fn wait_key(&mut self) -> Result<u32, Abort> { Err(Abort) }

    fn sleep_ms(&mut self, _ms: u32) -> Result<(), Abort> { self.tick() }

    fn random(&mut self) -> u32 {
        // xorshift32, so tests are deterministic.
        let mut x = if self.seed == 0 { 0x1379_2a5b } else { self.seed };
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.seed = x;
        x
    }
}
