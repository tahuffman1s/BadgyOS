//! Runs the sample scripts the badge seeds its drive with.
//!
//! These are the first Python most people will see on this badge, and they are
//! baked into the firmware image -- a typo in one of them is a syntax error on
//! screen the first time the drive is formatted, on a device with no editor.
//! So they get compiled and executed here, on every `cargo test`.
//!
//! The path reaches up out of this crate into the firmware's `samples/`
//! directory. That coupling is the point: it is the same bytes
//! `src/scripts.rs` embeds with `include_bytes!`.

use pycon::host::{Abort, Host, NullHost};
use pycon::{Completion, Script};

const HELLO: &str = include_str!("../../samples/hello.py");
const BOUNCE: &str = include_str!("../../samples/bounce.py");
const KEYS: &str = include_str!("../../samples/keys.py");
const JIGGLE: &str = include_str!("../../samples/jiggle.py");
const USBID: &str = include_str!("../../samples/usbid.py");

/// A host that answers like a badge with a key held down, and gives up after a
/// bounded number of ticks.
///
/// Two of the samples loop until a key is pressed, so a host that reports no
/// keys would run them forever. Reporting a key makes them take their exit
/// path, which is the path worth testing.
struct Bench {
    inner: NullHost,
    ticks: u32,
    limit: u32,
    /// What `keys()` and `wait_key()` report.
    key: u32,
    /// Whether a host is listening for mouse reports.
    mouse: bool,
    /// Reports the script asked to send, and how many of those were accepted.
    reports: u32,
    /// The button mask most recently set.
    buttons: u8,
    /// The USB identity, tracked so identity-API tests can read it back.
    vid: u16,
    pid: u16,
    name: String,
    /// How many times the script asked to re-present the device.
    reattaches: u32,
    /// Frames the script injected, in slot order.
    sprites: Vec<Vec<String>>,
    /// The frames the mascot is being held on, and the line under him.
    held: (i32, i32),
    caption: String,
    /// How many frames the script drew into its own page.
    drawn: u32,
}

/// The shape of a badger frame, for a bench that has no art in it.
///
/// Blank rows, but the right number of them and the right length: a script that
/// paints into what `badgy_art` gives it indexes those rows, so a bench that
/// returned something smaller would fail scripts that are perfectly correct on
/// the badge, and one that returned nothing would not exercise them at all.
const ART_W: usize = 72;
const ART_H: usize = 74;

impl Bench {
    fn new(key: u32) -> Self {
        Bench {
            inner: NullHost::default(),
            ticks: 0,
            limit: 200_000,
            key,
            mouse: false,
            reports: 0,
            buttons: 0,
            vid: pycon::host::USB_VID_DEFAULT,
            pid: pycon::host::USB_PID_DEFAULT,
            name: String::new(),
            reattaches: 0,
            sprites: Vec::new(),
            held: (pycon::host::BADGY_AUTO, pycon::host::BADGY_AUTO),
            caption: String::new(),
            drawn: 0,
        }
    }

    fn slot(&self, frame: i32) -> Option<usize> {
        let i = usize::try_from(frame - pycon::host::SPRITE_SLOT_BASE).ok()?;
        if i < self.sprites.len() { Some(i) } else { None }
    }

    fn with_mouse(mut self) -> Self {
        self.mouse = true;
        self
    }
}

impl Host for Bench {
    fn tick(&mut self) -> Result<(), Abort> {
        self.ticks += 1;
        // A sample that ignores its exit condition would spin here. Aborting
        // makes that a test failure rather than a hung suite.
        if self.ticks > self.limit { Err(Abort) } else { Ok(()) }
    }

    fn print_line(&mut self, s: &str) { self.inner.print_line(s) }

    fn gfx_clear(&mut self) {}

    fn gfx_pixel(&mut self, _x: i32, _y: i32, _on: bool) {}

    fn gfx_text(&mut self, _x: i32, _y: i32, _s: &str, _on: bool) {}

    fn gfx_rect(&mut self, _a: i32, _b: i32, _c: i32, _d: i32, _f: bool) {}

    fn gfx_line(&mut self, _a: i32, _b: i32, _c: i32, _d: i32) {}

    fn gfx_show(&mut self) -> Result<(), Abort> { self.tick() }

    fn keys(&mut self) -> u32 { self.key }

    fn wait_key(&mut self) -> Result<u32, Abort> { Ok(self.key) }

    fn sleep_ms(&mut self, _ms: u32) -> Result<(), Abort> { self.tick() }

    fn random(&mut self) -> u32 { self.inner.random() }

