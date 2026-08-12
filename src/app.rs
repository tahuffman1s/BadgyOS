//! The application: a screen state machine driven by the jog wheel, and the
//! compositor for everything else that draws.
//!
//! # The loop
//!
//! This is task 0. It services the USB controller, samples the key matrix,
//! gives the script importer a chance to notice that the drive changed, and
//! repaints if anything is animating -- and then, instead of spinning out the
//! rest of its 4 ms, it hands the badge to whatever scripts are running. See
//! [`crate::sched`]. Timing is still counted in polls rather than milliseconds
//! because a full `draw()` -- 2 KiB over a 2 MHz SPI -- costs about as much as
//! three polls.
//!
//! # Who owns the panel
//!
//! This task, exclusively. Scripts draw into their own pages; the one with
//! focus has its page copied onto the panel here and nowhere else. So the
//! screen shows one of two things: a firmware screen drawn straight into the
//! display buffer, or a script's page presented whole. [`Screen::ScriptView`]
//! is the second case, and while it is up the keys belong to the script -- with
//! two exceptions, both chords, because single keys are part of the script API:
//! LEFT+CENTER stops the script (handled inside it, in `runner`), and
//! LEFT+RIGHT leaves it running and comes back here.
//!
//! # Where USB fits
//!
//! `usb::poll()` is one register read when there is nothing to do, so it is
//! called liberally: once per pass, on every task switch, and on both sides of
//! a panel refresh. The refresh is the one place the loop is genuinely blind
//! for ~14 ms, which is well inside what a host tolerates.

use alloc::boxed::Box;
use alloc::string::String;

use bao1x_hal::sh1107::{COLUMN, Mono, Oled128x128, ROW};
use ux_api::minigfx::{ColorNative, FrameBuffer, Point};

use crate::anim::{Demo, Fire, MatrixRain, Plasma};
use crate::badgy::Badgy;
use crate::gfx::{self, CHAR_HEIGHT};
use crate::input::{ALL_KEYS, Key, KeySet, Keys};
use crate::mascot;
use crate::menu::{self, Action, ItemList, MenuDef, MenuView, ScriptList};
use crate::platform;
use crate::sched::{self, Ended, Status};
use crate::scripts::Scripts;
use crate::usb;
use crate::util::{FmtBuf, hash3};

/// Gap between key-matrix samples.
const POLL_MS: usize = 4;
/// Polls per animation frame. With the draw itself costing ~14ms this lands
/// around 20fps, which is as smooth as the panel's own refresh warrants.
const FRAME_POLLS: u32 = 8;
/// How long the wheel button has to be held to leave the button test.
const HOLD_EXIT_POLLS: u16 = 200;
/// Polls between repaints of a screen whose contents change on their own.
const SLOW_REFRESH_POLLS: u32 = 200;
/// How long the wheel button has to be held on the drive screen to reformat.
/// Longer than the button-test exit, because this one destroys files.
const HOLD_FORMAT_POLLS: u16 = 400;
/// Consecutive polls the leave-it-running chord must be held for. Two, because
/// the UI's own debouncer has already filtered contact bounce by the time this
/// sees it -- this is only guarding against catching one key of the chord a
/// poll before the other.
const UNFOCUS_HOLD: u8 = 2;

/// Contrast is stored as a step index; the panel takes 0..=255.
const BRIGHTNESS_STEPS: u8 = 16;
const BRIGHTNESS_MIN: u8 = 0x0f;
const BRIGHTNESS_STEP: u8 = 0x10;
/// Index whose value matches `sh1107::DEFAULT_BRIGHTNESS` (0x3f).
const BRIGHTNESS_DEFAULT_IDX: u8 = 3;

const MAX_DEPTH: usize = 4;

/// Characters that fit across the panel in the 6x12 font.
const COLS: usize = (COLUMN / gfx::CHAR_WIDTH) as usize;

/// Where Badgy's top-left corner goes on the home screen. The sprite is 74 rows
/// tall, so this leaves the title band above him and the caption band below.
const BADGY_TOP: isize = 20;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Screen {
    /// The home screen: Badgy over the matrix rain.
    Splash,
    Menu,
    Demo,
    KeyTest,
    Brightness,
    SysInfo,
    About,
    /// Drive status, and the long-hold reformat.
    UsbDrive,
    /// What happened when a script finished.
    ScriptResult,
    /// Badgy's sprite sheet: one frame per detent of the wheel.
    Badgy,
    /// A running script's page, presented whole. The keys belong to it.
    ScriptView,
    /// What is running, what it is costing, and how to stop it.
    Tasks,
}

/// Which list a menu level is showing. Keeping the static tree as `&'static`
/// means the whole menu structure still lives in flash; only the scripts screen
/// is built at runtime.
#[derive(Copy, Clone)]
enum Src {
    Static(&'static MenuDef),
    Scripts,
}

#[derive(Copy, Clone)]
struct Level {
    src: Src,
    view: MenuView,
}

/// What the result screen is reporting.
enum Outcome {
    Finished,
    /// The user held the exit chord.
    Stopped,
    /// A syntax or runtime error, already formatted with its line number.
    Failed(String),
    /// Not a script at all -- something the badge did, like a reformat.
    Note(&'static str),
}

pub struct App {
    screen: Screen,
    /// Open menus, innermost last. `stack[0]` is always the root menu.
    stack: [Level; MAX_DEPTH],
    depth: usize,

    /// The badger, and what mood he is in.
    badgy: Badgy,

    rain: MatrixRain,
    fire: Fire,
    plasma: Plasma,
    demo: Demo,

    /// The scripts on the USB drive, and the machinery that keeps them current.
    scripts: Scripts,
    /// Name and result of the last script run, for the result screen.
    last_script: String,
    last_outcome: Outcome,

    brightness_idx: u8,
    flipped: bool,

