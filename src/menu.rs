//! The menu: a scrolling list widget, the static menu tree it walks, and the
//! seam that lets a *dynamically* built list use the same widget.
//!
//! The static parts are `&'static` data in `.rodata` (which link.x maps into
//! flash), so the tree costs nothing at runtime beyond the cursor and scroll
//! offset that [`MenuView`] keeps for each open level.
//!
//! The scripts menu cannot be that, because its contents come off a USB drive
//! at runtime. Rather than make everything dynamic -- which would move the
//! whole tree into the heap to serve one screen -- the widget takes an
//! [`ItemList`], and both kinds implement it. `MenuDef` is unchanged.

use bao1x_hal::sh1107::{COLUMN, Mono, ROW};
use ux_api::minigfx::{ColorNative, FrameBuffer, Point};

use crate::anim::Demo;
use crate::gfx::{self, CHAR_HEIGHT};

/// What picking an item does.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Action {
    /// Descend into another menu.
    Submenu(&'static MenuDef),
    /// Run a full-screen animation until a key is pressed.
    Demo(Demo),
    /// Live view of the key matrix.
    KeyTest,
    /// Wheel-adjustable OLED contrast.
    Brightness,
    /// Rotate the panel 180 degrees (and the wheel's sense with it).
    ToggleFlip,
    SysInfo,
    About,
    /// Badgy's sprite sheet, one frame at a time.
    Badgy,
    /// Open the list of scripts found on the USB drive.
    Scripts,
    /// Run the script at this index in that list.
    RunScript(u8),
    /// The USB drive status screen.
    UsbDrive,
    /// Pop a level; at the root, return to the splash screen.
    Back,
}

/// A list the menu widget can draw and walk.
///
/// Deliberately by-index rather than by-iterator: a dynamic list is stored
/// somewhere the widget does not own, and handing out references to it would
/// mean borrowing the whole `App` for the duration of a repaint.
pub trait ItemList {
    fn title(&self) -> &str;
    fn len(&self) -> usize;
    fn label(&self, i: usize) -> &str;
    fn action(&self, i: usize) -> Action;
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: &'static str,
    pub action: Action,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct MenuDef {
    pub title: &'static str,
    pub items: &'static [MenuItem],
}

impl ItemList for MenuDef {
    fn title(&self) -> &str { self.title }

    fn len(&self) -> usize { self.items.len() }

    fn label(&self, i: usize) -> &str { self.items[i].label }

    fn action(&self, i: usize) -> Action { self.items[i].action }
}

// ------------------------------------------------------------- the menu tree

pub static DEMOS: MenuDef = MenuDef {
    title: "DEMOS",
    items: &[
        MenuItem { label: "Matrix Rain", action: Action::Demo(Demo::Matrix) },
        MenuItem { label: "ASCII Fire", action: Action::Demo(Demo::Fire) },
        MenuItem { label: "Plasma", action: Action::Demo(Demo::Plasma) },
        MenuItem { label: "Back", action: Action::Back },
    ],
};

pub static DISPLAY: MenuDef = MenuDef {
    title: "DISPLAY",
    items: &[
        MenuItem { label: "Brightness", action: Action::Brightness },
        MenuItem { label: "Flip 180", action: Action::ToggleFlip },
        MenuItem { label: "Back", action: Action::Back },
    ],
};

pub static MAIN: MenuDef = MenuDef {
    title: "BadgyOS",
    items: &[
        MenuItem { label: "Scripts", action: Action::Scripts },
        MenuItem { label: "USB Drive", action: Action::UsbDrive },
        MenuItem { label: "Demos", action: Action::Submenu(&DEMOS) },
        MenuItem { label: "Button Test", action: Action::KeyTest },
        MenuItem { label: "Display", action: Action::Submenu(&DISPLAY) },
        MenuItem { label: "System Info", action: Action::SysInfo },
        MenuItem { label: "Badgy", action: Action::Badgy },
        MenuItem { label: "About", action: Action::About },
        MenuItem { label: "Home Screen", action: Action::Back },
    ],
};

/// The scripts on the drive, as a menu.
///
/// Built fresh for each repaint from whatever [`crate::scripts::Scripts`]
/// currently holds, which is why it borrows rather than owns: the list can
/// change between one frame and the next when a host writes to the drive.
pub struct ScriptList<'a> {
    pub scripts: &'a crate::scripts::Scripts,
}

impl ItemList for ScriptList<'_> {
    fn title(&self) -> &str { "SCRIPTS" }

    /// One extra item for "Back", which also guarantees the list is never
    /// empty -- a zero-length menu would leave the cursor pointing at nothing.
    fn len(&self) -> usize { self.scripts.len() + 1 }

    fn label(&self, i: usize) -> &str { if i < self.scripts.len() { self.scripts.name(i) } else { "Back" } }

    fn action(&self, i: usize) -> Action {
        if i < self.scripts.len() { Action::RunScript(i as u8) } else { Action::Back }
    }
}

