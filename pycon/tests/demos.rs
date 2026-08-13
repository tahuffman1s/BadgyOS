//! Runs the keyboard demo scripts from `demos/`.
//!
//! These are not seeded onto the badge -- they are the scripts a person copies
//! across to exercise the keyboard by hand. But "exercise the keyboard" is no
//! excuse for a script that does not parse, so each one is compiled and run
//! here against a keyboard that records what it was told to do, on every
//! `cargo test`. The assertions are deliberately loose: enough to prove the
//! feature each demo is meant to show actually reaches the host, not to pin the
//! exact byte stream (`samples.rs` already does that for the primitives).

use pycon::host::{Abort, Host, NullHost};
use pycon::{Completion, Script};

const LEDS: &str = include_str!("../../demos/kbd_leds.py");
const TYPE: &str = include_str!("../../demos/kbd_type.py");
const NKRO: &str = include_str!("../../demos/kbd_nkro.py");
const OS: &str = include_str!("../../demos/kbd_os.py");
const COMBO: &str = include_str!("../../demos/kbd_combo.py");

/// A host that answers like a badge with a keyboard, CENTER held down, and a
/// bounded patience. CENTER held is what drives every demo down its "on a
/// button press, do the thing" path so the thing is actually exercised; the
/// tick limit is what stops the demos' `while True` from running forever.
struct KbdBench {
    inner: NullHost,
    ticks: u32,
    limit: u32,
    ready: bool,
    leds: u32,
    os: i32,
    /// One line per keyboard report the script asked to send.
    events: Vec<String>,
    mods: u8,
    /// How many times the script read the host LEDs / probed the OS.
    led_reads: u32,
    os_probes: u32,
}

impl KbdBench {
    fn new() -> Self {
        KbdBench {
            inner: NullHost::default(),
            ticks: 0,
            limit: 20_000,
            ready: true,
            leds: 0,
            os: pycon::host::OS_UNKNOWN,
            events: Vec::new(),
            mods: 0,
            led_reads: 0,
            os_probes: 0,
        }
    }

    fn downs(&self) -> usize { self.events.iter().filter(|e| e.starts_with("down ")).count() }
}

impl Host for KbdBench {
    fn tick(&mut self) -> Result<(), Abort> {
        self.ticks += 1;
        if self.ticks > self.limit { Err(Abort) } else { Ok(()) }
    }

    fn print_line(&mut self, s: &str) { self.inner.print_line(s); }

    fn gfx_clear(&mut self) {}

    fn gfx_pixel(&mut self, _x: i32, _y: i32, _on: bool) {}

    fn gfx_text(&mut self, _x: i32, _y: i32, _s: &str, _on: bool) {}

    fn gfx_rect(&mut self, _a: i32, _b: i32, _c: i32, _d: i32, _f: bool) {}

    fn gfx_line(&mut self, _a: i32, _b: i32, _c: i32, _d: i32) {}

    fn gfx_show(&mut self) -> Result<(), Abort> { self.tick() }

    // CENTER held: the button every demo waits on before it types.
    fn keys(&mut self) -> u32 { pycon::host::KEY_CENTER as u32 }

    fn wait_key(&mut self) -> Result<u32, Abort> { Ok(pycon::host::KEY_CENTER as u32) }

    // Sleeps tick, so the demos' inner "wait for the button to come up" loops
    // still march toward the limit and the script is stoppable.
    fn sleep_ms(&mut self, _ms: u32) -> Result<(), Abort> { self.tick() }

    fn random(&mut self) -> u32 { self.inner.random() }

    fn kbd_ready(&mut self) -> bool { self.ready }

    fn kbd_key(&mut self, code: u8, down: bool) -> Result<bool, Abort> {
        let dir = if down { "down" } else { "up" };
        self.events.push(format!("{} {:#04x} m{:#04x}", dir, code, self.mods));
        Ok(self.ready)
    }

    fn kbd_modifiers(&mut self, mask: u8) -> Result<bool, Abort> {
        self.mods = mask;
        self.events.push(format!("mod {:#04x}", mask));
        Ok(self.ready)
    }

    fn kbd_release_all(&mut self) -> Result<bool, Abort> {
        self.mods = 0;
        self.events.push(String::from("release_all"));
        Ok(self.ready)
    }

    fn kbd_leds(&mut self) -> u32 {
        self.led_reads += 1;
        self.leds
    }

