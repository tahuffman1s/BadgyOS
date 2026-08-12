//! What a script is allowed to do to Badgy.
//!
//! [`crate::badgy`] decides which frame the firmware would show. This decides
//! what a script can put in its place: a small table of injected sprite frames,
//! and a single claim on the mascot himself -- the frame he is held on, and the
//! line under him.
//!
//! # Why the art is copied into `.bss` rather than borrowed
//!
//! A script's rows live on the interpreter's heap and die with the script. The
//! mascot is composited by task 0, from a different stack, at a moment the
//! script has no say in -- usually while it is suspended somewhere inside a
//! `sleep()`. There is no lifetime that covers both, so `define` copies the art
//! into a fixed table and hands back an integer. That also bounds the cost: four
//! frames, always, whatever the scripts do.
//!
//! One byte per pixel is three times what the three states need, and it is the
//! right trade here -- packing would buy back 17 KiB of a 2 MiB SRAM and cost a
//! shift and a mask on every pixel of every frame of the home screen. What does
//! matter is that the table is all zeroes at rest: `.data` is replayed at boot
//! out of a 40-entry poke table (see [`crate::platform`]), and a table with a
//! non-zero initialiser would not fit in it. So the empty state of a cell is
//! [`gfx::CLEAR`], which is 0.
//!
//! # Who owns the badger
//!
//! There is one of him, so the rule is the one the mouse and the USB identity
//! already use: the first task to ask keeps him, and everyone else is told
//! `false`. He goes back to the firmware when that task ends, or when it hands
//! him back by asking for `BADGY_AUTO` with nothing to say.

use pycon::host::{
    BADGY_AUTO, BADGY_CAPTION_MAX, SPRITE_MAX_H, SPRITE_MAX_W, SPRITE_NONE, SPRITE_SLOT_BASE, SPRITE_SLOTS,
};

use crate::gfx::{self, Pixels};
use crate::sched;
use crate::util::FmtBuf;

/// Pixels in one slot. Four of these is 25 KiB of `.bss`, next to the 512 KiB
/// the RAM disk already takes.
const CELLS: usize = SPRITE_MAX_W * SPRITE_MAX_H;

/// One script-supplied frame.
pub struct Custom {
    w: u16,
    h: u16,
    /// Row-major, one byte per pixel, each [`gfx::CLEAR`], [`gfx::INK`] or
    /// [`gfx::DARK`].
    cells: [u8; CELLS],
}

impl Custom {
    const EMPTY: Custom = Custom { w: 0, h: 0, cells: [gfx::CLEAR; CELLS] };

    /// True once something has been drawn into it.
    fn filled(&self) -> bool { self.h > 0 }
}

impl Pixels for Custom {
    fn width(&self) -> u16 { self.w }

    fn height(&self) -> u16 { self.h }

    fn pixel(&self, x: usize, y: usize) -> u8 {
        if x >= self.w as usize || y >= self.h as usize {
            return gfx::CLEAR;
        }
        self.cells[y * SPRITE_MAX_W + x]
    }
}

/// The frame, or pair of frames, a script is holding the mascot on.
struct Pin {
    /// The task holding him, or 0 for nobody. Task ids start at 1.
    owner: usize,
    a: i32,
    b: i32,
}

/// Injected art. Never moves, never shrinks, and is all zeroes until a script
/// asks for a slot.
static mut ART: [Custom; SPRITE_SLOTS] = [Custom::EMPTY; SPRITE_SLOTS];
/// Which task filled each slot, or 0 for a free one.
static mut OWNER: [usize; SPRITE_SLOTS] = [0; SPRITE_SLOTS];
static mut PIN: Pin = Pin { owner: 0, a: BADGY_AUTO, b: BADGY_AUTO };
static mut CAPTION: FmtBuf<BADGY_CAPTION_MAX> = FmtBuf::new();
/// The mood the compositor last drew, so a script asking for `BADGY_AUTO` gets
/// the badger as he actually is rather than a fixed guess at him.
static mut SHOWN: i32 = BADGY_AUTO;

/// Is `tid` still a live task? A slot or a claim left by one that is gone is
/// free for the taking -- the same rule `runner` applies to the USB endpoint.
fn stale(tid: usize) -> bool { tid == 0 || !sched::used(tid) }

