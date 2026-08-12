//! Green threads: several scripts at once on one hart, and a way to take the
//! badge back from any of them.
//!
//! # Why cooperative, on a chip with no interrupts
//!
//! This core runs one thread of control and this firmware never unmasks an
//! interrupt -- `mtvec` points at `abort`. So "multi-threaded" here means
//! coroutines with their own stacks, switched by hand at points the running
//! code chooses.
//!
//! Cooperative scheduling normally has one fatal flaw: a task that never yields
//! wedges the machine, and `while True: pass` is a legal pycon program. That
//! flaw does not exist here, because the interpreter already yields on a
//! cadence it controls rather than one the script controls. `Interp::steps`
//! fires [`pycon::Host::tick`] every `tick_interval` statements no matter what
//! the statements are, and every blocking builtin (`sleep`, `wait_key`, `show`,
//! the mouse reports) funnels through the same place. A pycon script *cannot*
//! fail to reach a scheduling point, which is what makes this sound where it
//! would not be for arbitrary native code.
//!
//! It is also why preemption would be a poor trade. A timer interrupt would buy
//! the ability to reap a task wedged in Rust, and would cost putting the
//! allocator's spin lock, `usb::poll` and the ReRAM commit sequence in reach of
//! reentrancy -- the same argument `usb/mod.rs` makes for polling the USB
//! controller instead of taking its interrupt.
//!
//! # The switch
//!
//! `badgy_switch` is the whole kernel: push `ra` and `s0`-`s11`, store `sp` in
//! the outgoing task, load `sp` from the incoming one, pop, return. Everything
//! else the ABI says is caller-saved, and there is no float state on rv32imac.
//! A task's stack is a slice of `.bss`; a fresh one gets a hand-built frame
//! whose return address is [`task_entry`], so the first switch into it "returns"
//! into the trampoline.
//!
//! # What is shared, and how
//!
//! * **The panel.** Nobody draws to it. Every task draws into its own [`Fb`] and the compositor -- task
//!   [`UI`], the only thing that touches the hardware -- presents whichever page has focus.
//! * **The keys.** Only the focused task sees them, or two scripts would fight over the wheel.
//! * **The clock.** [`platform::tick_clock`] is the single reader of timer0's sticky flag; see its comment
//!   for why that matters more than it looks.
//! * **The allocator.** `linked_list_allocator`'s lock is never held across a switch, because switches happen
//!   only where this module puts them and none of those places is inside `alloc`. That is a property of
//!   cooperative scheduling worth writing down: with preemption it would be false.
//! * **The heap itself** is not partitioned. Three scripts share the 768 KiB, and [`spawn`] refuses to start
//!   a fourth thing when what is left would not see a script through -- see [`Spawn::LowHeap`].

use alloc::boxed::Box;
use core::ptr::addr_of_mut;

use bao1x_hal::sh1107::Oled128x128;
use ux_api::minigfx::FrameBuffer;

use crate::gfx::Fb;
use crate::platform;
use crate::runner;
use crate::usb;
use crate::util::FmtBuf;

/// Task slots, including the UI. Three scripts at once is not a hardware limit
/// -- it is a stack and heap budget, and three is what fits comfortably next to
/// a 16 KiB script's parse peak.
pub const MAX_TASKS: usize = 4;
/// Slots available to scripts.
pub const SCRIPT_SLOTS: usize = MAX_TASKS - 1;
/// The UI, and the compositor: slot 0, running on the boot stack.
pub const UI: usize = 0;

/// Stack per script task, in words. 48 KiB.
///
/// Sized from what bounds it: `MAX_EVAL_DEPTH` (96) caps how deep the recursive
/// evaluator can go, and a frame there costs a couple of hundred bytes at
/// `opt-level = "s"` -- so the worst case is on the order of 24 KiB and this is
/// twice that. Generous on purpose, because the alternative use for the memory
/// is nothing: this comes out of `.bss`, which the heap does not compete for,
/// and there is still half a megabyte under the boot stack afterwards. The task
/// manager shows each task's high-water mark, so the real number is checkable
/// on the badge rather than a matter of belief.
const STACK_WORDS: usize = 12 * 1024;