    /// Set when the screen contents have changed and need pushing to the panel.
    dirty: bool,
    /// Polls since the last animation frame.
    frame_polls: u32,
    /// Consecutive polls the wheel button has been held on a hold-to-act screen.
    hold_polls: u16,
    /// False until the wheel button has been seen released on such a screen.
    hold_armed: bool,
    /// Frame counter, used by the splash's glitch effect.
    frame: u32,
    /// Free-running poll counter, for screens that refresh on a slow cadence.
    slow_polls: u32,
    /// Which frame the sprite-sheet screen is showing.
    sheet: usize,

    /// Cursor in the task manager, as a row index.
    task_cursor: usize,
    /// Consecutive polls the leave-it-running chord has been held.
    unfocus_polls: u8,
    /// Slots whose ending has already been reported. Without this the loop
    /// would announce a finished background script on every pass until its row
    /// was dismissed.
    noticed: u8,

    perclk: u32,
}

impl App {
    pub fn new(perclk: u32) -> Self {
        App {
            screen: Screen::Splash,
            stack: [Level { src: Src::Static(&menu::MAIN), view: MenuView { cursor: 0, top: 0 } }; MAX_DEPTH],
            depth: 0,
            badgy: Badgy::new(),
            rain: MatrixRain::new(0xd0f_c034),
            fire: Fire::new(0x5eed_1337),
            plasma: Plasma::new(),
            demo: Demo::Matrix,
            scripts: Scripts::new(),
            last_script: String::new(),
            last_outcome: Outcome::Finished,
            brightness_idx: BRIGHTNESS_DEFAULT_IDX,
            flipped: false,
            dirty: true,
            frame_polls: 0,
            hold_polls: 0,
            hold_armed: false,
            frame: 0,
            slow_polls: 0,
            sheet: 0,
            task_cursor: 0,
            unfocus_polls: 0,
            noticed: 0,
            perclk,
        }
    }

    /// Restore the script volume and take an inventory.
    ///
    /// Runs before USB is attached: on a first boot this formats the volume and
    /// writes half a megabyte to ReRAM, and doing that while a host is watching
    /// the drive would look like the device hanging. The volume ID it needs
    /// comes from the chip's own serial, not from the USB stack.
    pub fn init_storage(&mut self) { self.scripts.init(); }

    /// Never returns: this is the whole firmware after bring-up.
    pub fn run(mut self, disp: &mut Oled128x128<'_>, keys: &mut Keys) -> ! {
        // Claim slot 0 before anything can be spawned into the others. From
        // here on this function is a task like any other -- it just happens to
        // be the one that owns the screen.
        sched::init();

        loop {
            usb::poll();

            // The importer only acts when the drive has been quiet for a
            // while, so this is nearly free on every other pass.
            if self.scripts.poll() && self.showing_scripts() {
                self.clamp_cursor();
                self.dirty = true;
            }

            self.reap_finished(keys);

            let fired = keys.poll();
            if self.screen == Screen::ScriptView {
                // The script has the keys. All this looks for is the chord that
                // takes the screen back without stopping it.
                self.watch_unfocus(keys);
            } else if fired.any() {
                self.badgy.poke();
                self.on_keys(fired, disp, keys);
            }

            self.update_holds(keys);

            // The drive screen shows link state and free space, which change
            // without anyone pressing anything -- plugging a cable in, or the
            // host finishing a copy. Repaint it about once a second rather than
            // adding it to the animation set, where it would cost a 14 ms panel
            // refresh twenty times a second to show nothing new.
            self.slow_polls = self.slow_polls.wrapping_add(1);
            if matches!(self.screen, Screen::UsbDrive | Screen::Tasks)
                && self.slow_polls % SLOW_REFRESH_POLLS == 0
            {
                self.dirty = true;
            }

            self.frame_polls += 1;
            if self.animated() && self.frame_polls >= FRAME_POLLS {
                self.frame_polls = 0;
                self.advance();
                self.dirty = true;
            }

            if self.screen == Screen::ScriptView {
                // Present a frame only when the script has one waiting. That
                // handshake is also what paces the script: its `show()` blocks
                // until this happens.
                let tid = sched::focus();
                if tid != sched::UI && sched::pending(tid) {
                    sched::present(tid, disp);
                    usb::poll();
                    disp.draw().ok();
                    usb::poll();
                }
                self.dirty = false;
            } else if self.dirty {
                self.dirty = false;
                self.render(disp, keys);
                usb::poll();
                disp.draw().ok();
                usb::poll();
            }

            // Keep the loop's cadence, but spend the wait running scripts
            // rather than spinning. With nothing else alive this is the busy
            // poll it replaced, to the millisecond.
            sched::pace(POLL_MS as u32);
        }
    }

    /// Notice tasks that have ended.
    ///
    /// A script that finishes in the background has nowhere to report to, so
    /// this is where its result is picked up: the console line, Badgy's mood if
    /// it crashed, and -- if it was the one on screen -- the result screen.
    /// Finished slots are otherwise left alone, so the manager can show what
    /// happened until the room is needed.
    fn reap_finished(&mut self, keys: &mut Keys) {
        for tid in 1..sched::MAX_TASKS {
            if sched::status(tid) != Status::Done {
                // Clears itself when the slot is reused, so a later task in the
                // same slot is not mistaken for one already reported.
                self.noticed &= !(1 << tid);
                continue;
            }
            if self.noticed & (1 << tid) != 0 {
                continue;
            }
            self.noticed |= 1 << tid;

            let focused = sched::focus() == tid;
            let outcome = match sched::ended(tid) {
                Ended::Failed => Outcome::Failed(String::from(sched::message(tid))),
                Ended::Stopped => Outcome::Stopped,
                _ => Outcome::Finished,
            };
            if matches!(outcome, Outcome::Failed(_)) {
                self.badgy.upset();
            }

            if focused {
                self.last_script = String::from(sched::name(tid));
                self.last_outcome = outcome;
                // The script has been reading the matrix directly this whole
                // time, so the UI's debouncer is stale -- and whatever key
                // stopped it is probably still held. Adopt the current state
                // rather than let it read as a press that dismisses the result
                // screen instantly.
                keys.resync();
                sched::set_focus(sched::UI);
                sched::reap(tid);
                self.screen = Screen::ScriptResult;
                self.dirty = true;
            }
        }
    }