// ------------------------------------------------------------------- the art

/// Copy `rows` into a slot and return the frame id for it, or [`SPRITE_NONE`].
///
/// `want` names a slot to overwrite, which is how a script animates without
/// running out: a loop that redefines one frame per pass needs one slot, not one
/// per pass. It has to be a slot this task already owns, or a free one.
pub fn define(tid: usize, rows: &[&str], want: Option<i32>) -> i32 {
    let Some(i) = pick(tid, want) else {
        return SPRITE_NONE;
    };

    // safety: single hart and no scheduling point anywhere below, so nothing
    // can be reading this slot while it is half-written. See `art`.
    let slot = unsafe { &mut (*core::ptr::addr_of_mut!(ART))[i] };
    slot.cells.fill(gfx::CLEAR);
    let h = rows.len().min(SPRITE_MAX_H);
    let mut w = 0;
    for (y, row) in rows.iter().take(h).enumerate() {
        // Rows are ASCII by the time they get here -- `builtins::sprite_rows`
        // rejects anything that is not one of the three characters -- so byte
        // indexing is pixel indexing.
        for (x, &b) in row.as_bytes().iter().take(SPRITE_MAX_W).enumerate() {
            slot.cells[y * SPRITE_MAX_W + x] = match b {
                pycon::host::SPRITE_INK => gfx::INK,
                pycon::host::SPRITE_DARK => gfx::DARK,
                _ => gfx::CLEAR,
            };
        }
        w = w.max(row.len().min(SPRITE_MAX_W));
    }
    // A frame of nothing but spaces is legal and blits nothing, but it must
    // still count as filled or the slot would read as free and be handed out
    // from under the script that asked for it.
    slot.w = w as u16;
    slot.h = h as u16;

    // safety: as above.
    unsafe {
        (*core::ptr::addr_of_mut!(OWNER))[i] = tid;
    }
    SPRITE_SLOT_BASE + i as i32
}

/// The slot index to write, honouring an explicit request and otherwise taking
/// the first that is free or abandoned.
fn pick(tid: usize, want: Option<i32>) -> Option<usize> {
    // safety: a read of a plain array on a single hart.
    let owners = unsafe { *core::ptr::addr_of!(OWNER) };
    if let Some(id) = want {
        let i = index(id)?;
        return if owners[i] == tid || stale(owners[i]) { Some(i) } else { None };
    }
    // `release` zeroes a finished task's slots, so in practice the first pass
    // finds everything. The second is the backstop for a slot whose owner went
    // away without it running -- which would otherwise be a slot lost for the
    // rest of the boot, and there are only four.
    owners.iter().position(|&o| o == 0).or_else(|| owners.iter().position(|&o| stale(o)))
}

/// Slot index behind a frame id, if it is one.
fn index(id: i32) -> Option<usize> {
    let i = id.checked_sub(SPRITE_SLOT_BASE)?;
    if (0..SPRITE_SLOTS as i32).contains(&i) { Some(i as usize) } else { None }
}

/// The art in the slot `id` names, if there is any.
///
/// # Why a `'static` reference out of a `static mut` is sound here
///
/// The table outlives everything, so the lifetime is honest. The aliasing is
/// too, for the same reason [`sched::fb_ptr`]'s pointers are: one hart, and
/// cooperative scheduling means a task only ever loses the CPU at a call that
/// says so. `define` contains no such call, and neither does a render pass, so
/// there is no instant at which one task is reading a slot another is writing.
pub fn art(id: i32) -> Option<&'static Custom> {
    let i = index(id)?;
    // safety: as above.
    let slot = unsafe { &(*core::ptr::addr_of!(ART))[i] };
    if slot.filled() { Some(slot) } else { None }
}

/// Every filled slot, lowest id first. For the sheet screen, which is the only
/// way to look at an injected frame on its own.
pub fn filled() -> impl Iterator<Item = (i32, &'static Custom)> {
    (0..SPRITE_SLOTS as i32).filter_map(|i| {
        let id = SPRITE_SLOT_BASE + i;
        art(id).map(|a| (id, a))
    })
}

// ---------------------------------------------------------------- the mascot

