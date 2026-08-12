//! Regression tests for the ways an untrusted script can try to take the badge
//! down without being obviously malicious.
//!
//! Every case here was found by fuzzing or by reading for it, and every one of
//! them used to fail: a panic, an allocation the heap cannot serve, or a burst
//! of work long enough that the user cannot interrupt it. On this device those
//! are the same bug -- the panic handler spins forever, a failed allocation
//! panics, and an uninterruptible loop needs a power cycle.
//!
//! Two properties are being defended:
//!
//! * **Nothing panics.** These run under `cargo test`, which has overflow checks on, so an arithmetic
//!   overflow that the release firmware would quietly wrap still fails here. That is deliberate: wrapping
//!   silently is not a defence, it is a coincidence.
//! * **Nothing runs unbounded.** Either the operation is refused with a message the user can read on the
//!   panel, or it completes.

use std::time::{Duration, Instant};

use pycon::host::{Abort, Host, NullHost};
use pycon::{Completion, Script};

enum Outcome {
    Done(Vec<String>),
    Refused(String),
}

impl Outcome {
    fn refused_with(self, needle: &str) -> String {
        match self {
            Outcome::Refused(m) => {
                assert!(m.contains(needle), "expected a message mentioning {:?}, got {:?}", needle, m);
                m
            }
            Outcome::Done(out) => panic!("expected a refusal mentioning {:?}, but it ran: {:?}", needle, out),
        }
    }

    fn done(self) -> Vec<String> {
        match self {
            Outcome::Done(o) => o,
            Outcome::Refused(m) => panic!("expected it to run, but it was refused: {}", m),
        }
    }
}

/// Run `src`, insisting that it terminates. Anything that takes more than a few
/// seconds on a laptop would be minutes on a 350 MHz badge, which counts as a
/// hang for our purposes.
fn run(src: &str) -> Outcome {
    let started = Instant::now();
    let result = match Script::compile(src) {
        Err(e) => Outcome::Refused(e.to_string()),
        Ok(s) => {
            let mut h = NullHost::default();
            match s.run(&mut h) {
                Ok(Completion::Finished) => Outcome::Done(h.output),
                Ok(Completion::Aborted) => Outcome::Refused(String::from("aborted")),
                Err(e) => Outcome::Refused(e.to_string()),
            }
        }
    };
    let took = started.elapsed();
    assert!(took < Duration::from_secs(20), "took {:?}, which is a hang on the badge", took);
    result
}

// ------------------------------------------------------------------ arithmetic

#[test]
fn dividing_the_most_negative_int_by_minus_one_does_not_panic() {
    // The quotient does not fit in an i32, so a plain `%` inside floor_div
    // overflows. Python has arbitrary precision and just answers 2147483648;
    // we wrap, which is the documented behaviour of every other operator here.
    for src in [
        "x = -2147483647 - 1\nprint(x // -1)\n",
        "x = -2147483647 - 1\nprint(x % -1)\n",
        "x = -2147483647 - 1\nprint(x / -1)\n",
        "x = -2147483647 - 1\nx //= -1\nprint(x)\n",
        "x = -2147483647 - 1\nx %= -1\nprint(x)\n",
    ] {
        let out = run(src).done();
        assert_eq!(out.len(), 1, "{:?} -> {:?}", src, out);
    }
}

#[test]
fn the_modulo_identity_holds_at_the_extremes() {
    assert_eq!(run("x = -2147483647 - 1\nprint(x % 3)\n").done(), ["1"]);
    assert_eq!(run("print(2147483647 % -3)\n").done(), ["-2"]);
}

#[test]
fn a_huge_exponent_returns_rather_than_grinding() {
    // Exponentiation is by squaring, so this is ~31 multiplies, not 2^31.
    let out = run("print(2 ** 2147483647)\n").done();
    assert_eq!(out.len(), 1);
    assert_eq!(run("print(1 ** 2147483647)\n").done(), ["1"]);
}

// ------------------------------------------------------------------ allocation

#[test]
fn a_list_cannot_grow_without_limit() {
    // append() used to be the one growth path with no cap on it.
    run("a = []\nwhile True:\n    a.append(0)\n").refused_with("limit");
    run("a = []\nwhile True:\n    a.insert(0, 1)\n").refused_with("limit");
    run("a = [0]\nwhile True:\n    a.extend(a)\n").refused_with("limit");
}

#[test]
fn a_string_cannot_grow_without_limit() {
    // Doubling: twenty iterations is a megabyte, against a 256 KiB heap.
    run("s = 'x'\nfor i in range(30):\n    s = s + s\n").refused_with("too long");
    run("s = 'x' * 1000000\n").refused_with("too long");
    run("s = 'a'\nfor i in range(30):\n    s = s.replace('a', 'aa')\n").refused_with("too long");
    run("a = ['x' * 16000] * 100\nprint(len(''.join(a)))\n").refused_with("too long");
}

#[test]
fn strings_that_do_fit_still_work() {
    assert_eq!(run("print(len('ab' * 100))\n").done(), ["200"]);
    assert_eq!(run("print(len('-'.join(['ab', 'cd'])))\n").done(), ["5"]);
    assert_eq!(run("print('aXa'.replace('X', 'YY'))\n").done(), ["aYYa"]);
}

// -------------------------------------------------------------- shared structure

#[test]
fn printing_a_self_referential_list_with_fan_out_is_bounded() {
    // Depth alone does not bound this: two self-references per level means
    // 2^depth nodes, and three means 43 million. The visit budget is what
    // actually stops it.
    let out = run("a = []\na.append(a)\na.append(a)\na.append(a)\nprint(len(str(a)))\n").done();
    assert_eq!(out.len(), 1);
    let n: usize = out[0].parse().expect("length should be a number");
    assert!(n < 8192, "repr grew to {} characters", n);
}