    /// While a script is on screen, watch for the chord that takes the screen
    /// back and leaves it running.
    fn watch_unfocus(&mut self, keys: &mut Keys) {
        let held = keys.held();
        if !(held.has(Key::Left) && held.has(Key::Right)) {
            self.unfocus_polls = 0;
            return;
        }
        self.unfocus_polls = self.unfocus_polls.saturating_add(1);
        if self.unfocus_polls < UNFOCUS_HOLD {
            return;
        }
        self.unfocus_polls = 0;
        sched::set_focus(sched::UI);
        keys.resync();
        self.screen = Screen::Tasks;
        self.task_cursor = 0;
        self.dirty = true;
    }

    /// The two screens where holding the wheel button does something, tracked
    /// together because the arming rule is the same: the press that opened the
    /// screen must be released before the timer starts, or entering the screen
    /// would immediately begin acting on it.
    fn update_holds(&mut self, keys: &Keys) {
        if !matches!(self.screen, Screen::KeyTest | Screen::UsbDrive) {
            return;
        }
        if !keys.held().has(Key::Select) {
            self.hold_armed = true;
            if self.hold_polls != 0 {
                self.hold_polls = 0;
                self.dirty = true;
            }
            return;
        }
        if !self.hold_armed {
            return;
        }
        self.hold_polls = self.hold_polls.saturating_add(1);
        let limit = if self.screen == Screen::KeyTest { HOLD_EXIT_POLLS } else { HOLD_FORMAT_POLLS };
        if self.hold_polls >= limit {
            self.hold_polls = 0;
            self.hold_armed = false;
            match self.screen {
                Screen::KeyTest => self.screen = Screen::Menu,
                Screen::UsbDrive => self.reformat(),
                _ => (),
            }
        }
        // The progress bar moves, so repaint.
        self.dirty = true;
    }

    /// Screens that repaint on their own, whether or not anything was pressed.
    fn animated(&self) -> bool { matches!(self.screen, Screen::Splash | Screen::Demo | Screen::KeyTest) }

    fn advance(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        match self.screen {
            Screen::Splash => {
                self.rain.step();
                self.badgy
                    .step(crate::badgy::State { busy: self.scripts.busy(), mounted: usb::is_configured() });
            }
            Screen::Demo => match self.demo {
                Demo::Matrix => self.rain.step(),
                Demo::Fire => self.fire.step(),
                Demo::Plasma => self.plasma.step(),
            },
            // KeyTest does not animate, but it does have to keep up with the
            // matrix, so it rides the same cadence.
            _ => (),
        }
    }

    // ------------------------------------------------------------------ menus

    fn showing_scripts(&self) -> bool {
        self.screen == Screen::Menu && matches!(self.stack[self.depth].src, Src::Scripts)
    }

    /// Run `f` against whichever list the current level shows.
    ///
    /// The two arms have different types, so this cannot just return a `&dyn`
    /// -- the dynamic one is a temporary built from `self.scripts`.
    fn with_list<R>(&self, f: impl FnOnce(&dyn ItemList) -> R) -> R {
        match self.stack[self.depth].src {
            Src::Static(d) => f(d),
            Src::Scripts => f(&ScriptList { scripts: &self.scripts }),
        }
    }

    /// Keep the cursor inside a list that shrank underneath it -- which happens
    /// whenever a file is deleted from the drive while its menu is open.
    fn clamp_cursor(&mut self) {
        let len = self.with_list(|l| l.len());
        let view = self.view_mut();
        if view.cursor >= len {
            view.cursor = len.saturating_sub(1);
        }
        // Recompute the scroll offset properly rather than just clamping `top`
        // to the cursor: a list that *grew* would otherwise stay scrolled where
        // it was, with the cursor off the bottom of the screen.
        view.reveal(len);
    }

    // ------------------------------------------------------------------ input

    fn on_keys(&mut self, fired: KeySet, disp: &mut Oled128x128<'_>, keys: &mut Keys) {
        match self.screen {
            Screen::Splash => {
                self.depth = 0;
                self.screen = Screen::Menu;
                self.dirty = true;
            }
            Screen::Menu => {
                let len = self.with_list(|l| l.len());
                if fired.has(Key::Up) {
                    self.view_mut().step(-1, len);
                    self.dirty = true;
                }
                if fired.has(Key::Down) {
                    self.view_mut().step(1, len);
                    self.dirty = true;
                }
                // Chords are possible -- two switches can be down at once -- so
                // these are exclusive: activating and going back in the same
                // poll would leave the stack somewhere nobody asked for.
                if fired.has(Key::Select) || fired.has(Key::Center) || fired.has(Key::Right) {
                    // On the scripts list, RIGHT starts one without giving it
                    // the screen. Everywhere else it is just another "select":
                    // there is nothing else in the menu tree that could
                    // meaningfully happen in the background.
                    let background = fired.has(Key::Right)
                        && !fired.has(Key::Select)
                        && !fired.has(Key::Center)
                        && self.showing_scripts();
                    self.activate(background, disp, keys);
                } else if fired.has(Key::Left) {
                    self.back();
                }
            }
            Screen::Demo => {
                self.screen = Screen::Menu;
                self.dirty = true;
            }
            Screen::KeyTest => {
                // Every key is under test here, so nothing means "leave" -- just
                // repaint, and mirror the press onto the serial console so the
                // matrix can be checked with the badge face-down on a bench.
                for k in ALL_KEYS {
                    if fired.has(k) {
                        crate::println!("key: {}", k.name());
                    }
                }
                self.dirty = true;
            }
            Screen::Brightness => {
                if fired.has(Key::Up) && self.brightness_idx + 1 < BRIGHTNESS_STEPS {
                    self.brightness_idx += 1;
                    self.apply_brightness(disp);
                }
                if fired.has(Key::Down) && self.brightness_idx > 0 {
                    self.brightness_idx -= 1;
                    self.apply_brightness(disp);
                }
                if fired.has(Key::Select) || fired.has(Key::Left) || fired.has(Key::Center) {
                    self.screen = Screen::Menu;
                    self.dirty = true;
                }
            }
            Screen::UsbDrive => {
                // The wheel button is the reformat hold, so only the other keys
                // leave. Otherwise a tap of the button that opened this screen
                // would close it before the hold could ever start.
                if fired.has(Key::Left) || fired.has(Key::Right) || fired.has(Key::Center) {
                    self.screen = Screen::Menu;
                    self.dirty = true;
                }
            }
            Screen::Badgy => {
                let n = self.sheet_len();
                if fired.has(Key::Up) {
                    self.sheet = (self.sheet + n - 1) % n;
                } else if fired.has(Key::Down) {
                    self.sheet = (self.sheet + 1) % n;
                } else {
                    self.screen = Screen::Menu;
                }
                self.dirty = true;
            }
            Screen::Tasks => self.on_task_keys(fired, keys),
            // Handled in `watch_unfocus`, which runs instead of this: while a
            // script is on screen every key is the script's.
            Screen::ScriptView => (),
            Screen::SysInfo | Screen::About | Screen::ScriptResult => {
                self.screen = Screen::Menu;
                self.dirty = true;
            }
        }
    }