/// Words at the bottom of each stack that nothing should ever reach, checked at
/// every scheduling point. 1 KiB.
const GUARD_WORDS: usize = 256;

/// Written over a fresh stack. Two jobs: the untouched part measures how deep a
/// task has been, and the part inside the guard zone is a canary.
const POISON: usize = 0xa5a5_a5a5;

/// Callee-saved frame `badgy_switch` pushes: `ra` + `s0`-`s11` is 52 bytes,
/// rounded up to the 16-byte stack alignment the RISC-V ABI requires.
const FRAME_BYTES: usize = 64;
/// Where `ra` sits inside that frame.
const RA_OFFSET: usize = 60;

/// Heap that must remain after a spawn for the new task to be able to do
/// anything at all. Above `HEAP_RESERVE`, which is what the *firmware* needs to
/// survive; a script that cannot allocate a list is not worth starting.
const SPAWN_RESERVE: usize = platform::HEAP_RESERVE + 64 * 1024;

/// Frame interval a script's `show()` is paced to, in milliseconds.
///
/// This is what a panel refresh costs, and before there was a scheduler it was
/// what paced every animation on the badge: `show()` blocked for ~14 ms because
/// the SPI transfer did. Presenting from a page made `show()` nearly free for
/// background tasks, which would have quietly sped every backgrounded animation
/// up by an order of magnitude. So the cost is kept, deliberately, and spent on
/// other tasks instead of on the bus.
const FRAME_MS: u32 = 14;

/// What a task is doing, for the task manager and for the CPU meter.
///
/// Only [`Status::Run`] is charged for time: a task spinning in `nap` is
/// waiting, not working, and charging it would make a badge full of sleeping
/// scripts look busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// Slot is empty. Must be the zero variant: the task table lives in `.bss`.
    Free = 0,
    Run,
    /// In `sleep()`.
    Sleep,
    /// In `wait_key()`.
    Key,
    /// In `show()`, waiting for the compositor or for the frame interval.
    Draw,
    /// Ran to the end, or was stopped. The slot holds the outcome until it is
    /// reaped.
    Done,
}

impl Status {
    /// Three characters, for a task manager row.
    pub const fn abbrev(self) -> &'static str {
        match self {
            Status::Free => "---",
            Status::Run => "run",
            Status::Sleep => "slp",
            Status::Key => "key",
            Status::Draw => "drw",
            Status::Done => "end",
        }
    }
}

/// How a task ended. Mirrors `app::Outcome`, but without the `String`: the task
/// table is a `static` in `.bss` and every non-zero word of a static costs an
/// entry in the image's poke table, so nothing here allocates or starts out
/// non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Ended {
    /// Still running, or the slot is free.
    No = 0,
    Finished,
    /// Killed -- by the exit chord, or from the task manager.
    Stopped,
    /// A runtime error; the message is in `Task::msg`.
    Failed,
}

struct Task {
    /// Stack pointer while suspended. Meaningless while running.
    sp: usize,
    status: Status,
    ended: Ended,
    /// Set to ask the task to stop at its next tick.
    kill: bool,
    /// The task was stopped by the firmware rather than by a person, and `msg`
    /// already says why. Keeps [`finish`] from reporting a stack overflow as an
    /// ordinary "stopped".
    faulted: bool,
    /// Script name, for the manager and the result screen.
    name: FmtBuf<24>,
    /// Error message, if `ended == Failed`.
    msg: FmtBuf<96>,
    /// The program, handed over by [`spawn`] and taken by the task itself.
    /// Raw rather than a `Box` because it crosses a stack switch.
    script: *mut pycon::Script,
    seed: u32,
    /// Index into [`STACKS`], plus one. Zero means "no stack", which is the UI.
    stack: usize,
    /// Milliseconds this task has been `Run` at a clock edge -- a sampling
    /// profiler, in effect, and fair for the same reason one is.
    ms: u32,
    /// The task has a frame ready that the compositor has not presented.
    dirty: bool,
    /// Last `show()`, for pacing to [`FRAME_MS`].
    shown_ms: u32,
}

