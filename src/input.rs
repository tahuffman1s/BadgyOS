//! The jog wheel and the three face buttons, read as a 2x3 GPIO matrix.
//!
//! # What is actually wired up
//!
//! From `dc34-core-hw`'s netlist (`ux.kicad_sch`; SW2 is a Haoyu TS-1513B,
//! described in the library as a "1-D directional switch with center press" --
//! that is the wheel on the side of the badge):
//!
//! ```text
//!                KB_C0 (PF2)     KB_C1 (PF3)      KB_C2 (PF4)
//!   KB_R0 (PF6)  SW2 CCW         SW2 PUSH         SW2 CW          <- the wheel
//!   KB_R1 (PF7)  SW5             SW3              SW4 (via Q4)    <- face buttons
//! ```
//!
//! Rows idle high and are pulled low one at a time; a pressed key drags its
//! column down against the pad's internal pull-up (the IOX pull-up register
//! resets to all-ones -- `apb_cr #(... .IV(16'hffff)) sfr_gpiopu` in
//! `baochip-1x/rtl/modules/ifsub/rtl/iox.sv:158` -- but we set it explicitly so
//! this does not depend on what boot1 left behind).
//!
//! SW4 does not short its row to its column directly: it drives the gate of Q4,
//! whose source/drain sit on KB_R1/KB_C2, so it still reads as `(row 1, col 2)`.
//!
//! # Why this does not just call `bao1x_hal::board::scan_keyboard`
//!
//! Three reasons. It `println!`s the raw row/column of every press, which would
//! flood the console; it re-runs `setup_kb_pins` on every call; and its row-1
//! mapping does not match the hardware. Upstream has `(1, 0) => Right`,
//! `(1, 2) => Center` and `(1, 3) => Left`, but there is no column 3 -- so
//! `Left` is unreachable, `(1, 1)` falls through to `Invalid`, and `Right` comes
//! out of SW5's position. The keypad controller's own decode
//! (`kpc_sr0_to_key`, bit positions 4/5/6 for row 1) agrees with the netlist:
//! SW5 is Left, SW3 is Right, SW4 is Center. That is what we use.

use bao1x_api::{IoSetup, IoxDir, IoxDriveStrength, IoxEnable, IoxFunction, IoxPort, IoxValue};
use bao1x_hal::iox::Iox;
use utralib::utra;

const KB_PORT: IoxPort = IoxPort::PF;
const ROW_PINS: [u8; 2] = [6, 7];
const COL_PINS: [u8; 3] = [2, 3, 4];

/// Polls before a held key starts repeating, and the gap between repeats.
/// Counted in [`Keys::poll`] calls, which the main loop spaces roughly a
/// millisecond apart -- exact enough for a key-repeat feel.
const REPEAT_DELAY: u16 = 420;
const REPEAT_PERIOD: u16 = 90;

/// Which keys auto-repeat: the wheel's two directions and nothing else.
/// Scrolling a long list by holding the wheel over is useful; every other key
/// on this badge selects or cancels, and those must happen once per press.
const REPEATS: u8 = Key::Up.bit() | Key::Down.bit();

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum Key {
    /// Wheel rolled one detent "down" the list (SW2 CCW).
    Down = 0,
    /// Wheel pressed in (SW2 PUSH).
    Select = 1,
    /// Wheel rolled one detent "up" the list (SW2 CW).
    Up = 2,
    Left = 3,
    Right = 4,
    Center = 5,
}

pub const KEY_COUNT: usize = 6;
pub const ALL_KEYS: [Key; KEY_COUNT] = [Key::Down, Key::Select, Key::Up, Key::Left, Key::Right, Key::Center];

impl Key {
    #[inline]
    pub const fn bit(self) -> u8 { 1 << self as u8 }

    pub const fn name(self) -> &'static str {
        match self {
            Key::Down => "WHEEL DN",
            Key::Select => "WHEEL IN",
            Key::Up => "WHEEL UP",
            Key::Left => "LEFT",
            Key::Right => "RIGHT",
            Key::Center => "CENTER",
        }
    }
}

/// A set of keys, as a bitmask indexed by [`Key`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub struct KeySet(pub u8);

impl KeySet {
    #[inline]
    pub fn has(self, k: Key) -> bool { self.0 & k.bit() != 0 }

    #[inline]
    pub fn any(self) -> bool { self.0 != 0 }
}

pub struct Keys {
    iox: Iox,
    /// Debounced state: what we believe is physically held right now.
    stable: u8,
    /// Previous raw sample, for the two-in-a-row debounce.
    last_raw: u8,
    /// Per-key count of consecutive polls held, driving auto-repeat.
    held_polls: [u16; KEY_COUNT],
    flipped: bool,
}