    /// The task manager's keys: wheel to pick a row, push to bring it to the
    /// front, RIGHT to stop it (or to clear a finished one), LEFT to leave.
    ///
    /// RIGHT rather than a long hold, because the thing it does is already the
    /// recovery action -- a hold-to-confirm on "stop the runaway script" would
    /// be a confirmation prompt on the fire alarm.
    fn on_task_keys(&mut self, fired: KeySet, keys: &mut Keys) {
        // The list shrinks underneath the cursor whenever a task is reaped, so
        // this is checked here rather than only where rows are removed.
        let rows = self.task_rows();
        if self.task_cursor >= rows {
            self.task_cursor = rows - 1;
        }
        if fired.has(Key::Up) {
            self.task_cursor = (self.task_cursor + rows - 1) % rows;
            self.dirty = true;
        }
        if fired.has(Key::Down) {
            self.task_cursor = (self.task_cursor + 1) % rows;
            self.dirty = true;
        }
        let Some(tid) = self.task_at(self.task_cursor) else {
            // The "Back" row. RIGHT here is the panic button: stop everything.
            if fired.has(Key::Right) && sched::running() > 0 {
                crate::println!("stopping every task");
                sched::kill_all();
                self.dirty = true;
            } else if fired.has(Key::Select) || fired.has(Key::Center) || fired.has(Key::Left) {
                self.screen = Screen::Menu;
                self.dirty = true;
            }
            return;
        };

        if fired.has(Key::Left) {
            self.screen = Screen::Menu;
            self.dirty = true;
        } else if fired.has(Key::Select) || fired.has(Key::Center) {
            if sched::status(tid) == Status::Done {
                // Nothing to look at, so show what happened instead.
                self.last_script = String::from(sched::name(tid));
                self.last_outcome = match sched::ended(tid) {
                    Ended::Failed => Outcome::Failed(String::from(sched::message(tid))),
                    Ended::Stopped => Outcome::Stopped,
                    _ => Outcome::Finished,
                };
                sched::reap(tid);
                self.screen = Screen::ScriptResult;
            } else {
                sched::set_focus(tid);
                keys.resync();
                self.screen = Screen::ScriptView;
            }
            self.dirty = true;
        } else if fired.has(Key::Right) {
            if sched::status(tid) == Status::Done {
                sched::reap(tid);
            } else {
                crate::println!("task {}: stop requested", tid);
                sched::kill(tid);
            }
            self.task_cursor = self.task_cursor.min(self.task_rows().saturating_sub(1));
            self.dirty = true;
        }
    }

    /// Rows in the manager: one per occupied slot, plus "Back".
    fn task_rows(&self) -> usize { sched::occupied() + 1 }

    /// The task a row points at, or `None` for the "Back" row.
    fn task_at(&self, row: usize) -> Option<usize> {
        (1..sched::MAX_TASKS).filter(|&t| sched::used(t)).nth(row)
    }

    fn activate(&mut self, background: bool, disp: &mut Oled128x128<'_>, keys: &mut Keys) {
        // Read the action out before doing anything: `with_list` borrows
        // `self`, and everything below needs it mutably.
        let cursor = self.view().cursor;
        let action = self.with_list(|l| if cursor < l.len() { l.action(cursor) } else { Action::Back });
        let label =
            self.with_list(|l| if cursor < l.len() { String::from(l.label(cursor)) } else { String::new() });
        crate::println!("menu: {}", label);

        match action {
            Action::Submenu(def) => self.push(Src::Static(def)),
            Action::Scripts => self.push(Src::Scripts),
            Action::RunScript(i) => self.start_script(i as usize, !background, keys),
            Action::Tasks => {
                self.task_cursor = 0;
                self.screen = Screen::Tasks;
            }
            Action::UsbDrive => {
                self.hold_polls = 0;
                self.hold_armed = false;
                self.screen = Screen::UsbDrive;
            }
            Action::Demo(which) => {
                self.demo = which;
                self.screen = Screen::Demo;
                self.frame_polls = FRAME_POLLS; // draw the first frame immediately
            }
            Action::KeyTest => {
                self.hold_polls = 0;
                self.hold_armed = false;
                self.screen = Screen::KeyTest;
            }
            Action::Brightness => self.screen = Screen::Brightness,
            Action::ToggleFlip => {
                self.flipped = !self.flipped;
                disp.flip_vertical(self.flipped).ok();
                // Rolling the wheel "up" has to keep meaning "up the list" once
                // the badge is upside down, so the key map turns over with it.
                keys.set_flipped(self.flipped);
            }
            Action::SysInfo => self.screen = Screen::SysInfo,
            Action::Badgy => {
                self.sheet = 0;
                self.screen = Screen::Badgy;
            }
            Action::About => self.screen = Screen::About,
            Action::Back => self.back(),
        }
        self.dirty = true;
    }