impl Task {
    /// All-zero, so the table lands in `.bss` and costs no image bytes.
    const NEW: Task = Task {
        sp: 0,
        status: Status::Free,
        ended: Ended::No,
        kill: false,
        faulted: false,
        name: FmtBuf::new(),
        msg: FmtBuf::new(),
        script: core::ptr::null_mut(),
        seed: 0,
        stack: 0,
        ms: 0,
        dirty: false,
        shown_ms: 0,
    };
}

/// A task stack. Aligned to 16 because that is what the RISC-V ABI requires of
/// `sp` at a call boundary, and a bare `[usize; N]` is only aligned to 4 -- so
/// without this the frame [`spawn`] lays down would be legal about a quarter of
/// the time, which is the worst kind of bug to go looking for.
#[repr(align(16))]
struct Stack([usize; STACK_WORDS]);

// The scheduler's state. `static mut` with access funnelled through this
// module, on the same grounds `usb::poll` gives: one hart, no interrupts, and
// switches only at the points below, so no two borrows can overlap.
static mut TASKS: [Task; MAX_TASKS] = [Task::NEW; MAX_TASKS];
static mut STACKS: [Stack; SCRIPT_SLOTS] = [const { Stack([0; STACK_WORDS]) }; SCRIPT_SLOTS];
/// One page per script slot -- the UI has none, because it draws straight into
/// the display buffer it owns. Kept apart from [`TASKS`] so that a task holding
/// its own page cannot be said to alias the table.
static mut FBS: [Fb; SCRIPT_SLOTS] = [Fb::NEW; SCRIPT_SLOTS];
static mut CURRENT: usize = UI;
/// Whose page reaches the panel. `UI` means the firmware's own screens.
static mut FOCUS: usize = UI;
/// Clock reading at the last time accounting.
static mut LAST_SAMPLE: u32 = 0;

// safety, for every `&mut *addr_of_mut!(...)` below: single hart, no
// interrupts, and the only preemption point in the firmware is `switch` -- so
// none of these short-lived borrows can overlap another.
#[inline]
fn tasks() -> &'static mut [Task; MAX_TASKS] { unsafe { &mut *addr_of_mut!(TASKS) } }

#[inline]
fn task(tid: usize) -> &'static mut Task { &mut tasks()[tid] }

core::arch::global_asm!(
    "
    .section .text.badgy_switch, \"ax\", @progbits
    .globl badgy_switch
    .type badgy_switch, @function
badgy_switch:
    addi sp, sp, -64
    sw   ra, 60(sp)
    sw   s0, 56(sp)
    sw   s1, 52(sp)
    sw   s2, 48(sp)
    sw   s3, 44(sp)
    sw   s4, 40(sp)
    sw   s5, 36(sp)
    sw   s6, 32(sp)
    sw   s7, 28(sp)
    sw   s8, 24(sp)
    sw   s9, 20(sp)
    sw   s10, 16(sp)
    sw   s11, 12(sp)
    sw   sp, 0(a0)
    lw   sp, 0(a1)
    lw   s11, 12(sp)
    lw   s10, 16(sp)
    lw   s9, 20(sp)
    lw   s8, 24(sp)
    lw   s7, 28(sp)
    lw   s6, 32(sp)
    lw   s5, 36(sp)
    lw   s4, 40(sp)
    lw   s3, 44(sp)
    lw   s2, 48(sp)
    lw   s1, 52(sp)
    lw   s0, 56(sp)
    lw   ra, 60(sp)
    addi sp, sp, 64
    ret
    .size badgy_switch, . - badgy_switch
"
);

extern "C" {
    /// Save the current context into `*save`, restore the one at `*load`.
    ///
    /// Returns to its caller on the *incoming* stack, which is the whole trick:
    /// the call appears to return normally, just much later and to a different
    /// task.
    fn badgy_switch(save: *mut usize, load: *const usize);
}

// -------------------------------------------------------------------- startup

/// Claim slot [`UI`] for the caller. Called once, from the main loop's task,
/// before anything can be spawned.
pub fn init() {
    let t = task(UI);
    t.status = Status::Run;
    let _ = t.name.format(format_args!("BadgyOS"));
    unsafe { LAST_SAMPLE = platform::tick_clock() };
}

// --------------------------------------------------------------- housekeeping