// ----------------------------------------------------------------- the widget

const HEADER_H: isize = CHAR_HEIGHT + 2;
const ITEMS_TOP: isize = HEADER_H + 2;
const ITEM_H: isize = CHAR_HEIGHT;
const SCROLLBAR_W: isize = 3;
const LABEL_X: isize = 4;

/// How many items fit below the title bar.
pub const VISIBLE: usize = ((ROW - ITEMS_TOP) / ITEM_H) as usize;

/// Where a given menu level is scrolled to. One of these per level on the stack,
/// so backing out of a submenu puts the cursor back where it was.
#[derive(Debug, Copy, Clone, Default)]
pub struct MenuView {
    pub cursor: usize,
    pub top: usize,
}

impl MenuView {
    /// Move the cursor by `delta`, wrapping at both ends, and scroll to follow.
    pub fn step(&mut self, delta: isize, len: usize) {
        if len == 0 {
            return;
        }
        let len_i = len as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(len_i) as usize;
        self.scroll_into_view(len);
    }

    /// Bring the cursor back on screen without moving it. Used after the list
    /// itself changed, where `step` would be wrong -- there is no delta.
    pub fn reveal(&mut self, len: usize) { self.scroll_into_view(len); }

    fn scroll_into_view(&mut self, len: usize) {
        if len <= VISIBLE {
            self.top = 0;
            return;
        }
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + VISIBLE {
            self.top = self.cursor - VISIBLE + 1;
        }
        self.top = self.top.min(len - VISIBLE);
    }
}

/// Paint a menu. The caller is expected to have cleared the framebuffer.
pub fn render(fb: &mut dyn FrameBuffer, list: &dyn ItemList, view: &MenuView) {
    let lit: ColorNative = Mono::White.into();
    let dark: ColorNative = Mono::Black.into();

    // Title bar: solid, with the text knocked out of it.
    gfx::fill_rect(fb, Point::new(0, 0), Point::new(COLUMN - 1, HEADER_H - 1), lit);
    gfx::msg_centered(fb, list.title(), COLUMN, 1, dark, lit);

    let len = list.len();
    let scrolls = len > VISIBLE;
    let right = if scrolls { COLUMN - 1 - SCROLLBAR_W - 1 } else { COLUMN - 1 };

    for slot in 0..VISIBLE {
        let idx = view.top + slot;
        if idx >= len {
            break;
        }
        let y = ITEMS_TOP + slot as isize * ITEM_H;
        let selected = idx == view.cursor;
        let (fg, bg) = if selected { (dark, lit) } else { (lit, dark) };
        if selected {
            gfx::fill_rect(fb, Point::new(0, y), Point::new(right, y + ITEM_H - 1), lit);
        }
        // A script's filename can be longer than the 20 cells that fit beside
        // the scrollbar; `msg` would run it off the edge and `put_pixel` would
        // clip it mid-glyph. Truncating with an ellipsis reads as deliberate.
        let label = list.label(idx);
        let room = ((right - LABEL_X) / gfx::CHAR_WIDTH) as usize;
        if label.chars().count() <= room {
            gfx::msg(fb, label, Point::new(LABEL_X, y), fg, bg);
        } else {
            let cut = label.char_indices().nth(room.saturating_sub(1)).map(|(i, _)| i).unwrap_or(0);
            gfx::msg(fb, &label[..cut], Point::new(LABEL_X, y), fg, bg);
            gfx::glyph(fb, '~', Point::new(LABEL_X + (room as isize - 1) * gfx::CHAR_WIDTH, y), fg, bg);
        }
    }

    if scrolls {
        render_scrollbar(fb, view.top, len, lit);
    }
}

fn render_scrollbar(fb: &mut dyn FrameBuffer, top: usize, len: usize, lit: ColorNative) {
    let x0 = COLUMN - SCROLLBAR_W;
    let x1 = COLUMN - 1;
    let track_top = ITEMS_TOP;
    let track_h = ROW - ITEMS_TOP;

    // Track: just the two side rails, so it reads as a groove rather than a bar.
    for y in track_top..ROW {
        fb.put_pixel(Point::new(x0, y), lit);
        fb.put_pixel(Point::new(x1, y), lit);
    }

    let thumb_h = (track_h * VISIBLE as isize / len as isize).max(6);
    let travel = track_h - thumb_h;
    let thumb_y = track_top + travel * top as isize / (len - VISIBLE) as isize;
    gfx::fill_rect(fb, Point::new(x0, thumb_y), Point::new(x1, thumb_y + thumb_h - 1), lit);
}
