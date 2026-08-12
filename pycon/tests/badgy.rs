//! The badger API: injected sprites, and holding the mascot on one.
//!
//! Everything here is about the seam rather than the art. A script hands over
//! rows of text and gets back an integer; the badge is then expected to hold on
//! to that art after the call, after the loop, and after the script has gone
//! back to sleep -- so the parts worth pinning down in a test are which rows are
//! accepted, what happens when there is nowhere to put them, and whether an id
//! that means "nowhere" can be passed on without stopping the program.

use pycon::host::{
    Abort, BADGY_AUTO, BADGY_IDLE, Host, NullHost, SPRITE_MAX_H, SPRITE_MAX_W, SPRITE_NONE, SPRITE_SLOT_BASE,
    SPRITE_SLOTS,
};
use pycon::{Completion, Script};

/// A badge-shaped badger: it really does keep the art, in a table the same size
/// as the firmware's, so a script that runs out of slots runs out here too.
#[derive(Default)]
struct Badger {
    inner: NullHost,
    slots: Vec<Option<Vec<String>>>,
    held: (i32, i32),
    caption: String,
    drawn: Vec<(i32, i32, i32)>,
}

/// Stand-in for the sheet: one small frame, distinguishable from anything a
/// test writes by hand.
fn sheet_art() -> Vec<String> { vec![String::from("##"), String::from(".."), String::from("  ")] }

impl Badger {
    fn new() -> Self { Badger { slots: vec![None; SPRITE_SLOTS], ..Default::default() } }

    fn index(&self, frame: i32) -> Option<usize> {
        let i = usize::try_from(frame - SPRITE_SLOT_BASE).ok()?;
        if i < self.slots.len() { Some(i) } else { None }
    }
}

impl Host for Badger {
    fn tick(&mut self) -> Result<(), Abort> { self.inner.tick() }

    fn print_line(&mut self, s: &str) { self.inner.print_line(s) }

    fn gfx_clear(&mut self) {}

    fn gfx_pixel(&mut self, _x: i32, _y: i32, _on: bool) {}

    fn gfx_text(&mut self, _x: i32, _y: i32, _s: &str, _on: bool) {}

    fn gfx_rect(&mut self, _a: i32, _b: i32, _c: i32, _d: i32, _f: bool) {}

    fn gfx_line(&mut self, _a: i32, _b: i32, _c: i32, _d: i32) {}

    fn gfx_show(&mut self) -> Result<(), Abort> { self.tick() }

    fn keys(&mut self) -> u32 { 0 }

    fn wait_key(&mut self) -> Result<u32, Abort> { Err(Abort) }

    fn sleep_ms(&mut self, _ms: u32) -> Result<(), Abort> { self.tick() }

    fn random(&mut self) -> u32 { self.inner.random() }

    fn badgy_art(&mut self, frame: i32) -> Option<Vec<String>> {
        if let Some(i) = self.index(frame) {
            return self.slots[i].clone();
        }
        if frame == BADGY_AUTO || frame == BADGY_IDLE { Some(sheet_art()) } else { None }
    }

    fn badgy_define(&mut self, rows: &[&str]) -> i32 {
        match self.slots.iter().position(|s| s.is_none()) {
            Some(i) => {
                self.slots[i] = Some(rows.iter().map(|r| String::from(*r)).collect());
                SPRITE_SLOT_BASE + i as i32
            }
            None => SPRITE_NONE,
        }
    }

    fn badgy_redefine(&mut self, slot: i32, rows: &[&str]) -> i32 {
        match self.index(slot) {
            Some(i) => {
                self.slots[i] = Some(rows.iter().map(|r| String::from(*r)).collect());
                slot
            }
            None => SPRITE_NONE,
        }
    }

    fn badgy_draw(&mut self, x: i32, y: i32, frame: i32) -> bool {
        if self.badgy_art(frame).is_none() {
            return false;
        }
        self.drawn.push((x, y, frame));
        true
    }

    fn badgy_mood(&mut self, a: i32, b: i32) -> bool {
        self.held = (a, b);
        true
    }

    fn badgy_say(&mut self, s: &str) -> bool {
        self.caption = String::from(s);
        true
    }
}

fn run(src: &str) -> (Vec<String>, Badger) {
    let s = Script::compile(src).unwrap_or_else(|e| panic!("compile failed: {}", e));
    let mut h = Badger::new();
    match s.run(&mut h) {
        Ok(Completion::Finished) => {
            let out = core::mem::take(&mut h.inner.output);
            (out, h)
        }
        Ok(Completion::Aborted) => panic!("script aborted unexpectedly"),
        Err(e) => panic!("runtime error: {}", e),
    }
}

/// Run expecting a script error, and return the message.
fn fail(src: &str) -> String {
    let mut h = Badger::new();
    match Script::compile(src) {
        Err(e) => e.to_string(),
        Ok(s) => match s.run(&mut h) {
            Err(e) => e.to_string(),
            Ok(c) => panic!("expected a failure, got {:?}", c),
        },
    }
}

// ------------------------------------------------------------------ the happy path

#[test]
fn a_script_can_inject_art_and_hold_the_badger_on_it() {
    let (out, h) = run("id = sprite(['##', '..'])\nprint(id)\nprint(badgy_mood(id))\nbadgy_say('hi')\n");
    assert_eq!(out, [SPRITE_SLOT_BASE.to_string(), String::from("True")]);
    assert_eq!(h.slots[0].as_deref(), Some(["##".to_string(), "..".to_string()].as_slice()));
    assert_eq!(h.held, (SPRITE_SLOT_BASE, SPRITE_SLOT_BASE));
    assert_eq!(h.caption, "hi");
}