/// Charge the time since the last look to whoever was running.
///
/// The resolution is one millisecond and most switches happen inside one, so
/// almost every call charges nothing. What lands is a sample of "who was
/// running when the clock ticked", which is how a profiler works and is fair
/// over any window long enough to display.
fn account() {
    let now = platform::tick_clock();
    // safety: as `tasks()`.
    let last = unsafe { LAST_SAMPLE };
    let delta = now.wrapping_sub(last);
    if delta == 0 {
        return;
    }
    unsafe { LAST_SAMPLE = now };
    let cur = current();
    if task(cur).status == Status::Run {
        task(cur).ms = task(cur).ms.wrapping_add(delta);
    }
}

/// Has the running task written into the bottom of its own stack?
///
/// # Why this is worth two loads per switch
///
/// Before there were tasks, "too deep" meant walking into half a megabyte of
/// unused SRAM, and `MAX_EVAL_DEPTH` was there so it never happened. Now the
/// thing under a task's stack is *another task's stack*, and the failure mode
/// of running off the end changed from harmless to the worst kind: silent
/// corruption of a page or a table somewhere else, surfacing later as something
/// that looks nothing like a deep script.
///
/// So the bottom kilobyte is left poisoned and probed here. Probing two words
/// rather than all 256 is not laziness: a stack grows downward one frame at a
/// time, so the highest guard word is the first thing an overflow touches, and
/// the lowest is what catches a frame large enough to have stepped over it.
///
/// This is a net rather than a proof. The evaluator can nest up to
/// `MAX_EVAL_DEPTH` levels between two ticks, so in principle it could cross
/// the whole guard zone inside one window -- which is why the zone sits under
/// 24 KiB of headroom rather than being the only thing in the way.
fn check_guard() {
    let tid = current();
    let t = task(tid);
    if t.stack == 0 || t.faulted {
        return;
    }
    // safety: as `tasks()`; reading the stack of the task that is running,
    // below anything it has legitimately written.
    let guard = unsafe { &(*addr_of_mut!(STACKS))[t.stack - 1].0 };
    if guard[GUARD_WORDS - 1] == POISON && guard[0] == POISON {
        return;
    }
    fault(tid, "ran out of stack -- too deeply nested");
}

/// Stop a task because the firmware says so, with a reason.
///
/// Unlike [`kill`] this records why, so the result screen says something more
/// useful than "stopped". It still goes out through the interpreter's own abort
/// path -- there is no way to stop a task from outside its own stack, and that
/// is deliberate: see [`kill`].
pub fn fault(tid: usize, why: &str) {
    let t = task(tid);
    if t.faulted {
        return;
    }
    t.faulted = true;
    t.kill = true;
    let _ = t.msg.format(format_args!("{}", why));
    crate::println!("task {}: {}", tid, why);
}

/// The next slot that can run, searching round-robin from the current one.
///
/// [`UI`] is always runnable, so this always finds something -- which is what
/// lets a finished task yield into oblivion without a special case, and why
/// there is no idle task.
fn pick(from: usize) -> usize {
    for step in 1..=MAX_TASKS {
        let cand = (from + step) % MAX_TASKS;
        let t = task(cand);
        if cand == UI || (t.status != Status::Free && t.status != Status::Done) {
            return cand;
        }
    }
    UI
}

/// Give up the CPU. Returns when the scheduler comes back around.
///
/// Everything that blocks in this firmware is a loop around this call. There is
/// no wait queue and no sleep list: a waiting task is still scheduled, and
/// re-checks its own condition. With at most four slots that costs a compare
/// per round, which is cheaper than the bookkeeping it replaces.
pub fn yield_now() {
    // Servicing USB from the switch point means it happens on every task's
    // tick, not just the UI's, so the drive stays responsive no matter which
    // task is hot.
    usb::poll();
    account();
    check_guard();

    let cur = current();
    let next = pick(cur);
    if next == cur {
        return;
    }
    unsafe { CURRENT = next };
    // safety: `cur` and `next` are different slots, so the two pointers do not
    // alias, and every suspended task's `sp` was written by this same routine.
    unsafe {
        let table = addr_of_mut!(TASKS) as *mut Task;
        badgy_switch(
            core::ptr::addr_of_mut!((*table.add(cur)).sp),
            core::ptr::addr_of!((*table.add(next)).sp),
        );
    }
}

