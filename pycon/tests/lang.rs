//! End-to-end language tests: source in, printed output (or an error) out.
//!
//! These are the tests that actually protect the badge. The unit tests inside
//! each module check a piece in isolation; these check that a script someone
//! might really drop onto the drive does what they expect -- and, for the
//! hostile cases, that it fails cleanly instead of taking the firmware with it.

use pycon::host::{Host, NullHost};
use pycon::{Completion, Script};

/// Run `src` and return everything it printed.
fn out(src: &str) -> Vec<String> {
    let mut h = NullHost::default();
    let s = Script::compile(src).unwrap_or_else(|e| panic!("compile failed: {}", e));
    match s.run(&mut h) {
        Ok(Completion::Finished) => h.output,
        Ok(Completion::Aborted) => panic!("script aborted unexpectedly"),
        Err(e) => panic!("runtime error: {}", e),
    }
}

/// Run `src` expecting a failure, and return the message.
fn fail(src: &str) -> String {
    let mut h = NullHost::default();
    match Script::compile(src) {
        Err(e) => e.to_string(),
        Ok(s) => match s.run(&mut h) {
            Err(e) => e.to_string(),
            Ok(c) => panic!("expected a failure, got {:?} with output {:?}", c, h.output),
        },
    }
}

fn one(src: &str) -> String {
    let o = out(src);
    assert_eq!(o.len(), 1, "expected exactly one line, got {:?}", o);
    o.into_iter().next().unwrap()
}

// ------------------------------------------------------------------ the basics

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(one("print(1 + 2 * 3)\n"), "7");
    assert_eq!(one("print((1 + 2) * 3)\n"), "9");
    assert_eq!(one("print(2 ** 3 ** 2)\n"), "512"); // right associative
    assert_eq!(one("print(-2 ** 2)\n"), "-4");
    assert_eq!(one("print(7 // 2)\n"), "3");
    assert_eq!(one("print(-7 // 2)\n"), "-4");
    assert_eq!(one("print(-1 % 8)\n"), "7");
    assert_eq!(one("print(1 << 4 | 3)\n"), "19");
}

#[test]
fn strings_and_lists() {
    assert_eq!(one("print('a' + 'b')\n"), "ab");
    assert_eq!(one("print('ab' * 3)\n"), "ababab");
    assert_eq!(one("print([1, 2] + [3])\n"), "[1, 2, 3]");
    assert_eq!(one("print([0] * 3)\n"), "[0, 0, 0]");
    assert_eq!(one("print('hello'[1])\n"), "e");
    assert_eq!(one("print('hello'[-1])\n"), "o");
    assert_eq!(one("print(len('hello'))\n"), "5");
}

#[test]
fn control_flow() {
    assert_eq!(out("for i in range(3):\n    if i == 1:\n        continue\n    print(i)\n"), ["0", "2"]);
    assert_eq!(one("n = 0\nwhile n < 5:\n    n += 1\nprint(n)\n"), "5");
    assert_eq!(out("for i in range(10):\n    if i > 2:\n        break\n    print(i)\n"), ["0", "1", "2"]);
}

#[test]
fn elif_else_picks_exactly_one_arm() {
    let src = "\
for n in [1, 2, 3]:
    if n == 1:
        print('one')
    elif n == 2:
        print('two')
    else:
        print('many')
";
    assert_eq!(out(src), ["one", "two", "many"]);
}

#[test]
fn functions_recursion_and_scope() {
    let src = "\
def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(10))
";
    assert_eq!(one(src), "55");
}

#[test]
fn a_function_may_be_called_before_it_is_defined() {
    // `def` at the top level is hoisted, which is what lets a script read
    // top-down with helpers at the bottom.
    assert_eq!(one("print(helper())\ndef helper():\n    return 42\n"), "42");
}

#[test]
fn assignment_in_a_function_is_local_unless_declared_global() {
    let src = "\
x = 1
def sneaky():
    x = 2
def honest():
    global x
    x = 3
sneaky()
print(x)
honest()
print(x)
";
    assert_eq!(out(src), ["1", "3"]);
}

#[test]
fn a_function_can_read_a_global_it_does_not_assign() {
    assert_eq!(one("cfg = 7\ndef f():\n    return cfg + 1\nprint(f())\n"), "8");
}

