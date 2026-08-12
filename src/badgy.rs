//! Badgy: which frame of the badger to draw, and what he has to say about it.
//!
//! The sprite sheet in [`crate::sprites`] is just art. This is the part that
//! makes it read as a character: an idle cycle with an irregular blink, a mood
//! picked from what the firmware is actually doing, and a caption.
//!
//! # Continuous state is derived; only events are pushed
//!
//! [`Badgy::step`] is handed the firmware's real state once per animation frame
//! and works the mood out from it, rather than exposing `set_busy()`-style
//! setters for callers to keep in sync: there is one place that has to be right
//! (the [`State`] that `app` builds) instead of one per transition, and a mood
//! cannot get stuck on -- the failure mode that makes a mascot look broken
//! rather than asleep.
//!
//! The two things that are *moments* rather than conditions -- a key press, a
//! script blowing up -- come in as calls ([`Badgy::poke`], [`Badgy::upset`]) and
//! are held for a bounded number of frames. "The last script failed" is true
//! forever once it happens, so deriving a mood from it directly would leave
//! Badgy sulking for the rest of the boot.
//!
//! # Timing
//!
//! Everything here counts animation frames, not milliseconds, for the reason
//! given in [`crate::app`]: a panel refresh costs about three polls, so a
//! millisecond clock sampled from the render loop under-counts by however long
//! drawing took. At `FRAME_POLLS = 8` and `POLL_MS = 4` a frame is roughly
//! 45 ms once the draw itself is paid for, so the constants below are in units
//! of about 20 per second.

use crate::sprites::{self, Sprite};
use crate::util::hash3;

/// Frames of no key presses before Badgy dozes off.
const SLEEP_AFTER: u32 = 900;
/// How long Badgy shows off a freshly mounted drive before going back to idle.
const PLUGGED_FOR: u32 = 80;
/// How long a failed script keeps him rattled.
const UPSET_FOR: u32 = 120;
/// Frames a blink lasts. Two is enough to read as a blink and short enough that
/// it never looks like he is squinting.
const BLINK_FRAMES: u32 = 2;
/// Average frames between blinks. The actual gap is this plus a hash-derived
/// spread, because a blink on a fixed period reads as a flashing light.
const BLINK_EVERY: u32 = 70;

/// What the rest of the firmware is doing, as far as Badgy is concerned.
#[derive(Copy, Clone, Default)]
pub struct State {
    /// A host is writing to the script drive, or a rescan is pending.
    pub busy: bool,
    /// A host has the drive mounted.
    pub mounted: bool,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Mood {
    Idle,
    Blink,
    Asleep,
    /// Digging: the host is dropping files on the drive.
    Digging,
    /// Showing off the cable.
    Plugged,
    Upset,
}

pub struct Badgy {
    /// Free-running animation frame counter.
    frame: u32,
    /// Frames since the last key press.
    since_poke: u32,
    /// Frames left of showing the plug.
    plug_left: u32,
    /// Frames left of being rattled by a failed script.
    upset_left: u32,
    was_mounted: bool,
    mood: Mood,
}

impl Badgy {
    pub const fn new() -> Self {
        Badgy { frame: 0, since_poke: 0, plug_left: 0, upset_left: 0, was_mounted: false, mood: Mood::Idle }
    }

    /// Any key press: wake up, and reset the doze timer.
    pub fn poke(&mut self) {
        self.since_poke = 0;
        if self.mood == Mood::Asleep {
            self.mood = Mood::Idle;
        }
    }

    /// A script just failed.
    pub fn upset(&mut self) { self.upset_left = UPSET_FOR; }

    /// Advance one animation frame and re-derive the mood.
    pub fn step(&mut self, st: State) {
        self.frame = self.frame.wrapping_add(1);
        self.since_poke = self.since_poke.saturating_add(1);

        // Rising edge of "a host mounted the drive". Worth its own state because
        // it is the one event the badge cannot otherwise show the user: the
        // volume appearing on a computer happens entirely off-badge.
        if st.mounted && !self.was_mounted {
            self.plug_left = PLUGGED_FOR;
        }
        self.was_mounted = st.mounted;
        self.plug_left = self.plug_left.saturating_sub(1);
        self.upset_left = self.upset_left.saturating_sub(1);

        self.mood = if self.upset_left > 0 {
            Mood::Upset
        } else if st.busy {
            Mood::Digging
        } else if self.plug_left > 0 {
            Mood::Plugged
        } else if self.since_poke >= SLEEP_AFTER {
            Mood::Asleep
        } else if self.blinking() {
            Mood::Blink
        } else {
            Mood::Idle
        };
    }

    /// True for [`BLINK_FRAMES`] out of every ~[`BLINK_EVERY`].
    ///
    /// The gap is jittered by hashing the blink's own index, so blinks land at
    /// irregular intervals without keeping any extra state -- a fixed period is
    /// the thing that makes an animated face look like an indicator LED.
    fn blinking(&self) -> bool {
        let n = self.frame / BLINK_EVERY;
        let jitter = hash3(n, 0xbadd_9e11, 0) % (BLINK_EVERY - BLINK_FRAMES);
        let phase = self.frame % BLINK_EVERY;
        phase >= jitter && phase < jitter + BLINK_FRAMES
    }

    /// The frame to draw.
    pub fn sprite(&self) -> &'static Sprite {
        match self.mood {
            // Two-frame cycles are deliberately slow: at ~20fps, alternating
            // every frame is a flicker, not a breath.
            Mood::Idle => {
                if (self.frame / 24) % 2 == 0 {
                    &sprites::IDLE_A
                } else {
                    &sprites::IDLE_B
                }
            }
            Mood::Blink => &sprites::BLINK,
            Mood::Asleep => &sprites::SLEEP,
            Mood::Digging => {
                if (self.frame / 4) % 2 == 0 {
                    &sprites::DIG_A
                } else {
                    &sprites::DIG_B
                }
            }
            Mood::Plugged => &sprites::PLUGGED,
            Mood::Upset => &sprites::OOPS,
        }
    }

    /// One line, in Badgy's voice, for under the sprite.
    ///
    /// The idle line rotates on a slow cycle keyed off the frame counter, so a
    /// badge left on a table has something new to say now and then.
    pub fn caption(&self) -> &'static str {
        const IDLE_LINES: &[&str] = &[
            "-PUSH WHEEL-",
            "drop .py on me",
            "badgers dig it",
            "-PUSH WHEEL-",
            "1d50:6199",
            "no k0, no regrets",
        ];
        match self.mood {
            Mood::Digging => "digging...",
            Mood::Plugged => "drive mounted!",
            Mood::Asleep => "zzz - any key",
            Mood::Upset => "that went badly",
            Mood::Idle | Mood::Blink => IDLE_LINES[(self.frame as usize / 120) % IDLE_LINES.len()],
        }
    }
}