    fn push(&mut self, src: Src) {
        if self.depth + 1 < MAX_DEPTH {
            self.depth += 1;
            self.stack[self.depth] = Level { src, view: MenuView::default() };
        }
    }

    fn back(&mut self) {
        if self.depth > 0 {
            self.depth -= 1;
        } else {
            self.screen = Screen::Splash;
        }
        self.dirty = true;
    }

    fn view(&self) -> &MenuView { &self.stack[self.depth].view }

    fn view_mut(&mut self) -> &mut MenuView { &mut self.stack[self.depth].view }

    fn apply_brightness(&mut self, disp: &mut Oled128x128<'_>) {
        disp.brightness(BRIGHTNESS_MIN + self.brightness_idx * BRIGHTNESS_STEP).ok();
        self.dirty = true;
    }

    // ---------------------------------------------------------------- scripts

    /// Compile script `i` and hand it to the scheduler.
    ///
    /// Compiling happens here, on the UI's stack, rather than inside the new
    /// task: lexing and parsing hold the token vector and the AST at once and
    /// peak near 250 KB, so keeping it out of the task means two of those peaks
    /// never overlap -- and a syntax error is reported here and now, without a
    /// task ever having existed.
    ///
    /// `focus` decides whether the script gets the screen. Either way it is a
    /// task from the moment it starts; running one in the foreground is only
    /// running one whose page is being presented.
    fn start_script(&mut self, i: usize, focus: bool, keys: &mut Keys) {
        self.last_script = String::from(self.scripts.name(i));
        crate::println!("starting {}", self.last_script);

        let Some(src) = self.scripts.source(i) else {
            self.fail_to_start("could not read the file");
            return;
        };

        let script = match pycon::Script::compile(&src) {
            Ok(s) => s,
            Err(e) => {
                self.fail_to_start(&alloc::format!("{}", e));
                return;
            }
        };

        // Seed the script's random source from something that differs between
        // runs. There is no clock a script can read and the TRNG is never
        // powered up in this firmware, so the frame counter, the source length
        // and the millisecond count are what is available -- good enough for an
        // animation, and documented as not cryptographic.
        let seed = hash3(self.frame ^ platform::now_ms(), src.len() as u32, self.scripts.generation);
        // The source is a large allocation and the new task does not need it --
        // it holds the parsed form. Dropping it here keeps the peak down.
        drop(src);

        match sched::spawn(self.scripts.name(i), Box::new(script), seed) {
            Err(e) => self.fail_to_start(e.message()),
            Ok(tid) => {
                if focus {
                    sched::set_focus(tid);
                    keys.resync();
                    self.screen = Screen::ScriptView;
                } else {
                    // Stay where we are, so a second script can be started from
                    // the same list without walking back into it.
                    self.dirty = true;
                }
            }
        }
    }

    /// A script that never got as far as being a task: unreadable, unparseable,
    /// or nowhere to put it.
    fn fail_to_start(&mut self, why: &str) {
        crate::println!("{}: {}", self.last_script, why);
        self.last_outcome = Outcome::Failed(String::from(why));
        self.badgy.upset();
        self.screen = Screen::ScriptResult;
    }

    /// Wipe the drive back to a fresh volume with the sample files on it.
    fn reformat(&mut self) {
        crate::println!("reformatting the script drive");
        crate::store::clear();
        self.scripts.init();
        // The host is holding a cached view of the volume that no longer
        // exists, and if it writes that back it corrupts the new one. Dropping
        // off the bus and returning is the only way to be sure it is discarded.
        usb::reattach();
        self.last_script = String::from("script drive");
        self.last_outcome = Outcome::Note("reformatted, with the sample files back on it");
        self.screen = Screen::ScriptResult;
    }

    // ----------------------------------------------------------------- render

    fn render(&self, disp: &mut Oled128x128<'_>, keys: &Keys) {
        let fb: &mut dyn FrameBuffer = disp;
        fb.clear();
        match self.screen {
            Screen::Splash => self.render_splash(fb),
            Screen::Menu => {
                if self.showing_scripts() && self.scripts.is_empty() {
                    self.render_no_scripts(fb);
                } else {
                    self.with_list(|l| menu::render(fb, l, self.view()));
                }
            }
            Screen::Demo => {
                match self.demo {
                    Demo::Matrix => self.rain.render(fb),
                    Demo::Fire => self.fire.render(fb),
                    Demo::Plasma => self.plasma.render(fb),
                }
                // A one-line caption, knocked into the bottom of the frame so
                // it stays readable over whatever the animation is doing.
                caption(fb, self.demo.title());
            }
            Screen::KeyTest => self.render_key_test(fb, keys),
            Screen::Brightness => self.render_brightness(fb),
            Screen::SysInfo => self.render_sys_info(fb),
            Screen::About => self.render_about(fb),
            Screen::UsbDrive => self.render_usb(fb),
            Screen::Badgy => self.render_badgy_sheet(fb),
            Screen::ScriptResult => self.render_result(fb),
            Screen::Tasks => self.render_tasks(fb),
            // Not drawn here at all: the compositor copies the focused task's
            // page onto the panel instead. Reached only if a repaint is
            // requested in the same pass as the switch to this screen.
            Screen::ScriptView => (),
        }
    }