#[test]
fn lists_are_shared_by_reference() {
    assert_eq!(one("a = [1]\nb = a\nb.append(2)\nprint(a)\n"), "[1, 2]");
}

#[test]
fn list_and_string_methods() {
    assert_eq!(one("a = [3, 1, 2]\na.sort()\nprint(a)\n"), "[1, 2, 3]");
    assert_eq!(one("a = [1, 2, 3]\nprint(a.pop())\n"), "3");
    assert_eq!(one("print('a,b,c'.split(','))\n"), "['a', 'b', 'c']");
    assert_eq!(one("print('-'.join(['a', 'b']))\n"), "a-b");
    assert_eq!(one("print('Hi'.upper())\n"), "HI");
}

#[test]
fn chained_comparison() {
    assert_eq!(one("n = 5\nprint(0 <= n < 10)\n"), "True");
    assert_eq!(one("n = 15\nprint(0 <= n < 10)\n"), "False");
}

#[test]
fn membership_and_boolean_shortcircuit() {
    assert_eq!(one("print(2 in [1, 2, 3])\n"), "True");
    assert_eq!(one("print('z' not in 'abc')\n"), "True");
    // `and` returns the operand, not a bool, and must not evaluate the right
    // side when the left is falsy -- boom() would fail if it were called.
    assert_eq!(one("print(0 and boom())\n"), "0");
    assert_eq!(one("print(1 or boom())\n"), "1");
}

#[test]
fn index_assignment() {
    assert_eq!(one("a = [1, 2, 3]\na[0] = 9\na[-1] += 10\nprint(a)\n"), "[9, 2, 13]");
}

#[test]
fn a_realistic_badge_script_runs() {
    // The shape of a script someone would actually write for the badge.
    let src = "\
def box(x, y, w):
    rect(x, y, x + w, y + w, False)

clear()
for i in range(4):
    box(i * 8, i * 8, 6)
text(0, 100, 'hi ' + str(len('abc')))
show()
print('done')
";
    assert_eq!(one(src), "done");
}

// ------------------------------------------------------------ failure handling

#[test]
fn runtime_errors_carry_a_line_number() {
    let e = fail("a = 1\nb = 2\nc = a / 0\n");
    assert!(e.starts_with("line 3:"), "{}", e);
    assert!(e.contains("division by zero"), "{}", e);
}

#[test]
fn undefined_names_are_reported_by_name() {
    let e = fail("print(nope)\n");
    assert!(e.contains("'nope' is not defined"), "{}", e);
}

#[test]
fn out_of_range_index_is_an_error_not_a_clamp() {
    assert!(fail("a = [1]\nprint(a[5])\n").contains("out of range"));
    assert!(fail("a = [1]\nprint(a[-2])\n").contains("out of range"));
}

#[test]
fn wrong_argument_count_is_caught() {
    assert!(fail("def f(a, b):\n    return a\nf(1)\n").contains("takes 2"));
}

#[test]
fn type_errors_name_both_types() {
    let e = fail("print(1 + 'a')\n");
    assert!(e.contains("int") && e.contains("str"), "{}", e);
}

#[test]
fn unsupported_syntax_fails_at_compile_time() {
    for src in [
        "import os\n",
        "class Foo:\n    pass\n",
        "try:\n    pass\nexcept:\n    pass\n",
        "f = lambda x: x\n",
        "d = {1: 2}\n",
        "x = 1.5\n",
    ] {
        let mut h = NullHost::default();
        let r = Script::compile(src);
        assert!(r.is_err(), "expected {:?} to be rejected", src);
        let _ = &mut h;
    }
}

// --------------------------------------------------------- hostile input safety

#[test]
fn infinite_recursion_is_refused_not_crashed() {
    let e = fail("def f():\n    return f()\nf()\n");
    assert!(e.contains("recursion") || e.contains("deeply"), "{}", e);
}