    fn mouse_ready(&mut self) -> bool { self.mouse }

    fn mouse_move(&mut self, _dx: i8, _dy: i8, _wheel: i8) -> Result<bool, Abort> {
        if self.mouse {
            self.reports += 1;
        }
        Ok(self.mouse)
    }

    fn mouse_buttons(&mut self, mask: u8) -> Result<bool, Abort> {
        self.buttons = mask;
        if self.mouse {
            self.reports += 1;
        }
        Ok(self.mouse)
    }

    fn usb_ids(&mut self) -> (u16, u16) { (self.vid, self.pid) }

    fn usb_set_identity(&mut self, vid: u16, pid: u16) -> Result<bool, Abort> {
        // Mirror the firmware's one refusal so a test can exercise it.
        if vid == 0x1d50 && pid == 0x6196 {
            return Ok(false);
        }
        self.vid = vid;
        self.pid = pid;
        self.reattaches += 1;
        Ok(true)
    }

    fn usb_set_name(&mut self, name: &str) -> Result<bool, Abort> {
        self.name = String::from(name);
        self.reattaches += 1;
        Ok(true)
    }

    fn badgy_art(&mut self, frame: i32) -> Option<Vec<String>> {
        if let Some(i) = self.slot(frame) {
            return Some(self.sprites[i].clone());
        }
        if (pycon::host::BADGY_AUTO..=pycon::host::BADGY_MOOD_MAX).contains(&frame) {
            return Some(vec![" ".repeat(ART_W); ART_H]);
        }
        None
    }

    fn badgy_define(&mut self, rows: &[&str]) -> i32 {
        if self.sprites.len() >= pycon::host::SPRITE_SLOTS {
            return pycon::host::SPRITE_NONE;
        }
        self.sprites.push(rows.iter().map(|r| String::from(*r)).collect());
        pycon::host::SPRITE_SLOT_BASE + self.sprites.len() as i32 - 1
    }

    fn badgy_redefine(&mut self, slot: i32, rows: &[&str]) -> i32 {
        match self.slot(slot) {
            Some(i) => {
                self.sprites[i] = rows.iter().map(|r| String::from(*r)).collect();
                slot
            }
            None => pycon::host::SPRITE_NONE,
        }
    }