    /// What is running, what it costs, and how to stop it.
    fn render_tasks(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();
        let mut buf = FmtBuf::<32>::new();

        header(fb, "TASKS");
        let mut y = CHAR_HEIGHT + 4;

        for row in 0..self.task_rows() {
            let selected = row == self.task_cursor;
            let (fg, bg) = if selected { (dark, lit) } else { (lit, dark) };
            if selected {
                gfx::fill_rect(fb, Point::new(0, y), Point::new(COLUMN - 1, y + CHAR_HEIGHT - 1), lit);
            }
            match self.task_at(row) {
                None => gfx::msg(fb, "Back", Point::new(4, y), fg, bg),
                Some(tid) => {
                    // Name, then status and cost in a fixed right-hand column,
                    // so the numbers line up between rows and a long filename
                    // cannot push them off the edge.
                    let name = clip(sched::name(tid), 11);
                    let text = buf.format(format_args!(
                        "{:<11} {} {:>3}%",
                        name,
                        sched::status(tid).abbrev(),
                        sched::cpu_percent(tid)
                    ));
                    gfx::msg(fb, text, Point::new(4, y), fg, bg);
                }
            }
            y += CHAR_HEIGHT;
        }

        // Detail for the selected task, below the list. Two numbers worth
        // seeing on real hardware rather than believing from a comment: how
        // deep the stack has actually gone, and what is left of the shared
        // heap.
        y = ROW - CHAR_HEIGHT * 4 - 2;
        match self.task_at(self.task_cursor) {
            Some(tid) => {
                if let Some((used, total)) = sched::stack_used(tid) {
                    gfx::msg(
                        fb,
                        buf.format(format_args!("stack {:2}/{} KiB", used / 1024, total / 1024)),
                        Point::new(3, y),
                        lit,
                        dark,
                    );
                }
                y += CHAR_HEIGHT;
                gfx::msg(
                    fb,
                    buf.format(format_args!("heap  {:3} KiB free", platform::heap_free() / 1024)),
                    Point::new(3, y),
                    lit,
                    dark,
                );
                y += CHAR_HEIGHT;
                let hint = if sched::status(tid) == Status::Done {
                    "IN: result R: clear"
                } else {
                    "IN: view   R: stop"
                };
                gfx::msg(fb, hint, Point::new(3, y), lit, dark);
            }
            None => {
                gfx::msg(
                    fb,
                    buf.format(format_args!("{} of {} slots used", sched::occupied(), sched::SCRIPT_SLOTS)),
                    Point::new(3, y),
                    lit,
                    dark,
                );
                y += CHAR_HEIGHT;
                gfx::msg(fb, "R in the script list", Point::new(3, y), lit, dark);
                y += CHAR_HEIGHT;
                gfx::msg(fb, "starts one hidden", Point::new(3, y), lit, dark);
                y += CHAR_HEIGHT;
                if sched::running() > 0 {
                    gfx::msg(fb, "R here: stop all", Point::new(3, y), lit, dark);
                }
            }
        }
    }

    /// Home screen: Badgy, over the rain, under the name.
    ///
    /// The badger is drawn from a three-state sprite -- lit, dark, transparent --
    /// so he occludes the animation instead of having it show through him, and
    /// no plate has to be cut out of the background to make him readable.
    fn render_splash(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();

        self.rain.render(fb);

        // Title, in a band knocked out of the rain so the glitch effect reads.
        gfx::fill_rect(fb, Point::new(0, 0), Point::new(COLUMN - 1, 15), dark);
        glitched_centered(fb, "BadgyOS", 2, self.frame, lit, dark);

        gfx::sprite_centered(fb, self.badgy.art(), COLUMN, BADGY_TOP, lit, dark);

        // A script running in the background is otherwise invisible from the
        // home screen, which is where the badge spends most of its life. One
        // glyph in the corner of the title band is enough to say so.
        let live = sched::running();
        if live > 0 {
            let mut buf = FmtBuf::<8>::new();
            let text = buf.format(format_args!("{}*", live));
            gfx::msg(fb, text, Point::new(COLUMN - 2 * gfx::CHAR_WIDTH - 2, 2), lit, dark);
        }

        // Caption on its own dark band, for the same reason as the title.
        let y = ROW - CHAR_HEIGHT - 3;
        gfx::fill_rect(fb, Point::new(0, y - 2), Point::new(COLUMN - 1, ROW - 1), dark);
        gfx::msg_centered(fb, self.badgy.caption(), COLUMN, y, lit, dark);
    }

    /// Every frame of the sprite sheet, one per detent, with its index.
    ///
    /// This is here because the art is data: a frame that renders wrong on the
    /// panel but fine in `tools/badger.py`'s PNG has nowhere else to show
    /// itself. It also settles the "does the sprite blitter clip correctly"
    /// question on real hardware in about two seconds.
    ///
    /// Frames a script injected are on the end of the roll, which is the only
    /// way to look at one on its own -- on the home screen it is composited over
    /// the rain, and in the script that made it, it may never be drawn at all.
    fn render_badgy_sheet(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();
        let mut buf = FmtBuf::<32>::new();

        let sheet = crate::sprites::ALL;
        let n = self.sheet_len();
        let i = self.sheet.min(n - 1);
        header(fb, "BADGY");
        let label = if i < sheet.len() {
            gfx::sprite_centered(fb, sheet[i], COLUMN, BADGY_TOP - 4, lit, dark);
            buf.format(format_args!("frame {}/{}", i + 1, n))
        } else {
            match mascot::filled().nth(i - sheet.len()) {
                Some((id, art)) => {
                    gfx::sprite_centered(fb, art, COLUMN, BADGY_TOP - 4, lit, dark);
                    buf.format(format_args!("frame {}/{} slot {}", i + 1, n, id))
                }
                // A script ended and its slot was recycled between the key press
                // and this repaint. Nothing to draw, and nothing worth saying
                // beyond that it went.
                None => buf.format(format_args!("frame {}/{} gone", i + 1, n)),
            }
        };
        gfx::msg_centered(fb, label, COLUMN, ROW - CHAR_HEIGHT - 2, lit, dark);
    }