#[test]
fn an_infinite_loop_yields_to_the_host_and_can_be_stopped() {
    let mut h = NullHost::default();
    let s = Script::compile("while True:\n    pass\n").unwrap();
    // The host says "keep going" the first time and "stop" thereafter, which is
    // exactly what the firmware does when it sees the exit gesture.
    h.stop = false;
    // Wrap the host so the first tick succeeds and the rest abort.
    struct StopAfter {
        inner: NullHost,
        n: u32,
    }
    impl Host for StopAfter {
        fn tick_interval(&self) -> u32 { 16 }

        fn tick(&mut self) -> Result<(), pycon::Abort> {
            self.n += 1;
            if self.n > 3 { Err(pycon::Abort) } else { Ok(()) }
        }

        fn print_line(&mut self, s: &str) { self.inner.print_line(s) }

        fn gfx_clear(&mut self) {}

        fn gfx_pixel(&mut self, _x: i32, _y: i32, _on: bool) {}

        fn gfx_text(&mut self, _x: i32, _y: i32, _s: &str, _on: bool) {}

        fn gfx_rect(&mut self, _a: i32, _b: i32, _c: i32, _d: i32, _f: bool) {}

        fn gfx_line(&mut self, _a: i32, _b: i32, _c: i32, _d: i32) {}

        fn gfx_show(&mut self) -> Result<(), pycon::Abort> { self.tick() }

        fn keys(&mut self) -> u32 { 0 }

        fn wait_key(&mut self) -> Result<u32, pycon::Abort> { Err(pycon::Abort) }

        fn sleep_ms(&mut self, _ms: u32) -> Result<(), pycon::Abort> { self.tick() }

        fn random(&mut self) -> u32 { 0 }
    }
    let mut stopper = StopAfter { inner: h, n: 0 };
    assert_eq!(s.run(&mut stopper).unwrap(), Completion::Aborted);
    assert_eq!(stopper.n, 4, "should have stopped on the fourth tick");
}

#[test]
fn a_huge_range_is_refused_as_a_value_but_fine_as_a_loop() {
    // Materializing it would need megabytes...
    assert!(fail("a = range(1000000)\n").contains("limit"));
    // ...but iterating it needs nothing, so the loop form is allowed. Keep the
    // body cheap: this really does run a million iterations.
    assert_eq!(one("n = 0\nfor i in range(1000000):\n    n += 1\nprint(n)\n"), "1000000");
}

#[test]
fn a_giant_list_literal_is_refused() {
    let mut src = String::from("a = [");
    for i in 0..5000 {
        if i > 0 {
            src.push(',');
        }
        src.push('1');
    }
    src.push_str("]\n");
    assert!(fail(&src).contains("too long"));
}

#[test]
fn deeply_nested_data_does_not_blow_the_stack_when_printed() {
    // Build nesting at runtime, past the formatter's depth cap.
    let src = "\
a = [1]
for i in range(64):
    a = [a]
print(a)
";
    let printed = one(src);
    assert!(printed.contains("..."), "expected the formatter to give up: {}", printed);
}

#[test]
fn a_self_referential_list_can_be_printed() {
    assert!(one("a = []\na.append(a)\nprint(a)\n").contains("["));
}

#[test]
fn integer_overflow_wraps_rather_than_panicking() {
    // Release builds wrap anyway; this makes sure debug builds agree, so a
    // script cannot panic the firmware by counting too high.
    assert_eq!(one("print(2147483647 + 1)\n"), "-2147483648");
    assert_eq!(one("print(2147483647 * 2)\n"), "-2");
}

#[test]
fn shifting_past_the_word_size_is_defined() {
    assert_eq!(one("print(1 << 40)\n"), "0");
    assert_eq!(one("print(-1 >> 40)\n"), "-1");
    assert!(fail("print(1 << -1)\n").contains("negative shift"));
}

#[test]
fn mutating_a_list_while_iterating_it_does_not_panic() {
    // Whatever the semantics, it must not be a RefCell double-borrow.
    let o = out("a = [1, 2, 3]\nfor x in a:\n    a.pop()\n    print(x)\n");
    assert!(!o.is_empty());
}

#[test]
fn empty_and_whitespace_only_programs_are_valid() {
    assert!(out("").is_empty());
    assert!(out("\n\n   \n# just a comment\n").is_empty());
}

#[test]
fn crlf_line_endings_work() {
    // A script written on Windows and dropped onto the drive.
    assert_eq!(out("print(1)\r\nprint(2)\r\n"), ["1", "2"]);
}

#[test]
fn tabs_indent_the_same_as_spaces() {
    assert_eq!(one("if True:\n\tprint('yes')\n"), "yes");
}