#[test]
fn two_frames_alternate_and_one_frame_does_not() {
    let (_, h) = run("a = sprite([' '])\nb = sprite(['#'])\nbadgy_mood(a, b)\n");
    assert_eq!(h.held, (SPRITE_SLOT_BASE, SPRITE_SLOT_BASE + 1));

    let (_, h) = run("a = sprite([' '])\nbadgy_mood(a)\n");
    assert_eq!(h.held.0, h.held.1, "one frame should hold him still, not cycle against something else");
}

#[test]
fn art_read_back_from_the_badge_can_be_painted_on_and_handed_straight_back() {
    // The round trip jiggle.py depends on: what comes out of badgy_art must be
    // exactly what sprite() takes, with no conversion step in between.
    let (out, h) = run("rows = badgy_art(BADGY_IDLE)\n\
         rows[1] = '#.'\n\
         id = sprite(rows)\n\
         print(badgy_art(id))\n");
    assert_eq!(h.slots[0].as_ref().unwrap()[1], "#.");
    assert_eq!(out, ["['##', '#.', '  ']"]);
}

#[test]
fn releasing_the_badger_is_spelled_badgy_auto() {
    let (_, h) = run("id = sprite(['#'])\nbadgy_mood(id)\nbadgy_mood(BADGY_AUTO)\nbadgy_say('')\n");
    assert_eq!(h.held, (BADGY_AUTO, BADGY_AUTO));
    assert_eq!(h.caption, "");
}

#[test]
fn badgy_draws_the_current_mood_when_no_frame_is_named() {
    let (out, h) = run("print(badgy(10, 20))\n");
    assert_eq!(out, ["True"]);
    assert_eq!(h.drawn, [(10, 20, BADGY_AUTO)]);
}

// ------------------------------------------------------------------ running out

#[test]
fn slots_run_out_and_saying_so_does_not_stop_the_script() {
    // The last one has nowhere to go, and every call that takes a frame has to
    // accept the id it gets back: a badge with no room left is the same kind of
    // event as a mouse with no host, and neither is the script's fault.
    let mut src = String::new();
    for _ in 0..SPRITE_SLOTS + 1 {
        src.push_str("id = sprite(['#'])\n");
    }
    src.push_str("print(id)\nprint(badgy_mood(id))\nprint(badgy(0, 0, id))\n");
    let (out, _) = run(&src);
    assert_eq!(out, [SPRITE_NONE.to_string(), String::from("True"), String::from("False")]);
}

#[test]
fn a_frame_can_be_overwritten_in_place_so_an_animation_needs_one_slot() {
    let (out, h) = run("id = sprite(['#'])\n\
         i = 0\n\
         while i < 20:\n\
         \x20   id = sprite(['.'], id)\n\
         \x20   i = i + 1\n\
         print(id)\n");
    assert_eq!(out, [SPRITE_SLOT_BASE.to_string()], "twenty redraws should still be one slot");
    assert_eq!(h.slots[1], None);
    assert_eq!(h.slots[0].as_ref().unwrap()[0], ".");
}

#[test]
fn a_host_with_no_badger_answers_every_call_without_failing() {
    // The bench, and any future build without a mascot. A script written against
    // the API still runs end to end; it just never puts anything on screen.
    let mut h = NullHost::default();
    let s = Script::compile(
        "id = sprite(['#'])\nprint(id)\nprint(badgy_mood(id))\nprint(badgy(0, 0))\nprint(badgy_art(BADGY_IDLE))\n",
    )
    .unwrap();
    assert_eq!(s.run(&mut h).unwrap(), Completion::Finished);
    assert_eq!(h.output, ["-1", "False", "False", "[]"]);
}

// ------------------------------------------------------------------ bad art

#[test]
fn rows_may_only_hold_the_three_sprite_characters() {
    let msg = fail("sprite(['#.', 'xo'])\n");
    assert!(msg.contains("row 1"), "{}", msg);
    assert!(msg.contains('x'), "the offending character should be named: {}", msg);
}

#[test]
fn art_that_would_not_fit_on_the_badge_is_refused_with_its_size() {
    let wide = format!("sprite(['{}'])\n", "#".repeat(SPRITE_MAX_W + 1));
    let msg = fail(&wide);
    assert!(msg.contains(&SPRITE_MAX_W.to_string()), "{}", msg);

    let tall = format!("sprite([{}])\n", "'#',".repeat(SPRITE_MAX_H + 1));
    let msg = fail(&tall);
    assert!(msg.contains("rows"), "{}", msg);
}

#[test]
fn sprite_wants_a_list_of_strings_and_says_so() {
    assert!(fail("sprite('##')\n").contains("list of rows"));
    assert!(fail("sprite([])\n").contains("at least one row"));
    assert!(fail("sprite([1, 2])\n").contains("row 0"));
}

#[test]
fn an_id_that_is_neither_a_mood_nor_a_slot_is_a_typo_worth_naming() {
    // Deliberately not silently ignored: frame ids are constants or values
    // sprite() returned, never arithmetic, so a number out of range is a
    // mistake and finding it a line at a time is the expensive way.
    let msg = fail("badgy_mood(99)\n");
    assert!(msg.contains("BADGY_"), "{}", msg);
    assert!(fail("badgy(0, 0, 7)\n").contains("BADGY_"));
    // ...but the one out-of-range value that means something is allowed
    // through everywhere, so a failed sprite() can be passed on.
    let (out, _) = run("print(badgy_mood(SPRITE_NONE))\nprint(badgy_art(SPRITE_NONE))\n");
    assert_eq!(out, ["True", "[]"]);
}

#[test]
fn only_a_slot_can_be_overwritten() {
    let msg = fail("sprite(['#'], BADGY_IDLE)\n");
    assert!(msg.contains("sprite()"), "{}", msg);
}