    /// Frames the sheet screen can page through: the built-in roll, plus
    /// whatever scripts have injected.
    fn sheet_len(&self) -> usize { crate::sprites::ALL.len() + mascot::filled().count() }

    fn render_no_scripts(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();
        header(fb, "SCRIPTS");
        let mut y = 20;
        for text in [
            "No .py files yet.",
            "",
            "Plug the badge into",
            "a computer, open the",
            "BADGYOS drive and",
            "drop a .py file on",
            "it. See readme.txt.",
            "",
            // Only the buttons leave: the wheel still scrolls the (one-item)
            // list underneath this hint.
            "push: back",
        ] {
            gfx::msg(fb, text, Point::new(3, y), lit, dark);
            y += CHAR_HEIGHT;
        }
    }

    fn render_key_test(&self, fb: &mut dyn FrameBuffer, keys: &Keys) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();
        let held = keys.held();
        let mark = |k: Key| if held.has(k) { '#' } else { ' ' };
        let mut buf = FmtBuf::<32>::new();

        header(fb, "BUTTON TEST");
        let line = |fb: &mut dyn FrameBuffer, text: &str, y: isize| {
            gfx::msg(fb, text, Point::new(4, y), lit, dark);
        };

        let mut y = 16;
        line(fb, "WHEEL      BUTTONS", y);
        y += CHAR_HEIGHT;
        line(fb, buf.format(format_args!("UP  [{}]    L  [{}]", mark(Key::Up), mark(Key::Left))), y);
        y += CHAR_HEIGHT;
        line(fb, buf.format(format_args!("IN  [{}]    C  [{}]", mark(Key::Select), mark(Key::Center))), y);
        y += CHAR_HEIGHT;
        line(fb, buf.format(format_args!("DN  [{}]    R  [{}]", mark(Key::Down), mark(Key::Right))), y);
        y += CHAR_HEIGHT * 2;

        // Bit order is `Key`'s: center, right, left, up, select, down.
        line(fb, buf.format(format_args!("mask 0b{:06b}", held.0)), y);
        y += CHAR_HEIGHT * 2;
        line(fb, "hold IN to exit", y);

        hold_bar(fb, self.hold_polls, HOLD_EXIT_POLLS);
    }

    fn render_brightness(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();
        let mut buf = FmtBuf::<32>::new();

        header(fb, "BRIGHTNESS");
        gfx::msg_centered(
            fb,
            buf.format(format_args!(
                "{:2} / {}  (0x{:02x})",
                self.brightness_idx + 1,
                BRIGHTNESS_STEPS,
                BRIGHTNESS_MIN + self.brightness_idx * BRIGHTNESS_STEP
            )),
            COLUMN,
            26,
            lit,
            dark,
        );

        // Segmented bar: one block per step, filled up to the current level.
        let tl = Point::new(6, 48);
        let br = Point::new(COLUMN - 7, 68);
        gfx::stroke_rect(fb, tl, br, lit);
        let seg = (br.x - tl.x - 3) / BRIGHTNESS_STEPS as isize;
        for i in 0..=self.brightness_idx as isize {
            let x = tl.x + 2 + i * seg;
            gfx::fill_rect(fb, Point::new(x, tl.y + 3), Point::new(x + seg - 2, br.y - 3), lit);
        }

        gfx::msg_centered(fb, "wheel: adjust", COLUMN, 88, lit, dark);
        gfx::msg_centered(fb, "push:  done", COLUMN, 102, lit, dark);
    }

    fn render_usb(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();
        let mut buf = FmtBuf::<32>::new();

        header(fb, "USB DRIVE");
        let mut y = 16;
        let mut line = |fb: &mut dyn FrameBuffer, text: &str| {
            gfx::msg(fb, text, Point::new(3, y), lit, dark);
            y += CHAR_HEIGHT;
        };

        let (state, speed) = usb::status();
        line(fb, buf.format(format_args!("link  {}", state)));
        line(fb, buf.format(format_args!("speed {}", speed)));
        line(fb, if usb::is_configured() { "drive mounted" } else { "drive waiting" });
        line(fb, buf.format(format_args!("used  {:5} B", self.scripts.used)));
        line(fb, buf.format(format_args!("free  {:5} B", self.scripts.free)));
        line(fb, buf.format(format_args!("files {}", self.scripts.len())));
        line(fb, "");
        line(fb, "hold IN: format");

        hold_bar(fb, self.hold_polls, HOLD_FORMAT_POLLS);
    }

    fn render_result(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();

        let (title, body) = match &self.last_outcome {
            Outcome::Finished => ("DONE", None),
            Outcome::Stopped => ("STOPPED", None),
            Outcome::Failed(m) => ("ERROR", Some(m.as_str())),
            Outcome::Note(m) => ("USB DRIVE", Some(*m)),
        };
        header(fb, title);

        let mut y = 18;
        // The script's own name first -- with several on the drive, "ERROR" on
        // its own is not much use.
        gfx::msg(fb, &clip(&self.last_script, COLS), Point::new(3, y), lit, dark);
        y += CHAR_HEIGHT + 4;

        match body {
            Some(msg) => {
                // Wrap on whitespace so an error message reads as sentences
                // rather than being cut mid-word at column 21.
                for chunk in wrap(msg, COLS) {
                    if y > ROW - CHAR_HEIGHT * 2 {
                        break;
                    }
                    gfx::msg(fb, chunk, Point::new(3, y), lit, dark);
                    y += CHAR_HEIGHT;
                }
            }
            None => {
                gfx::msg(fb, "see the serial", Point::new(3, y), lit, dark);
                y += CHAR_HEIGHT;
                gfx::msg(fb, "console for print()", Point::new(3, y), lit, dark);
            }
        }
        gfx::msg(fb, "any key: back", Point::new(3, ROW - CHAR_HEIGHT - 2), lit, dark);
    }