/// Claim the badger for `tid`, or report that someone else has him.
fn claim(tid: usize) -> bool {
    // safety: single hart, and no switch happens inside this function.
    unsafe {
        let pin = &mut *core::ptr::addr_of_mut!(PIN);
        if stale(pin.owner) || pin.owner == tid {
            pin.owner = tid;
            true
        } else {
            false
        }
    }
}

/// Hold him on `a`, alternating with `b` when they differ.
pub fn hold(tid: usize, a: i32, b: i32) -> bool {
    if !claim(tid) {
        return false;
    }
    // safety: as `claim`.
    unsafe {
        let pin = &mut *core::ptr::addr_of_mut!(PIN);
        pin.a = a;
        pin.b = b;
    }
    tidy(tid);
    true
}

/// Set the line under him. An empty string gives him his own back.
pub fn say(tid: usize, s: &str) -> bool {
    if !claim(tid) {
        return false;
    }
    // safety: as `claim`.
    unsafe {
        let cap = &mut *core::ptr::addr_of_mut!(CAPTION);
        cap.clear();
        // Truncated on a character boundary, so a caption with a multi-byte
        // character in it cannot leave the buffer holding half of one.
        let end = s.char_indices().map(|(i, c)| i + c.len_utf8()).take(BADGY_CAPTION_MAX).last().unwrap_or(0);
        let _ = cap.format(format_args!("{}", &s[..end]));
    }
    tidy(tid);
    true
}

/// Drop the claim once a script is holding nothing: no frame and no caption.
///
/// Without this the only way back to the firmware's own badger is for the script
/// to end, so a jiggler that pauses would keep the mascot frozen on a frame it
/// is no longer earning.
fn tidy(tid: usize) {
    // safety: as `claim`.
    unsafe {
        let pin = &mut *core::ptr::addr_of_mut!(PIN);
        if pin.owner == tid && pin.a == BADGY_AUTO && (*core::ptr::addr_of!(CAPTION)).as_str().is_empty() {
            pin.owner = 0;
            pin.b = BADGY_AUTO;
        }
    }
}

/// The frames a script is holding him on, if any.
pub fn held() -> Option<(i32, i32)> {
    // safety: single hart, plain reads.
    unsafe {
        let pin = &*core::ptr::addr_of!(PIN);
        if stale(pin.owner) || pin.a == BADGY_AUTO { None } else { Some((pin.a, pin.b)) }
    }
}

/// What a script has him saying, if anything.
pub fn caption() -> Option<&'static str> {
    // safety: `CAPTION` lives as long as the firmware, and is only written from
    // `say`, which has no scheduling point in it.
    unsafe {
        let pin = &*core::ptr::addr_of!(PIN);
        if stale(pin.owner) {
            return None;
        }
        let s = (*core::ptr::addr_of!(CAPTION)).as_str();
        if s.is_empty() { None } else { Some(s) }
    }
}

/// Record the mood the compositor is drawing, for `BADGY_AUTO` to resolve to.
pub fn publish(frame: i32) {
    // safety: single hart, a plain word write.
    unsafe {
        SHOWN = frame;
    }
}

/// The mood the badger is in right now.
pub fn shown() -> i32 {
    // safety: single hart, a plain word read.
    let f = unsafe { SHOWN };
    // Before the home screen has ever been drawn there is no answer; idle is
    // the honest one, since that is what he would be doing.
    if f == BADGY_AUTO { pycon::host::BADGY_IDLE } else { f }
}

/// Let go of everything `tid` was holding.
///
/// Called when a task ends. Without it a script that pinned the mascot and then
/// crashed would leave the home screen showing a frame nothing is maintaining --
/// and, worse, leave the slot behind it locked for the rest of the boot.
pub fn release(tid: usize) {
    if tid == 0 {
        return;
    }
    // safety: single hart, and no switch happens inside this function.
    unsafe {
        let owners = &mut *core::ptr::addr_of_mut!(OWNER);
        for o in owners.iter_mut() {
            if *o == tid {
                *o = 0;
            }
        }
        let pin = &mut *core::ptr::addr_of_mut!(PIN);
        if pin.owner == tid {
            *pin = Pin { owner: 0, a: BADGY_AUTO, b: BADGY_AUTO };
            (*core::ptr::addr_of_mut!(CAPTION)).clear();
        }
    }
}