/// Yield until `ms` have passed.
///
/// Always gives up the CPU at least once, even for `nap(0)`, so that a loop
/// built out of this cannot accidentally become a loop that never yields.
/// Says nothing about the caller's [`Status`] -- the caller sets that, because
/// only it knows whether it is sleeping, waiting for a key, or waiting to draw.
pub fn nap(ms: u32) {
    let start = platform::now_ms();
    yield_now();
    while !platform::elapsed(start, ms) {
        yield_now();
    }
}

/// The UI's replacement for `delay_polled`: hold the loop's cadence, but spend
/// the wait on whatever else wants to run.
pub fn pace(ms: u32) {
    let start = platform::now_ms();
    set_status(UI, Status::Sleep);
    while !platform::elapsed(start, ms) {
        yield_now();
    }
    set_status(UI, Status::Run);
}

// ------------------------------------------------------------------- spawning

/// Why a script could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spawn {
    /// Every script slot is busy with something still running.
    NoSlot,
    /// Not enough heap left to run another one.
    LowHeap,
}

impl Spawn {
    pub const fn message(self) -> &'static str {
        match self {
            Spawn::NoSlot => "too many scripts already running -- stop one first",
            Spawn::LowHeap => "not enough memory left to start another script",
        }
    }
}

/// Start `script` as a task, and return its slot.
///
/// The program is compiled by the *caller*, on the caller's stack, and handed
/// over already parsed. That is deliberate: lexing and parsing hold the token
/// vector and the AST at the same time and peak near 250 KB for a large script,
/// so doing it here keeps two of those peaks from ever overlapping -- and it
/// means a syntax error is reported by the menu, synchronously, without a task
/// ever existing.
pub fn spawn(name: &str, script: Box<pycon::Script>, seed: u32) -> Result<usize, Spawn> {
    if platform::heap_free() < SPAWN_RESERVE {
        return Err(Spawn::LowHeap);
    }
    let tid = free_slot().ok_or(Spawn::NoSlot)?;
    let slot = tid - 1; // slot 0 is the UI, which has no stack of its own

    // Poison the stack so the manager can report how much of it was ever
    // touched, and lay a first frame at the top whose return address is the
    // trampoline. The first switch into this task "returns" into `task_entry`.
    let sp = {
        // safety: this slot is free, so no task is running on this stack.
        let stack = unsafe { &mut (*addr_of_mut!(STACKS))[slot] };
        stack.0.fill(POISON);
        let top = stack.0.as_mut_ptr() as usize + STACK_WORDS * core::mem::size_of::<usize>();
        let sp = top - FRAME_BYTES;
        // safety: `sp + RA_OFFSET` is inside the frame just carved out of the
        // stack we own.
        unsafe { ((sp + RA_OFFSET) as *mut usize).write(task_entry as *const () as usize) };
        sp
    };

    fb(tid).clear();

    let t = task(tid);
    t.sp = sp;
    t.status = Status::Run;
    t.ended = Ended::No;
    t.kill = false;
    t.faulted = false;
    t.script = Box::into_raw(script);
    t.seed = seed;
    t.stack = slot + 1;
    t.ms = 0;
    t.dirty = false;
    t.shown_ms = platform::now_ms();
    t.msg.clear();
    let _ = t.name.format(format_args!("{}", name));

    crate::println!("task {}: spawned {}", tid, name);
    Ok(tid)
}

/// A slot for a new task: an empty one, or the oldest corpse.
///
/// Reusing a `Done` slot means a run whose result nobody looked at is dropped
/// silently rather than blocking the next launch. The manager is where results
/// are read, and it holds them until something needs the room.
fn free_slot() -> Option<usize> {
    (1..MAX_TASKS).find(|&i| task(i).status == Status::Free).or_else(|| {
        let tid = (1..MAX_TASKS).find(|&i| task(i).status == Status::Done)?;
        reap(tid);
        Some(tid)
    })
}