    fn render_sys_info(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();
        let mut buf = FmtBuf::<32>::new();

        header(fb, "SYSTEM INFO");
        let mut y = 16;
        let mut line = |fb: &mut dyn FrameBuffer, text: &str| {
            gfx::msg(fb, text, Point::new(3, y), lit, dark);
            y += CHAR_HEIGHT;
        };

        line(fb, buf.format(format_args!("CPU    {:4} MHz", platform::SYSTEM_CLOCK_FREQUENCY / 2_000_000)));
        line(fb, buf.format(format_args!("PERCLK {:4} MHz", self.perclk / 1_000_000)));
        line(fb, buf.format(format_args!("SRAM   {:4} KiB", platform::RAM_SIZE / 1024)));
        line(fb, buf.format(format_args!("HEAP   {:4} KiB", platform::HEAP_LEN / 1024)));
        line(fb, buf.format(format_args!("DISK   {:4} KiB", crate::usb::msc::DISK_BYTES / 1024)));
        line(fb, buf.format(format_args!("IMG  0x{:08x}", bao1x_api::BAREMETAL_START)));
        line(fb, buf.format(format_args!("PANEL  {}x{} 1bpp", COLUMN, ROW)));
        line(fb, buf.format(format_args!("TASKS  {} of {} live", sched::running(), sched::SCRIPT_SLOTS)));
        line(fb, "");
        line(fb, "any key: back");
    }

    fn render_about(&self, fb: &mut dyn FrameBuffer) {
        let lit: ColorNative = Mono::White.into();
        let dark: ColorNative = Mono::Black.into();

        header(fb, "ABOUT");
        let mut y = 16;
        // Nine lines is what fits below the title bar. Anything added here has
        // to displace something.
        for text in [
            "BadgyOS + pycon",
            "for the DC34 badge",
            "",
            "wheel scroll, push",
            "select, left back",
            "right runs hidden",
            "L+C stops, L+R hides",
            "",
            "dev-signed: no k0",
        ] {
            gfx::msg(fb, text, Point::new(3, y), lit, dark);
            y += CHAR_HEIGHT;
        }
    }
}

/// The title bar shared by the non-menu screens.
fn header(fb: &mut dyn FrameBuffer, title: &str) {
    let lit: ColorNative = Mono::White.into();
    let dark: ColorNative = Mono::Black.into();
    gfx::fill_rect(fb, Point::new(0, 0), Point::new(COLUMN - 1, CHAR_HEIGHT + 1), lit);
    gfx::msg_centered(fb, title, COLUMN, 1, dark, lit);
}

/// Progress towards a long-hold action, along the bottom edge.
fn hold_bar(fb: &mut dyn FrameBuffer, held: u16, limit: u16) {
    let lit: ColorNative = Mono::White.into();
    let bar_tl = Point::new(4, ROW - 10);
    let bar_br = Point::new(COLUMN - 5, ROW - 3);
    gfx::stroke_rect(fb, bar_tl, bar_br, lit);
    let span = bar_br.x - bar_tl.x - 3;
    let filled = span * held as isize / limit as isize;
    if filled > 0 {
        gfx::fill_rect(
            fb,
            Point::new(bar_tl.x + 2, bar_tl.y + 2),
            Point::new(bar_tl.x + 1 + filled, bar_br.y - 2),
            lit,
        );
    }
}

/// A caption strip along the bottom edge, for use over an animation.
fn caption(fb: &mut dyn FrameBuffer, text: &str) {
    let lit: ColorNative = Mono::White.into();
    let dark: ColorNative = Mono::Black.into();
    // The band has to clear the animation's whole bottom cell row, which starts
    // three pixels above the caption baseline -- otherwise the tops of those
    // glyphs peek out as a dotted line.
    let y = ROW - CHAR_HEIGHT - 2;
    gfx::fill_rect(fb, Point::new(0, y - 3), Point::new(COLUMN - 1, ROW - 1), dark);
    gfx::msg_centered(fb, text, COLUMN, y, lit, dark);
}

/// Truncate to `cols` characters on a character boundary.
fn clip(s: &str, cols: usize) -> &str {
    match s.char_indices().nth(cols) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Break `s` into lines of at most `cols` characters, preferring spaces.
///
/// Returns an iterator so nothing is allocated: error messages are the only
/// caller and they are already on the heap.
fn wrap(s: &str, cols: usize) -> impl Iterator<Item = &str> {
    let mut rest = s;
    core::iter::from_fn(move || {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        if rest.chars().count() <= cols {
            let all = rest;
            rest = "";
            return Some(all);
        }
        // The last space at or before the column limit, so words stay whole.
        let hard = rest.char_indices().nth(cols).map(|(i, _)| i).unwrap_or(rest.len());
        let cut = rest[..hard].rfind(' ').map(|i| i).unwrap_or(hard);
        let (head, tail) = rest.split_at(cut);
        rest = tail;
        Some(head)
    })
}

/// Draw `text` centered, with the occasional character replaced by a random one
/// for a single frame. Cheap way to make a static title look alive next to the
/// rain without animating the letterforms themselves.
fn glitched_centered(
    fb: &mut dyn FrameBuffer,
    text: &str,
    y: isize,
    frame: u32,
    fg: ColorNative,
    bg: ColorNative,
) {
    const NOISE: &[u8] = b"#@%$&*/\\|<>=+";
    let x0 = (COLUMN - gfx::text_width(text)) / 2;
    for (i, c) in text.chars().enumerate() {
        let h = hash3(i as u32, frame, 0x9e37);
        let c = if h % 96 == 0 { NOISE[(h >> 8) as usize % NOISE.len()] as char } else { c };
        gfx::glyph(fb, c, Point::new(x0 + i as isize * gfx::CHAR_WIDTH, y), fg, bg);
    }
}