    fn detect_os(&mut self) -> Result<i32, Abort> {
        self.os_probes += 1;
        Ok(self.os)
    }
}

/// Compile and run a demo to its tick limit, returning the host so a test can
/// inspect what it recorded. Every demo is an endless screen, so aborting at
/// the limit is the expected end, not a failure.
fn run(name: &str, src: &str) -> KbdBench {
    let script = Script::compile(src).unwrap_or_else(|e| panic!("{} does not compile: {}", name, e));
    let mut host = KbdBench::new();
    match script.run(&mut host) {
        Ok(Completion::Aborted) => host,
        Ok(Completion::Finished) => panic!("{} finished unexpectedly -- it should loop until stopped", name),
        Err(e) => panic!("{} failed at runtime: {}", name, e),
    }
}

#[test]
fn kbd_leds_reads_the_host_back() {
    // Its whole job is to poll kbd_leds() and draw them, so it should read the
    // LEDs many times over and never touch a key.
    let host = run("kbd_leds.py", LEDS);
    assert!(host.led_reads > 10, "kbd_leds.py should poll the LEDs, saw {}", host.led_reads);
    assert_eq!(host.downs(), 0, "kbd_leds.py should not press any key");
}

#[test]
fn kbd_type_types_a_mixed_line() {
    // The line has upper, lower, digits and shifted punctuation, so a run of it
    // must press many keys and must raise Shift at least once.
    let host = run("kbd_type.py", TYPE);
    assert!(host.downs() > 20, "kbd_type.py should type a whole line, saw {} presses", host.downs());
    assert!(
        host.events.iter().any(|e| e == "mod 0x02"),
        "kbd_type.py should hold Shift for the capitals and symbols"
    );
}

#[test]
fn kbd_nkro_holds_ten_keys_before_releasing() {
    // The test is ten presses with no release between them, then one
    // release_all. If any key were released early there would be an "up"
    // before the release_all.
    let host = run("kbd_nkro.py", NKRO);
    let first_release = host.events.iter().position(|e| e == "release_all").expect("nkro should release");
    let downs_before = host.events[..first_release].iter().filter(|e| e.starts_with("down ")).count();
    let ups_before = host.events[..first_release].iter().filter(|e| e.starts_with("up ")).count();
    assert_eq!(downs_before, 10, "kbd_nkro.py should hold all ten digits at once");
    assert_eq!(ups_before, 0, "kbd_nkro.py should not release any key until the whole row is down");
    assert!(
        host.inner.output.iter().any(|l| l == "holding 10 keys"),
        "kbd_nkro.py should report holding all ten"
    );
}

#[test]
fn kbd_os_probes_the_host() {
    // detect_os() runs once at the top and again on the held CENTER, so the
    // probe count is at least two, and it should not type.
    let host = run("kbd_os.py", OS);
    assert!(host.os_probes >= 2, "kbd_os.py should probe the OS, saw {}", host.os_probes);
    assert_eq!(host.downs(), 0, "kbd_os.py should not press a key");
}

#[test]
fn kbd_combo_selects_all_then_retypes() {
    // On a non-Mac guess it picks Ctrl. The select-all chord is a key tapped
    // under a modifier, so the recorded stream must contain a modifier report
    // that is neither plain Shift nor nothing -- Ctrl is 0x01.
    let host = run("kbd_combo.py", COMBO);
    assert!(host.os_probes >= 1, "kbd_combo.py should ask the OS which modifier to use");
    assert!(
        host.events.iter().any(|e| e == "mod 0x01"),
        "kbd_combo.py should hold Ctrl for select-all on a non-Mac host"
    );
    assert!(host.downs() > 10, "kbd_combo.py should type both words");
}

#[test]
fn kbd_combo_uses_cmd_on_a_mac() {
    // Same script, a Mac guess: the select-all chord should be Cmd (GUI, 0x08)
    // instead of Ctrl. This is the one branch detect_os() actually changes.
    let script = Script::compile(COMBO).expect("kbd_combo.py compiles");
    let mut host = KbdBench::new();
    host.os = pycon::host::OS_MAC;
    assert_eq!(script.run(&mut host).unwrap(), Completion::Aborted);
    assert!(
        host.events.iter().any(|e| e == "mod 0x08"),
        "on macOS kbd_combo.py should hold Cmd (GUI) for select-all"
    );
    assert!(
        !host.events.iter().any(|e| e == "mod 0x01"),
        "on macOS kbd_combo.py should not fall back to Ctrl"
    );
}