/// Where a fresh task begins.
///
/// `extern "C"` and never returning: it is entered by `ret`, not by a call, so
/// there is nothing above it on the stack to return to.
extern "C" fn task_entry() -> ! {
    let tid = current();
    // safety: `spawn` filled this in before making the slot runnable, and this
    // is the only place that takes it back.
    let script = unsafe { Box::from_raw(task(tid).script) };
    task(tid).script = core::ptr::null_mut();

    // A scope, so the interpreter, the host and the program are all dropped --
    // returning their heap -- before the slot is marked done. A killed task
    // unwinds through the same path as one that finished, which is what lets
    // `BadgeHost::drop` still release a mouse button the script was holding.
    {
        let seed = task(tid).seed;
        runner::run_task(tid, &script, seed);
    }
    drop(script);

    let t = task(tid);
    t.status = Status::Done;
    crate::println!("task {}: {} ended", tid, t.name.as_str());

    // Nothing schedules a `Done` task, so this yield never comes back.
    loop {
        yield_now();
    }
}

/// Record how a task ended. Called by the runner from inside the task.
///
/// A task the firmware faulted keeps the reason it was given: the interpreter
/// reports the abort as an ordinary stop, because from inside it that is what
/// it looks like, and "stopped" is not what the user needs to read after a
/// stack overflow.
pub fn finish(tid: usize, ended: Ended, msg: &str) {
    let t = task(tid);
    if t.faulted {
        t.ended = Ended::Failed;
        return;
    }
    t.ended = ended;
    if !msg.is_empty() {
        let _ = t.msg.format(format_args!("{}", msg));
    }
}

/// Ask a task to stop at its next tick.
///
/// This does not touch the task's stack. It sets a flag that the interpreter's
/// own abort path reads, so the script unwinds exactly as it does for the exit
/// chord -- freeing its values, running `Drop`, and releasing any mouse button
/// it was holding. Tearing the stack down from outside would leak all of that,
/// which is why there is no such call.
pub fn kill(tid: usize) {
    if tid != UI && task(tid).status != Status::Free {
        task(tid).kill = true;
    }
}

/// Kill every script. Used by the manager's "stop all".
pub fn kill_all() {
    for tid in 1..MAX_TASKS {
        kill(tid);
    }
}

/// Release a finished slot, so its stack and page can be used again.
pub fn reap(tid: usize) {
    if tid == UI || task(tid).status != Status::Done {
        return;
    }
    if unsafe { FOCUS } == tid {
        unsafe { FOCUS = UI };
    }
    let t = task(tid);
    t.status = Status::Free;
    t.ended = Ended::No;
    t.kill = false;
    t.faulted = false;
    t.msg.clear();
    t.name.clear();
    t.stack = 0;
}

// ------------------------------------------------------------------- the page

#[inline]
fn fb(tid: usize) -> &'static mut Fb {
    debug_assert!(tid != UI, "the UI draws into the display buffer, not a page");
    // safety: as `tasks()`, plus: exactly one task owns each page, and the
    // compositor only reads a page while its owner is suspended -- which,
    // cooperatively scheduled, is whenever the compositor is running at all.
    unsafe { &mut (*addr_of_mut!(FBS))[tid - 1] }
}

/// The page a task draws into. For the runner, which holds it for the life of
/// the task.
pub fn fb_ptr(tid: usize) -> *mut Fb { fb(tid) as *mut Fb }

/// Offer a finished frame to the compositor, and wait until it is on glass.
///
/// Unfocused tasks have nobody to wait for, so they are paced to [`FRAME_MS`]
/// instead -- the same wall-clock cost their `show()` had when it drove the
/// panel directly. An animation therefore runs at the same speed in the
/// background as in the foreground, and the time goes to other tasks rather
/// than to the SPI bus.
pub fn show(tid: usize) {
    let t = task(tid);
    t.dirty = true;
    let since = t.shown_ms;
    set_status(tid, Status::Draw);
    // Focus is re-read every pass rather than decided once, because it can move
    // while this waits: taking the screen back from a script (LEFT+RIGHT) while
    // it sits here would otherwise leave it waiting for a compositor that is no
    // longer looking at it, which is a hang -- and the only one this design
    // could have had.
    while !killed(tid) {
        if focus() == tid {
            if !task(tid).dirty {
                break;
            }
        } else if platform::elapsed(since, FRAME_MS) {
            break;
        }
        yield_now();
    }
    let t = task(tid);
    t.shown_ms = platform::now_ms();
    t.dirty = false;
    set_status(tid, Status::Run);
}

