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

use pycon::host::{BADGY_AUTO, BADGY_BLINK, BADGY_DIG, BADGY_IDLE, BADGY_OOPS, BADGY_PLUG, BADGY_SLEEP};

use crate::gfx::Pixels;
use crate::mascot;
use crate::sprites;
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
/// Frames per half of the idle breathing cycle. Two-frame cycles are
/// deliberately slow: at ~20fps, alternating every frame is a flicker, not a
/// breath.
const IDLE_SWAP: u32 = 24;
/// Frames per half of the digging cycle -- fast, because it is work.
const DIG_SWAP: u32 = 4;
/// Frames per half of a script's two-frame hold. Between the two above: a
/// script that hands over a pair of frames means them to read as an action, and
/// has no way to say how fast.
const HOLD_SWAP: u32 = 12;

/// What the rest of the firmware is doing, as far as Badgy is concerned.
#[derive(Copy, Clone, Default)]
pub struct State {
    /// A host is writing to the script drive, or a rescan is pending.
    pub busy: bool,
    /// A host has the drive mounted.
    pub mounted: bool,
    /// A script is running, whether or not it is the thing on screen.
    pub working: bool,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Mood {
    Idle,
    Blink,
    Asleep,
    /// Digging: the host is dropping files on the drive.
    Digging,
    /// A script is running and has not said what it wants him doing.
    Working,
    /// Showing off the cable.
    Plugged,
    Upset,
}

impl Mood {
    /// The `BADGY_*` id a script names this mood by.
    ///
    /// [`Mood::Working`] shares the digging frames: there is one animation for
    /// "busy" and a script running is the other thing that is.
    pub fn id(self) -> i32 {
        match self {
            Mood::Idle => BADGY_IDLE,
            Mood::Blink => BADGY_BLINK,
            Mood::Asleep => BADGY_SLEEP,
            Mood::Digging | Mood::Working => BADGY_DIG,
            Mood::Plugged => BADGY_PLUG,
            Mood::Upset => BADGY_OOPS,
        }
    }
}

/// The art behind a frame id, whether it is one of ours or one a script
/// injected.
///
/// `tick` is a free-running animation counter; the two-frame moods divide it
/// down themselves, so a caller does not have to know which moods are cycles or
/// how fast they run. `None` for an id with nothing behind it -- an empty slot,
/// or [`pycon::host::SPRITE_NONE`] passed straight through from a `sprite()`
/// that found no room.
pub fn frame_art(id: i32, tick: u32) -> Option<&'static dyn Pixels> {
    // Resolved first, so every caller can hand this "whatever he is doing" and
    // get the frame that is actually on the home screen.
    let id = if id == BADGY_AUTO { mascot::shown() } else { id };
    let cycle = |a: &'static sprites::Sprite, b: &'static sprites::Sprite, every: u32| {
        if (tick / every) % 2 == 0 { a } else { b }
    };
    let sheet: &'static sprites::Sprite = match id {
        BADGY_IDLE => cycle(&sprites::IDLE_A, &sprites::IDLE_B, IDLE_SWAP),
        BADGY_BLINK => &sprites::BLINK,
        BADGY_SLEEP => &sprites::SLEEP,
        BADGY_DIG => cycle(&sprites::DIG_A, &sprites::DIG_B, DIG_SWAP),
        BADGY_PLUG => &sprites::PLUGGED,
        BADGY_OOPS => &sprites::OOPS,
        _ => return mascot::art(id).map(|a| a as &'static dyn Pixels),
    };
    Some(sheet)
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

        // A running script outranks anything the USB side has to say, and comes
        // before the doze timer: a badge left alone for a minute with a jiggler
        // running is not idle, and a mascot that falls asleep on top of working
        // firmware is the thing that makes a badge look dead. A script that has
        // taken him says better than this what it is doing; this is what the
        // ones that never ask get, so that "a script is running" is never
        // something only the `1*` in the corner knows.
        self.mood = if self.upset_left > 0 {
            Mood::Upset
        } else if st.working {
            Mood::Working
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

        // Published rather than returned, because the script that wants to know
        // is on another stack entirely: `badgy(x, y)` with no frame named draws
        // the badger as he is right now, and this is where "right now" is
        // written down.
        mascot::publish(self.mood.id());
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
    ///
    /// A script holding the mascot wins here, at the last possible moment,
    /// rather than by writing into the mood: the state machine above keeps
    /// running underneath, so when the script lets go -- or ends, or crashes --
    /// Badgy is already in the mood he would have been in, instead of resuming
    /// from whatever he was doing when someone took him.
    ///
    /// Nothing outranks the hold, [`Mood::Upset`] included. A script that has
    /// taken the badger keeps him until it lets go or ends, which is what makes
    /// the pose worth trusting: a badge showing the jiggler's mouse is jiggling,
    /// full stop, rather than jiggling unless the drive happened to mount or
    /// some *other* script fell over two seconds ago. The one crash that does
    /// show through is the holder's own -- [`mascot::release`] frees the pin
    /// when the task ends, so by the time [`Mood::Upset`] is on there is no hold
    /// left to beat.
    pub fn art(&self) -> &'static dyn Pixels {
        if let Some((a, b)) = mascot::held() {
            let id = if (self.frame / HOLD_SWAP) % 2 == 1 { b } else { a };
            // Falling through on `None` is deliberate: a script may hold him
            // on a slot it never filled, and an empty screen where the
            // badger was is a worse answer than the badger.
            if let Some(art) = frame_art(id, self.frame) {
                return art;
            }
        }
        frame_art(self.mood.id(), self.frame).unwrap_or(&sprites::IDLE_A)
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
        // A script's line, if it gave one. Not gated on `Upset` the way the
        // sprite is: the caption is the only place a background script can say
        // what it is doing, and losing it for two seconds because some *other*
        // script crashed would be a lie about which one went wrong.
        if let Some(s) = mascot::caption() {
            return s;
        }
        match self.mood {
            Mood::Digging => "digging...",
            // Same frames as digging, a different thing to say about them: this
            // is the badge's own work, not a host's.
            Mood::Working => "script running",
            Mood::Plugged => "drive mounted!",
            Mood::Asleep => "zzz - any key",
            Mood::Upset => "that went badly",
            Mood::Idle | Mood::Blink => IDLE_LINES[(self.frame as usize / 120) % IDLE_LINES.len()],
        }
    }
}