#[test]
fn comparing_two_self_referential_lists_is_bounded() {
    let out =
        run("a = []\nb = []\nfor i in range(4):\n    a.append(a)\n    b.append(b)\nprint(a == b)\n").done();
    assert_eq!(out.len(), 1);
}

#[test]
fn sorting_a_list_of_tangled_lists_terminates() {
    run("a = []\na.append(a)\nb = [a, a, a]\nb.sort()\nprint(1)\n").done();
}

#[test]
fn printing_a_wide_nested_list_is_truncated_not_unbounded() {
    let out = run("a = [1] * 4000\nb = [a] * 4000\nprint(len(str(b)))\n").done();
    let n: usize = out[0].parse().unwrap();
    assert!(n < 8192, "repr grew to {} characters", n);
}

// --------------------------------------------------------------------- parsing

#[test]
fn a_long_run_of_unary_operators_is_refused_not_stack_overflowed() {
    let src = format!("x = {}1\n", "~".repeat(10_000));
    run(&src).refused_with("deep");
}

#[test]
fn deeply_nested_brackets_are_refused() {
    let src = format!("x = {}1{}\n", "[".repeat(5000), "]".repeat(5000));
    run(&src).refused_with("deep");
}

#[test]
fn a_deeply_nested_list_value_can_be_built_and_dropped() {
    // The arena means dropping this is a few Vec frees, not a recursion as deep
    // as the nesting. Building it at runtime dodges the parser's depth cap, so
    // this really does exercise the teardown path.
    run("a = []\nn = 0\nwhile n < 3000:\n    a = [a]\n    n = n + 1\nprint(n)\n").done();
}

#[test]
fn garbage_input_never_hangs_the_lexer() {
    // Deterministic byte soup over the characters most likely to confuse the
    // indentation and string state machines. Every input must return a verdict.
    let alphabet: &[u8] =
        b"abc if:\n\t()[]#'\"\\+-*/%<>=!&|^~,.0123456789_ \relse def return while for in not and or global";
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 33) as usize
    };
    for _ in 0..20_000 {
        let n = next() % 60 + 1;
        let src: String = (0..n).map(|_| alphabet[next() % alphabet.len()] as char).collect();
        // Only the verdict matters; either outcome is fine, hanging is not.
        let _ = Script::compile(&src);
    }
}

// ------------------------------------------------------------------- liveness

/// A host that refuses to let the script run for more than `limit` ticks, and
/// records how many it got.
struct Watchdog {
    ticks: u32,
    limit: u32,
}

impl Host for Watchdog {
    fn tick_interval(&self) -> u32 { 64 }

    fn tick(&mut self) -> Result<(), Abort> {
        self.ticks += 1;
        if self.ticks > self.limit { Err(Abort) } else { Ok(()) }
    }

    fn print_line(&mut self, _s: &str) {}

    fn gfx_clear(&mut self) {}

    fn gfx_pixel(&mut self, _x: i32, _y: i32, _on: bool) {}

    fn gfx_text(&mut self, _x: i32, _y: i32, _s: &str, _on: bool) {}

    fn gfx_rect(&mut self, _a: i32, _b: i32, _c: i32, _d: i32, _f: bool) {}

    fn gfx_line(&mut self, _a: i32, _b: i32, _c: i32, _d: i32) {}

    fn gfx_show(&mut self) -> Result<(), Abort> { self.tick() }

    fn keys(&mut self) -> u32 { 0 }

    fn wait_key(&mut self) -> Result<u32, Abort> { self.tick().map(|_| 0) }

    fn sleep_ms(&mut self, _ms: u32) -> Result<(), Abort> { self.tick() }

    fn random(&mut self) -> u32 { 4 }
}

#[test]
fn every_kind_of_endless_loop_can_be_stopped() {
    // The exit chord only works if the loop reaches tick(). Each of these is a
    // different loop in the evaluator.
    for src in [
        "while True:\n    pass\n",
        "while True:\n    x = 1\n",
        "for i in range(2000000000):\n    pass\n",
        "a = [1, 2, 3]\nwhile True:\n    for x in a:\n        pass\n",
        "s = 'abcdefgh'\nwhile True:\n    for c in s:\n        pass\n",
        "def f():\n    while True:\n        pass\nf()\n",
        "while True:\n    show()\n",
        "while True:\n    sleep(1)\n",
        "while True:\n    wait_key()\n",
    ] {
        let script = Script::compile(src).unwrap_or_else(|e| panic!("{:?}: {}", src, e));
        let mut host = Watchdog { ticks: 0, limit: 500 };
        let started = Instant::now();
        assert_eq!(
            script.run(&mut host).unwrap(),
            Completion::Aborted,
            "{:?} did not stop when the host said to",
            src
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?} took {:?} to notice",
            src,
            started.elapsed()
        );
    }
}

#[test]
fn a_file_of_single_character_tokens_does_not_over_grow_the_token_vector() {
    // The worst case for the lexer's reservation: one token per byte, plus a
    // dedent run at the end. The vector must have been sized for it up front --
    // if the reservation under-shoots, this is where a doubling realloc would
    // briefly need 1.5x the memory, on the heap where that vector is already
    // the largest single thing.
    let src = format!("{}\n", "~".repeat(16 * 1024));
    // It is refused (the unary chain is far past MAX_PARSE_DEPTH), but it must
    // be refused by the parser, not by the allocator.
    run(&src).refused_with("deep");
}

#[test]
fn an_empty_program_still_lexes() {
    // The reservation arithmetic has a `+ 8` in it; make sure the degenerate
    // input at the other end is fine too.
    assert!(run("").done().is_empty());
}