/// Does this task have a frame the compositor has not shown yet?
pub fn pending(tid: usize) -> bool { task(tid).dirty }

/// Copy a task's page onto the panel. The caller still has to `draw()`.
///
/// This is the only route from a page to the hardware, and the UI task is the
/// only caller -- so the 2 KiB copy happens on the one stack that is allowed to
/// touch the display.
pub fn present(tid: usize, disp: &mut Oled128x128<'_>) {
    disp.blit_screen(fb(tid).words());
    task(tid).dirty = false;
}

// ----------------------------------------------------------------- accessors

pub fn current() -> usize { unsafe { CURRENT } }

/// Which task's page is on the panel. [`UI`] means the firmware's own screens.
pub fn focus() -> usize { unsafe { FOCUS } }

/// Put a task's page on the panel. Only a task that is still running can take
/// the screen -- a finished one has nothing to show that its result screen does
/// not say better.
pub fn set_focus(tid: usize) {
    let alive =
        tid < MAX_TASKS && matches!(status(tid), Status::Run | Status::Sleep | Status::Key | Status::Draw);
    let target = if tid != UI && alive { tid } else { UI };
    unsafe { FOCUS = target };
    // Ask the incoming task for the panel at once rather than leaving the
    // outgoing screen up: it may be mid-sleep and not due to draw for a while,
    // and its page already holds the last thing it drew.
    if target != UI {
        task(target).dirty = true;
    }
}

pub fn is_focused(tid: usize) -> bool { focus() == tid }

pub fn killed(tid: usize) -> bool { task(tid).kill }

pub fn status(tid: usize) -> Status { task(tid).status }

pub fn set_status(tid: usize, s: Status) {
    // A task that has ended keeps saying so; the runner sets `Run` back on the
    // way out of a blocking call and must not resurrect a corpse.
    if task(tid).status != Status::Done {
        task(tid).status = s;
    }
}

pub fn ended(tid: usize) -> Ended { task(tid).ended }

pub fn name(tid: usize) -> &'static str { task(tid).name.as_str() }

pub fn message(tid: usize) -> &'static str { task(tid).msg.as_str() }

/// Is anything in this slot -- running or finished and unread?
pub fn used(tid: usize) -> bool { task(tid).status != Status::Free }

/// Scripts that are still running.
pub fn running() -> usize {
    (1..MAX_TASKS)
        .filter(|&i| matches!(task(i).status, Status::Run | Status::Sleep | Status::Key | Status::Draw))
        .count()
}

/// Slots holding anything at all, finished ones included.
pub fn occupied() -> usize { (1..MAX_TASKS).filter(|&i| used(i)).count() }

/// This task's share of the time all tasks have spent running, in percent.
///
/// Relative rather than absolute: the badge is never idle -- the UI spins when
/// it has nothing to do -- so "percent of a core" would read 100 forever. What
/// is worth seeing is which script is eating the machine.
pub fn cpu_percent(tid: usize) -> u32 {
    let total: u32 = (0..MAX_TASKS).map(|i| task(i).ms).sum();
    if total == 0 { 0 } else { task(tid).ms.saturating_mul(100) / total }
}

/// Bytes of this task's stack that have ever been touched, and the size of it.
///
/// Counts from the low end: the untouched part is still [`POISON`], so the
/// first word that is not gives the deepest the task has ever been. The UI has
/// no stack of its own -- it runs on the boot stack, which has the rest of SRAM
/// under it -- so it reports nothing.
pub fn stack_used(tid: usize) -> Option<(usize, usize)> {
    let slot = task(tid).stack.checked_sub(1)?;
    // safety: as `tasks()`; a read-only walk of a stack whose owner is
    // suspended (the caller is the UI, so it must be).
    let stack = unsafe { &(*addr_of_mut!(STACKS))[slot] };
    let untouched = stack.0.iter().take_while(|&&w| w == POISON).count();
    let word = core::mem::size_of::<usize>();
    Some(((STACK_WORDS - untouched) * word, STACK_WORDS * word))
}