impl Keys {
    /// Claims PF2/PF3/PF4 (columns) and PF6/PF7 (rows).
    ///
    /// PF7 is shared with JTAG TDI and a test point, and PF5 -- the USB SE0
    /// switch -- sits between the two groups but is left alone.
    pub fn new() -> Self {
        let iox = Iox::new(utra::iox::HW_IOX_BASE as *mut u32);
        for r in ROW_PINS {
            iox.setup_pin(
                KB_PORT,
                r,
                Some(IoxDir::Output),
                Some(IoxFunction::Gpio),
                None,
                None,
                Some(IoxEnable::Enable),
                Some(IoxDriveStrength::Drive2mA),
            );
            iox.set_gpio_pin(KB_PORT, r, IoxValue::High);
        }
        for c in COL_PINS {
            iox.setup_pin(
                KB_PORT,
                c,
                Some(IoxDir::Input),
                Some(IoxFunction::Gpio),
                Some(IoxEnable::Enable), // schmitt trigger
                Some(IoxEnable::Enable), // pull-up: the only thing holding the column high
                Some(IoxEnable::Enable), // slow slew
                Some(IoxDriveStrength::Drive2mA),
            );
        }
        Keys { iox, stable: 0, last_raw: 0, held_polls: [0; KEY_COUNT], flipped: false }
    }

    /// Mirror up/down and left/right, to match a vertically flipped panel.
    pub fn set_flipped(&mut self, flipped: bool) { self.flipped = flipped; }

    /// Adopt the current physical state as the debounced state, without
    /// reporting anything as newly pressed.
    ///
    /// Needed after something else has owned the keys for a while -- a running
    /// script, which samples them through [`Keys::scan_raw`] and leaves this
    /// debouncer untouched. Without it, a key that was already down when the
    /// script exited looks like a fresh press to the very next `poll()`, and
    /// the screen the script left behind is dismissed before it can be read.
    pub fn resync(&mut self) {
        let raw = self.scan();
        self.stable = raw;
        self.last_raw = raw;
        self.held_polls = [0; KEY_COUNT];
    }

    /// Keys physically held right now, debounced.
    pub fn held(&self) -> KeySet { KeySet(self.remap(self.stable)) }

    /// One undebounced sample of the matrix, for callers that are not the UI.
    ///
    /// A running script polls the keys far more often than the UI does, and
    /// [`Keys::poll`] counts its debounce and auto-repeat in calls -- so
    /// letting the interpreter drive it would make `REPEAT_DELAY` mean
    /// something different depending on how busy the script was, and the menu
    /// would feel different after running one. This leaves that state alone.
    pub fn scan_raw(&self) -> u8 { self.remap(self.scan()) }

    /// Sample the matrix once and return the keys that "fired" -- pressed since
    /// the last poll, or repeating because they are being held.
    ///
    /// Call this on a steady cadence; the debounce and the repeat timing are
    /// both counted in calls, not in milliseconds.
    pub fn poll(&mut self) -> KeySet {
        let raw = self.scan();
        let mut fired = 0u8;

        // A sample only counts once it has been seen twice running. The switches
        // bounce for a couple of milliseconds and the poll interval is about
        // that, so this is a cheap and sufficient filter.
        if raw == self.last_raw && raw != self.stable {
            fired |= raw & !self.stable;
            self.stable = raw;
        }
        self.last_raw = raw;

        for (i, count) in self.held_polls.iter_mut().enumerate() {
            if self.stable & (1 << i) != 0 {
                *count = count.saturating_add(1);
                // Only the wheel's two directions repeat. A repeating Select
                // would re-activate whatever the user landed on while they were
                // still leaning on the button -- which is exactly what happens
                // after a hold-to-act screen: holding past the threshold exits
                // to the menu, and the repeat then re-opens the screen they
                // just left. Nobody has ever wanted a held "OK" to mean "OK,
                // OK, OK".
                if REPEATS & (1 << i) != 0
                    && *count > REPEAT_DELAY
                    && (*count - REPEAT_DELAY) % REPEAT_PERIOD == 0
                {
                    fired |= 1 << i;
                }
            } else {
                *count = 0;
            }
        }

        KeySet(self.remap(fired))
    }

    /// One full pass over the matrix. Returns a raw, un-remapped bitmask.
    fn scan(&self) -> u8 {
        let mut mask = 0u8;
        for (row_idx, row_pin) in ROW_PINS.iter().enumerate() {
            self.iox.set_gpio_pin(KB_PORT, *row_pin, IoxValue::Low);
            settle();
            // One bank read picks up all three columns at the same instant.
            let bank = self.iox.get_gpio_bank(KB_PORT);
            self.iox.set_gpio_pin(KB_PORT, *row_pin, IoxValue::High);

            for (col_idx, col_pin) in COL_PINS.iter().enumerate() {
                if bank & (1 << col_pin) == 0 {
                    mask |= 1 << (row_idx * COL_PINS.len() + col_idx);
                }
            }
        }
        mask
    }

    fn remap(&self, mask: u8) -> u8 {
        if !self.flipped {
            return mask;
        }
        let swap = |m: u8, a: Key, b: Key| -> u8 {
            let mut out = m & !(a.bit() | b.bit());
            if m & a.bit() != 0 {
                out |= b.bit();
            }
            if m & b.bit() != 0 {
                out |= a.bit();
            }
            out
        };
        swap(swap(mask, Key::Up, Key::Down), Key::Left, Key::Right)
    }
}

/// Let a column settle after its row driver changes.
///
/// Roughly 2us at 350 MHz. The pressed-key edge is driven hard by the row pad,
/// but the release edge is only the pad's weak pull-up against the trace and
/// pin capacitance, so it is worth waiting out. Written as inline asm because a
/// plain empty loop is fair game for the optimizer.
#[inline(always)]
fn settle() {
    for _ in 0..700 {
        // safety: `nop` touches no memory and no registers.
        unsafe { core::arch::asm!("nop", options(nomem, nostack, preserves_flags)) }
    }
}