    fn badgy_draw(&mut self, _x: i32, _y: i32, frame: i32) -> bool {
        if self.badgy_art(frame).is_none() {
            return false;
        }
        self.drawn += 1;
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

fn run(name: &str, src: &str, key: u32) -> Vec<String> {
    let script = Script::compile(src).unwrap_or_else(|e| panic!("{} does not compile: {}", name, e));
    let mut host = Bench::new(key);
    match script.run(&mut host) {
        Ok(Completion::Finished) => host.inner.output,
        Ok(Completion::Aborted) => {
            panic!("{} did not finish within {} ticks -- does it honour its exit key?", name, host.limit)
        }
        Err(e) => panic!("{} failed at runtime: {}", name, e),
    }
}

#[test]
fn hello_py_runs() {
    let out = run("hello.py", HELLO, pycon::host::KEY_SELECT);
    assert_eq!(out, ["hello.py finished"]);
}

#[test]
fn bounce_py_runs_and_exits_on_a_key() {
    // The box moves until a key appears; with one held from the start it should
    // stop on the first pass and report zero bounces.
    let out = run("bounce.py", BOUNCE, pycon::host::KEY_CENTER);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert!(out[0].starts_with("bounced "), "{:?}", out);
}

#[test]
fn bounce_py_actually_bounces_when_left_alone() {
    // No key held, so it runs until the tick budget aborts it. What matters is
    // that it gets there by drawing frames rather than by hanging: an aborted
    // run is the expected outcome, and the abort must come from tick().
    let script = Script::compile(BOUNCE).unwrap();
    let mut host = Bench::new(0);
    host.limit = 5000;
    assert_eq!(script.run(&mut host).unwrap(), Completion::Aborted);
    assert!(host.ticks > 100, "the loop should be yielding regularly, saw {} ticks", host.ticks);
}

#[test]
fn keys_py_compiles_and_yields() {
    // keys.py has no exit condition of its own -- it is meant to be stopped
    // with the LEFT+CENTER chord -- so the only thing to check is that it
    // parses and that it hands control back often enough to be stoppable.
    let script = Script::compile(KEYS).unwrap_or_else(|e| panic!("keys.py does not compile: {}", e));
    let mut host = Bench::new(pycon::host::KEY_UP);
    host.limit = 2000;
    assert_eq!(script.run(&mut host).unwrap(), Completion::Aborted);
    assert!(host.ticks > 100, "keys.py should yield regularly, saw {} ticks", host.ticks);
}

#[test]
fn jiggle_py_moves_the_mouse_when_a_host_is_listening() {
    // Like keys.py it runs until the chord stops it, so the assertions are
    // about what it did on the way: it should nudge immediately rather than
    // sit through a whole interval first, and a nudge is a move out and a move
    // back, so the first one is two reports.
    let script = Script::compile(JIGGLE).unwrap_or_else(|e| panic!("jiggle.py does not compile: {}", e));
    let mut host = Bench::new(0).with_mouse();
    host.limit = 20_000;
    assert_eq!(script.run(&mut host).unwrap(), Completion::Aborted);
    assert!(host.reports >= 2, "expected an immediate out-and-back nudge, saw {}", host.reports);
    assert_eq!(host.buttons, 0, "a jiggler must never leave a button held");
    assert!(host.ticks > 100, "jiggle.py should yield regularly, saw {} ticks", host.ticks);

    // With a host listening it should also have taken the mascot, so that the
    // home screen says what is going on once the script is put in the
    // background -- which is the way a jiggler is actually used.
    let base = pycon::host::SPRITE_SLOT_BASE;
    assert_eq!(host.held, (base, base + 1), "expected a two-frame hold");
    assert_eq!(host.caption, "jiggling");
}

#[test]
fn jiggle_py_builds_two_badger_frames_with_a_mouse_in_them() {
    // The frames are built by painting into what `badgy_art` returns, which is
    // the part of the sprite API most worth guarding: an off-by-one in the
    // paste is not a crash, it is a badger with a mouse growing out of his ear,
    // and nothing else in the suite would notice.
    let script = Script::compile(JIGGLE).unwrap();
    let mut host = Bench::new(0).with_mouse();
    host.limit = 5_000;
    assert_eq!(script.run(&mut host).unwrap(), Completion::Aborted);

    assert_eq!(host.sprites.len(), 2, "expected one frame per half of the jiggle");
    for (n, frame) in host.sprites.iter().enumerate() {
        assert_eq!(frame.len(), ART_H, "frame {} is the wrong height", n);
        for (y, row) in frame.iter().enumerate() {
            assert_eq!(row.len(), ART_W, "frame {} row {} is the wrong width", n, y);
            assert!(
                row.bytes().all(|b| b == b'#' || b == b'.' || b == b' '),
                "frame {} row {} has something other than sprite characters in it",
                n,
                y
            );
        }
        // The bench's badger is blank, so every marked pixel in the result came
        // from the mouse -- and it has to be up beside his head where the plug
        // was, not spread over the frame.
        let ink: Vec<(usize, usize)> = frame
            .iter()
            .enumerate()
            .flat_map(|(y, row)| {
                row.bytes().enumerate().filter(|(_, b)| *b != b' ').map(move |(x, _)| (x, y))
            })
            .collect();
        assert!(!ink.is_empty(), "frame {} came out empty -- nothing was pasted", n);
        // The window is the gap between two bits of badger, and both edges are
        // load-bearing. Above row 21 is his ear, and a mouse that starts there
        // reads as a growth on his head rather than as something he is holding;
        // below row 42 is the paw pad, which is the only thing making it read as
        // held at all, so burying that loses the point of the frame too.
        assert!(
            ink.iter().all(|&(x, y)| (55..ART_W).contains(&x) && (21..43).contains(&y)),
            "frame {} painted outside the gap between Badgy's ear and his paw",
            n
        );
    }
    assert_ne!(host.sprites[0], host.sprites[1], "the two frames must differ or the hold does not animate");
}

#[test]
fn jiggle_py_sends_nothing_with_no_host() {
    // Unplugged is the normal state for a badge sitting on a table, and the
    // script has to keep drawing its screen through it rather than wedging on
    // a report nobody will collect.
    let script = Script::compile(JIGGLE).unwrap();
    let mut host = Bench::new(0);
    host.limit = 5_000;
    assert_eq!(script.run(&mut host).unwrap(), Completion::Aborted);
    assert_eq!(host.reports, 0);
    assert!(host.ticks > 100, "jiggle.py should keep running unplugged, saw {} ticks", host.ticks);

    // And it should still be holding the mascot. The home screen is what you
    // look at with the badge on a table, which is often the moment the cable is
    // out -- a jiggler that dropped the mouse whenever nothing was listening
    // would be invisible exactly when you went to look at it.
    let base = pycon::host::SPRITE_SLOT_BASE;
    assert_eq!(host.held, (base, base), "unplugged, he should hold one frame rather than wave it");
    assert_eq!(host.caption, "no host");
}

#[test]
fn jiggle_py_keeps_the_badger_while_it_is_paused() {
    // Pausing stops the reports, not the script -- so it must not hand the
    // mascot back. A jiggler that dropped the pose whenever it was paused would
    // put the home screen back on the firmware's idle badger while a script it
    // cannot see is still the thing the badge is doing.
    //
    // The wheel is reported held from the first pass, and the script only acts
    // on the edge, so this pauses it once and leaves it paused.
    let script = Script::compile(JIGGLE).unwrap();
    let mut host = Bench::new(pycon::host::KEY_SELECT).with_mouse();
    host.limit = 5_000;
    assert_eq!(script.run(&mut host).unwrap(), Completion::Aborted);
    assert_eq!(host.reports, 0, "a paused jiggler must not move the pointer");

    let base = pycon::host::SPRITE_SLOT_BASE;
    assert_eq!(host.held, (base, base), "paused, he should hold the mouse still rather than wave it");
    assert_eq!(host.caption, "paused");
}

#[test]
fn usbid_py_sets_and_restores_the_identity() {
    // It runs a fixed sequence and returns, so the whole thing is checkable:
    // it should change the identity, be refused the bootloader's id, and leave
    // the badge back on its defaults so a demo does not strand the device
    // under a name the user has to hunt down and undo.
    let out = run("usbid.py", USBID, 0);
    assert_eq!(out.len(), 5, "{:?}", out);
    assert!(out[0].starts_with("before "), "{:?}", out);
    assert!(out[1].starts_with("after "), "{:?}", out);
    assert_eq!(out[2], "applied True");
    assert_eq!(out[3], "bootloader id refused: True");
    assert!(out[4].starts_with("restored "), "{:?}", out);

    // And confirm through the host, not just the script's own printout, that
    // the identity really landed back on the defaults.
    let script = Script::compile(USBID).unwrap();
    let mut host = Bench::new(0);
    script.run(&mut host).unwrap();
    assert_eq!(host.vid, pycon::host::USB_VID_DEFAULT);
    assert_eq!(host.pid, pycon::host::USB_PID_DEFAULT);
    assert_eq!(host.name, "");
}

#[test]
fn the_samples_only_use_documented_builtins() {
    // The readme is the badge's only documentation, and it is on the drive
    // beside the samples. If a sample uses something the readme does not list,
    // the first thing a user copies will not be reproducible from what they
    // were told.
    const README: &str = include_str!("../../samples/readme.txt");
    const ALL: [&str; 5] = [HELLO, BOUNCE, KEYS, JIGGLE, USBID];
    for name in [
        "clear",
        "pixel",
        "line",
        "rect",
        "text",
        "show",
        "keys",
        "wait_key",
        "sleep",
        "rand",
        "print",
        "len",
        "str",
        "int",
        "hex",
        "range",
        "mouse_ready",
        "mouse_move",
        "mouse_buttons",
        "mouse_click",
        "usb_vid",
        "usb_pid",
        "usb_id",
        "usb_name",
        "sprite",
        "badgy_art",
        "badgy",
        "badgy_mood",
        "badgy_say",
    ] {
        let used = ALL.iter().any(|s| s.contains(&format!("{}(", name)));
        if used {
            assert!(README.contains(name), "samples use {}() but readme.txt does not mention it", name);
        }
    }
    for konst in [
        "WIDTH",
        "HEIGHT",
        "KEY_UP",
        "KEY_DOWN",
        "KEY_SELECT",
        "KEY_LEFT",
        "KEY_RIGHT",
        "KEY_CENTER",
        "MOUSE_LEFT",
        "MOUSE_RIGHT",
        "MOUSE_MIDDLE",
        "MOUSE_MAX",
        "USB_VID",
        "USB_PID",
        "BADGY_AUTO",
        "BADGY_IDLE",
        "BADGY_BLINK",
        "BADGY_SLEEP",
        "BADGY_DIG",
        "BADGY_PLUG",
        "BADGY_OOPS",
        "SPRITE_NONE",
        "SPRITE_SLOTS",
        "SPRITE_MAX_W",
        "SPRITE_MAX_H",
    ] {
        let used = ALL.iter().any(|s| s.contains(konst));
        if used {
            assert!(README.contains(konst), "samples use {} but readme.txt does not mention it", konst);
        }
    }
}
