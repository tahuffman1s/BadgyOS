//! The standard library, such as it is: free functions, value methods and the
//! handful of constants a script needs to talk to the badge.
//!
//! Names here are resolved *after* globals and locals, so a script that wants
//! to use `text` as a variable can, at the cost of losing the builtin for the
//! rest of the run. That ordering matches Python and avoids a class of
//! "why doesn't my variable work" confusion.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::host::{self, Host};
use crate::value::{MAX_STR_LEN, Value, resolve_index};

/// What a builtin can go wrong with.
pub enum Fault {
    /// A script-level error: bad argument count, wrong type, out of range.
    Msg(String),
    /// The firmware pulled the plug. Not the script's fault; propagates up
    /// without a line number.
    Abort,
}

impl From<host::Abort> for Fault {
    fn from(_: host::Abort) -> Self { Fault::Abort }
}

fn bad(msg: impl Into<String>) -> Fault { Fault::Msg(msg.into()) }

type R = Result<Value, Fault>;

/// `range()` and list repetition refuse to build anything bigger than this.
///
/// The badge has a few hundred KiB of heap and each element is a `Value`, so a
/// careless `range(10_000_000)` would exhaust it and take down the firmware
/// rather than the script. Failing loudly at a fixed limit is kinder.
pub const MAX_LIST_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    // general purpose
    Print,
    Len,
    Range,
    Str,
    Int,
    Bool,
    Chr,
    Ord,
    Hex,
    Abs,
    Min,
    Max,
    Sum,
    // badge: drawing
    Clear,
    Pixel,
    Text,
    Rect,
    Line,
    Show,
    // badge: input and timing
    Keys,
    WaitKey,
    Sleep,
    Rand,
    // badge: the USB mouse
    MouseReady,
    MouseMove,
    MouseButtons,
    MouseClick,
    // badge: the USB identity
    UsbVid,
    UsbPid,
    UsbId,
    UsbName,
}

impl Builtin {
    pub fn from_name(s: &str) -> Option<Builtin> {
        Some(match s {
            "print" => Builtin::Print,
            "len" => Builtin::Len,
            "range" => Builtin::Range,
            "str" => Builtin::Str,
            "int" => Builtin::Int,
            "bool" => Builtin::Bool,
            "chr" => Builtin::Chr,
            "ord" => Builtin::Ord,
            "hex" => Builtin::Hex,
            "abs" => Builtin::Abs,
            "min" => Builtin::Min,
            "max" => Builtin::Max,
            "sum" => Builtin::Sum,
            "clear" => Builtin::Clear,
            "pixel" => Builtin::Pixel,
            "text" => Builtin::Text,
            "rect" => Builtin::Rect,
            "line" => Builtin::Line,
            "show" => Builtin::Show,
            "keys" => Builtin::Keys,
            "wait_key" => Builtin::WaitKey,
            "sleep" => Builtin::Sleep,
            "rand" => Builtin::Rand,
            "mouse_ready" => Builtin::MouseReady,
            "mouse_move" => Builtin::MouseMove,
            "mouse_buttons" => Builtin::MouseButtons,
            "mouse_click" => Builtin::MouseClick,
            "usb_vid" => Builtin::UsbVid,
            "usb_pid" => Builtin::UsbPid,
            "usb_id" => Builtin::UsbId,
            "usb_name" => Builtin::UsbName,
            _ => return None,
        })
    }
}

/// Names that are constants rather than functions.
pub fn constant(name: &str) -> Option<Value> {
    Some(match name {
        "WIDTH" => Value::Int(host::SCREEN_W),
        "HEIGHT" => Value::Int(host::SCREEN_H),
        "KEY_UP" => Value::Int(host::KEY_UP as i32),
        "KEY_DOWN" => Value::Int(host::KEY_DOWN as i32),
        "KEY_SELECT" => Value::Int(host::KEY_SELECT as i32),
        "KEY_LEFT" => Value::Int(host::KEY_LEFT as i32),
        "KEY_RIGHT" => Value::Int(host::KEY_RIGHT as i32),
        "KEY_CENTER" => Value::Int(host::KEY_CENTER as i32),
        "MOUSE_LEFT" => Value::Int(host::MOUSE_LEFT as i32),
        "MOUSE_RIGHT" => Value::Int(host::MOUSE_RIGHT as i32),
        "MOUSE_MIDDLE" => Value::Int(host::MOUSE_MIDDLE as i32),
        "MOUSE_MAX" => Value::Int(host::MOUSE_MAX_STEP),
        "USB_VID" => Value::Int(host::USB_VID_DEFAULT as i32),
        "USB_PID" => Value::Int(host::USB_PID_DEFAULT as i32),
        _ => return None,
    })
}

/// How long a [`Builtin::MouseClick`] holds the button down.
///
/// A press and a release in the same instant is not a click anyone sees: the
/// host samples the interrupt endpoint on its own schedule -- every 8 to 10 ms
/// here -- and two reports inside one interval arrive together, which reads as
/// no button change at all. Three intervals is comfortably past that without
/// being slow enough to feel like a hold.
const CLICK_MS: u32 = 30;

/// Squeeze a script's `int` into the range one HID report can carry.
fn clamp_step(v: i32) -> i8 { v.clamp(-host::MOUSE_MAX_STEP, host::MOUSE_MAX_STEP) as i8 }

/// Validate a USB vendor or product id. These are 16-bit fields on the wire, so
/// a value that does not fit is a mistake worth naming rather than truncating
/// into a different device.
fn usb_id_arg(name: &str, args: &[Value], i: usize) -> Result<u16, Fault> {
    let v = int_arg(name, args, i)?;
    if !(0..=0xffff).contains(&v) {
        return Err(bad(alloc::format!("{}() id must be between 0 and 65535 (0xffff), got {}", name, v)));
    }
    Ok(v as u16)
}

/// Validate a button mask. Out-of-range bits are a typo, not a nuance -- there
/// are exactly three buttons -- so this fails rather than masking them off.
fn button_mask(name: &str, v: i32) -> Result<u8, Fault> {
    let all = (host::MOUSE_LEFT | host::MOUSE_RIGHT | host::MOUSE_MIDDLE) as i32;
    if v < 0 || v & !all != 0 {
        return Err(bad(alloc::format!(
            "{}() takes a mask of MOUSE_LEFT, MOUSE_RIGHT and MOUSE_MIDDLE, got {}",
            name,
            v
        )));
    }
    Ok(v as u8)
}

// ------------------------------------------------------------------ argument helpers

fn arity(name: &str, args: &[Value], min: usize, max: usize) -> Result<(), Fault> {
    if args.len() < min || args.len() > max {
        let expected =
            if min == max { alloc::format!("{}", min) } else { alloc::format!("{} to {}", min, max) };
        return Err(bad(alloc::format!("{}() takes {} argument(s), got {}", name, expected, args.len())));
    }
    Ok(())
}

fn int_arg(name: &str, args: &[Value], i: usize) -> Result<i32, Fault> {
    match &args[i] {
        Value::Int(v) => Ok(*v),
        Value::Bool(b) => Ok(*b as i32),
        other => Err(bad(alloc::format!(
            "{}() argument {} must be an int, got {}",
            name,
            i + 1,
            other.type_name()
        ))),
    }
}

fn str_arg<'a>(name: &str, args: &'a [Value], i: usize) -> Result<&'a str, Fault> {
    match &args[i] {
        Value::Str(s) => Ok(s),
        other => {
            Err(bad(alloc::format!("{}() argument {} must be a str, got {}", name, i + 1, other.type_name())))
        }
    }
}

/// Optional trailing bool, defaulting to `default`.
fn flag(args: &[Value], i: usize, default: bool) -> bool {
    args.get(i).map(|v| v.truthy()).unwrap_or(default)
}

// ------------------------------------------------------------------ dispatch

pub fn call(b: Builtin, args: &[Value], host: &mut dyn Host) -> R {
    match b {
        Builtin::Print => {
            // Space-separated like Python, and each element inside a container
            // is repr'd while a bare string is not.
            let mut line = String::new();
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    line.push(' ');
                }
                line.push_str(&a.to_display());
            }
            host.print_line(&line);
            Ok(Value::None)
        }
        Builtin::Len => {
            arity("len", args, 1, 1)?;
            args[0]
                .len()
                .map(|n| Value::Int(n as i32))
                .ok_or_else(|| bad(alloc::format!("len() does not apply to {}", args[0].type_name())))
        }
        Builtin::Range => {
            arity("range", args, 1, 3)?;
            let (start, stop, step) = match args.len() {
                1 => (0, int_arg("range", args, 0)?, 1),
                2 => (int_arg("range", args, 0)?, int_arg("range", args, 1)?, 1),
                _ => (int_arg("range", args, 0)?, int_arg("range", args, 1)?, int_arg("range", args, 2)?),
            };
            let items = range_values(start, stop, step)?;
            Ok(Value::list(items))
        }
        Builtin::Str => {
            arity("str", args, 1, 1)?;
            Ok(Value::str(args[0].to_display()))
        }
        Builtin::Bool => {
            arity("bool", args, 1, 1)?;
            Ok(Value::Bool(args[0].truthy()))
        }
        Builtin::Int => {
            arity("int", args, 1, 2)?;
            let radix = if args.len() == 2 { int_arg("int", args, 1)? } else { 10 };
            if !(2..=36).contains(&radix) {
                return Err(bad("int() base must be between 2 and 36"));
            }
            match &args[0] {
                Value::Int(v) => Ok(Value::Int(*v)),
                Value::Bool(b) => Ok(Value::Int(*b as i32)),
                Value::Str(s) => {
                    let t = s.trim();
                    let (neg, digits) = match t.strip_prefix('-') {
                        Some(rest) => (true, rest),
                        None => (false, t.strip_prefix('+').unwrap_or(t)),
                    };
                    // Accept the 0x/0b/0o prefixes when they agree with the base
                    // that was asked for, the way Python does.
                    let digits = match radix {
                        16 => digits.strip_prefix("0x").or(digits.strip_prefix("0X")).unwrap_or(digits),
                        2 => digits.strip_prefix("0b").or(digits.strip_prefix("0B")).unwrap_or(digits),
                        8 => digits.strip_prefix("0o").or(digits.strip_prefix("0O")).unwrap_or(digits),
                        _ => digits,
                    };
                    let n = i32::from_str_radix(digits, radix as u32)
                        .map_err(|_| bad(alloc::format!("int() could not parse '{}'", s)))?;
                    Ok(Value::Int(if neg { n.wrapping_neg() } else { n }))
                }
                other => Err(bad(alloc::format!("int() does not apply to {}", other.type_name()))),
            }
        }
        Builtin::Chr => {
            arity("chr", args, 1, 1)?;
            let n = int_arg("chr", args, 0)?;
            let c = u32::try_from(n)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| bad("chr() argument is not a character"))?;
            Ok(Value::str(c.to_string()))
        }
        Builtin::Ord => {
            arity("ord", args, 1, 1)?;
            let s = str_arg("ord", args, 0)?;
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Ok(Value::Int(c as i32)),
                _ => Err(bad("ord() expects a string of length 1")),
            }
        }
        Builtin::Hex => {
            arity("hex", args, 1, 1)?;
            let n = int_arg("hex", args, 0)?;
            Ok(Value::str(if n < 0 {
                alloc::format!("-0x{:x}", (n as i64).unsigned_abs())
            } else {
                alloc::format!("0x{:x}", n)
            }))
        }
        Builtin::Abs => {
            arity("abs", args, 1, 1)?;
            Ok(Value::Int(int_arg("abs", args, 0)?.wrapping_abs()))
        }
        Builtin::Min => fold_extreme("min", args, core::cmp::Ordering::Less),
        Builtin::Max => fold_extreme("max", args, core::cmp::Ordering::Greater),
        Builtin::Sum => {
            arity("sum", args, 1, 1)?;
            let Value::List(l) = &args[0] else {
                return Err(bad("sum() expects a list"));
            };
            let mut total: i32 = 0;
            for v in l.borrow().iter() {
                match v {
                    Value::Int(i) => total = total.wrapping_add(*i),
                    Value::Bool(b) => total = total.wrapping_add(*b as i32),
                    other => {
                        return Err(bad(alloc::format!("sum() cannot add {}", other.type_name())));
                    }
                }
            }
            Ok(Value::Int(total))
        }

        // -------------------------------------------------------------- drawing
        Builtin::Clear => {
            arity("clear", args, 0, 0)?;
            host.gfx_clear();
            Ok(Value::None)
        }
        Builtin::Pixel => {
            arity("pixel", args, 2, 3)?;
            host.gfx_pixel(int_arg("pixel", args, 0)?, int_arg("pixel", args, 1)?, flag(args, 2, true));
            Ok(Value::None)
        }
        Builtin::Text => {
            arity("text", args, 3, 4)?;
            let x = int_arg("text", args, 0)?;
            let y = int_arg("text", args, 1)?;
            // Accept any value and stringify it: `text(0, 0, score)` is what
            // people write, and demanding str() there is pure friction.
            let s = args[2].to_display();
            host.gfx_text(x, y, &s, flag(args, 3, true));
            Ok(Value::None)
        }
        Builtin::Rect => {
            arity("rect", args, 4, 5)?;
            host.gfx_rect(
                int_arg("rect", args, 0)?,
                int_arg("rect", args, 1)?,
                int_arg("rect", args, 2)?,
                int_arg("rect", args, 3)?,
                flag(args, 4, false),
            );
            Ok(Value::None)
        }
        Builtin::Line => {
            arity("line", args, 4, 4)?;
            host.gfx_line(
                int_arg("line", args, 0)?,
                int_arg("line", args, 1)?,
                int_arg("line", args, 2)?,
                int_arg("line", args, 3)?,
            );
            Ok(Value::None)
        }
        Builtin::Show => {
            arity("show", args, 0, 0)?;
            host.gfx_show()?;
            Ok(Value::None)
        }

        // ---------------------------------------------------------------- input
        Builtin::Keys => {
            arity("keys", args, 0, 0)?;
            Ok(Value::Int(host.keys() as i32))
        }
        Builtin::WaitKey => {
            arity("wait_key", args, 0, 0)?;
            Ok(Value::Int(host.wait_key()? as i32))
        }
        Builtin::Sleep => {
            arity("sleep", args, 1, 1)?;
            let ms = int_arg("sleep", args, 0)?;
            if ms < 0 {
                return Err(bad("sleep() cannot take a negative time"));
            }
            host.sleep_ms(ms as u32)?;
            Ok(Value::None)
        }
        Builtin::Rand => {
            arity("rand", args, 0, 2)?;
            let r = host.random();
            match args.len() {
                0 => Ok(Value::Int((r >> 1) as i32)),
                1 => {
                    let n = int_arg("rand", args, 0)?;
                    if n <= 0 {
                        return Err(bad("rand(n) needs n > 0"));
                    }
                    Ok(Value::Int((r % n as u32) as i32))
                }
                _ => {
                    let lo = int_arg("rand", args, 0)?;
                    let hi = int_arg("rand", args, 1)?;
                    if hi < lo {
                        return Err(bad("rand(lo, hi) needs lo <= hi"));
                    }
                    // Inclusive of both ends, like random.randint.
                    let span = (hi as i64 - lo as i64 + 1) as u64;
                    Ok(Value::Int((lo as i64 + (r as u64 % span) as i64) as i32))
                }
            }
        }

        // ---------------------------------------------------------------- mouse
        Builtin::MouseReady => {
            arity("mouse_ready", args, 0, 0)?;
            Ok(Value::Bool(host.mouse_ready()))
        }
        Builtin::MouseMove => {
            arity("mouse_move", args, 2, 3)?;
            let dx = clamp_step(int_arg("mouse_move", args, 0)?);
            let dy = clamp_step(int_arg("mouse_move", args, 1)?);
            let wheel = if args.len() == 3 { clamp_step(int_arg("mouse_move", args, 2)?) } else { 0 };
            Ok(Value::Bool(host.mouse_move(dx, dy, wheel)?))
        }
        Builtin::MouseButtons => {
            arity("mouse_buttons", args, 1, 1)?;
            let mask = button_mask("mouse_buttons", int_arg("mouse_buttons", args, 0)?)?;
            Ok(Value::Bool(host.mouse_buttons(mask)?))
        }
        Builtin::MouseClick => {
            arity("mouse_click", args, 0, 1)?;
            let raw =
                if args.is_empty() { host::MOUSE_LEFT as i32 } else { int_arg("mouse_click", args, 0)? };
            let mask = button_mask("mouse_click", raw)?;
            let down = host.mouse_buttons(mask)?;
            host.sleep_ms(CLICK_MS)?;
            // Released unconditionally. If the press did not go out, the
            // release is harmless; if it did and the release did not, the host
            // is left holding a button down, which is far worse than a click
            // that never happened.
            let up = host.mouse_buttons(0)?;
            Ok(Value::Bool(down && up))
        }

        // --------------------------------------------------------- usb identity
        Builtin::UsbVid => {
            arity("usb_vid", args, 0, 0)?;
            Ok(Value::Int(host.usb_ids().0 as i32))
        }
        Builtin::UsbPid => {
            arity("usb_pid", args, 0, 0)?;
            Ok(Value::Int(host.usb_ids().1 as i32))
        }
        Builtin::UsbId => {
            arity("usb_id", args, 1, 2)?;
            // One argument sets the product id and keeps the current vendor;
            // two set both. Reading the current vendor first is what lets
            // `usb_id(pid)` mean "same maker, different product".
            let (vid, pid) = if args.len() == 1 {
                (host.usb_ids().0, usb_id_arg("usb_id", args, 0)?)
            } else {
                (usb_id_arg("usb_id", args, 0)?, usb_id_arg("usb_id", args, 1)?)
            };
            Ok(Value::Bool(host.usb_set_identity(vid, pid)?))
        }
        Builtin::UsbName => {
            arity("usb_name", args, 1, 1)?;
            let name = str_arg("usb_name", args, 0)?;
            Ok(Value::Bool(host.usb_set_name(name)?))
        }
    }
}

/// The element list `range(start, stop, step)` denotes.
pub fn range_values(start: i32, stop: i32, step: i32) -> Result<Vec<Value>, Fault> {
    let n = range_len(start, stop, step)?;
    let mut out = Vec::with_capacity(n);
    let mut v = start as i64;
    for _ in 0..n {
        out.push(Value::Int(v as i32));
        v += step as i64;
    }
    Ok(out)
}

/// How many values `range(start, stop, step)` yields, refusing zero steps and
/// anything longer than [`MAX_LIST_LEN`].
pub fn range_len(start: i32, stop: i32, step: i32) -> Result<usize, Fault> {
    if step == 0 {
        return Err(bad("range() step must not be zero"));
    }
    // 64-bit throughout: `stop - start` overflows i32 for e.g. range(i32::MIN, i32::MAX).
    let span = stop as i64 - start as i64;
    let step64 = step as i64;
    let n = if (span > 0) != (step64 > 0) || span == 0 {
        0
    } else {
        ((span + step64 - step64.signum()) / step64) as usize
    };
    if n > MAX_LIST_LEN {
        return Err(bad(alloc::format!(
            "range() of {} values exceeds the {} element limit",
            n,
            MAX_LIST_LEN
        )));
    }
    Ok(n)
}

fn fold_extreme(name: &str, args: &[Value], want: core::cmp::Ordering) -> R {
    // min(list) and min(a, b, ...) are both spelled the same way in Python.
    let owned;
    let items: &[Value] = if args.len() == 1 {
        match &args[0] {
            Value::List(l) => {
                owned = l.borrow().clone();
                &owned
            }
            _ => return Err(bad(alloc::format!("{}() of a single non-list value", name))),
        }
    } else {
        args
    };
    if items.is_empty() {
        return Err(bad(alloc::format!("{}() of an empty sequence", name)));
    }
    let mut best = items[0].clone();
    for v in &items[1..] {
        let ord = v.cmp(&best).ok_or_else(|| {
            bad(alloc::format!("{}() cannot compare {} and {}", name, v.type_name(), best.type_name()))
        })?;
        if ord == want {
            best = v.clone();
        }
    }
    Ok(best)
}

// ------------------------------------------------------------------ methods

/// `recv.name(args)`.
pub fn method(recv: &Value, name: &str, args: &[Value]) -> R {
    match recv {
        Value::List(l) => list_method(l, name, args),
        Value::Str(s) => str_method(s, name, args),
        other => Err(bad(alloc::format!("{} has no method '{}'", other.type_name(), name))),
    }
}

fn list_method(l: &crate::value::ListRef, name: &str, args: &[Value]) -> R {
    // Every arm borrows for as short a time as possible: a script can pass the
    // same list as both receiver and argument (`a.extend(a)`), and holding a
    // mutable borrow across that would panic.
    match name {
        "append" => {
            arity("append", args, 1, 1)?;
            let mut me = l.borrow_mut();
            // Capped like extend() and insert(). Without this, `while True:
            // a.append(0)` is four words of Python that exhausts a 256 KiB heap
            // and takes the firmware down with the allocator.
            if me.len() >= MAX_LIST_LEN {
                return Err(bad("list would exceed the element limit"));
            }
            me.push(args[0].clone());
            Ok(Value::None)
        }
        "extend" => {
            arity("extend", args, 1, 1)?;
            let Value::List(other) = &args[0] else {
                return Err(bad("extend() expects a list"));
            };
            let items = other.borrow().clone();
            let mut me = l.borrow_mut();
            if me.len() + items.len() > MAX_LIST_LEN {
                return Err(bad("list would exceed the element limit"));
            }
            me.extend(items);
            Ok(Value::None)
        }
        "pop" => {
            arity("pop", args, 0, 1)?;
            let mut me = l.borrow_mut();
            let n = me.len();
            if n == 0 {
                return Err(bad("pop from an empty list"));
            }
            let idx = if args.is_empty() {
                n - 1
            } else {
                resolve_index(int_arg("pop", args, 0)?, n).ok_or_else(|| bad("pop index out of range"))?
            };
            Ok(me.remove(idx))
        }
        "insert" => {
            arity("insert", args, 2, 2)?;
            let mut me = l.borrow_mut();
            if me.len() >= MAX_LIST_LEN {
                return Err(bad("list would exceed the element limit"));
            }
            let n = me.len();
            let raw = int_arg("insert", args, 0)?;
            // insert() clamps rather than failing, which is what Python does.
            let idx = if raw < 0 { (n as i64 + raw as i64).max(0) as usize } else { (raw as usize).min(n) };
            me.insert(idx, args[1].clone());
            Ok(Value::None)
        }
        "remove" => {
            arity("remove", args, 1, 1)?;
            let mut me = l.borrow_mut();
            match me.iter().position(|v| v.eq(&args[0])) {
                Some(i) => {
                    me.remove(i);
                    Ok(Value::None)
                }
                None => Err(bad("remove(): value not in list")),
            }
        }
        "index" => {
            arity("index", args, 1, 1)?;
            let me = l.borrow();
            match me.iter().position(|v| v.eq(&args[0])) {
                Some(i) => Ok(Value::Int(i as i32)),
                None => Err(bad("index(): value not in list")),
            }
        }
        "count" => {
            arity("count", args, 1, 1)?;
            let me = l.borrow();
            Ok(Value::Int(me.iter().filter(|v| v.eq(&args[0])).count() as i32))
        }
        "clear" => {
            arity("clear", args, 0, 0)?;
            l.borrow_mut().clear();
            Ok(Value::None)
        }
        "reverse" => {
            arity("reverse", args, 0, 0)?;
            l.borrow_mut().reverse();
            Ok(Value::None)
        }
        "copy" => {
            arity("copy", args, 0, 0)?;
            let items = l.borrow().clone();
            Ok(Value::list(items))
        }
        "sort" => {
            arity("sort", args, 0, 0)?;
            // Pull the items out, sort, put them back: sorting in place through
            // a RefCell borrow would hold it across the comparator.
            let mut items = l.borrow().clone();
            let mut incomparable = false;
            items.sort_by(|a, b| {
                a.cmp(b).unwrap_or_else(|| {
                    incomparable = true;
                    core::cmp::Ordering::Equal
                })
            });
            if incomparable {
                return Err(bad("sort() cannot compare these values"));
            }
            *l.borrow_mut() = items;
            Ok(Value::None)
        }
        _ => Err(bad(alloc::format!("list has no method '{}'", name))),
    }
}

fn str_method(s: &alloc::rc::Rc<str>, name: &str, args: &[Value]) -> R {
    match name {
        "upper" => {
            arity("upper", args, 0, 0)?;
            Ok(Value::str(s.to_uppercase()))
        }
        "lower" => {
            arity("lower", args, 0, 0)?;
            Ok(Value::str(s.to_lowercase()))
        }
        "strip" => {
            arity("strip", args, 0, 0)?;
            Ok(Value::str(s.trim()))
        }
        "startswith" => {
            arity("startswith", args, 1, 1)?;
            Ok(Value::Bool(s.starts_with(str_arg("startswith", args, 0)?)))
        }
        "endswith" => {
            arity("endswith", args, 1, 1)?;
            Ok(Value::Bool(s.ends_with(str_arg("endswith", args, 0)?)))
        }
        "find" => {
            arity("find", args, 1, 1)?;
            let needle = str_arg("find", args, 0)?;
            // Python returns a character index, not a byte offset.
            Ok(Value::Int(match s.find(needle) {
                Some(byte) => s[..byte].chars().count() as i32,
                None => -1,
            }))
        }
        "replace" => {
            arity("replace", args, 2, 2)?;
            let from = str_arg("replace", args, 0)?;
            if from.is_empty() {
                return Err(bad("replace() with an empty pattern"));
            }
            let to = str_arg("replace", args, 1)?;
            // `'a'.replace('a', 'aa')` doubles, so a loop of twenty of them is
            // a megabyte. Size the result before building it.
            if to.len() > from.len() {
                let grown = s.matches(from).count() * (to.len() - from.len()) + s.len();
                if grown > MAX_STR_LEN {
                    return Err(bad("replace() result would be too long"));
                }
            }
            Ok(Value::str(s.replace(from, to)))
        }
        "split" => {
            arity("split", args, 0, 1)?;
            // Count first. Collecting and then checking would allocate one
            // `Value` per piece before deciding the answer was too big, and a
            // 16 KiB string split on a single character is 16 thousand of them.
            let sep = if args.is_empty() { None } else { Some(str_arg("split", args, 0)?) };
            if sep == Some("") {
                return Err(bad("split() with an empty separator"));
            }
            let n = match sep {
                None => s.split_whitespace().count(),
                Some(sep) => s.split(sep).count(),
            };
            if n > MAX_LIST_LEN {
                return Err(bad("split() would produce too many pieces"));
            }
            let parts: Vec<Value> = match sep {
                None => s.split_whitespace().map(Value::str).collect(),
                Some(sep) => s.split(sep).map(Value::str).collect(),
            };
            Ok(Value::list(parts))
        }
        "join" => {
            arity("join", args, 1, 1)?;
            let Value::List(l) = &args[0] else {
                return Err(bad("join() expects a list"));
            };
            let items = l.borrow();
            // Measure before building: 4096 strings of 16 KiB is 64 MB, and the
            // heap is 256 KiB.
            let mut total = s.len().saturating_mul(items.len().saturating_sub(1));
            for v in items.iter() {
                match v {
                    Value::Str(p) => total = total.saturating_add(p.len()),
                    other => {
                        return Err(bad(alloc::format!("join() cannot join {}", other.type_name())));
                    }
                }
            }
            if total > MAX_STR_LEN {
                return Err(bad("join() result would be too long"));
            }
            let mut out = String::with_capacity(total);
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(s);
                }
                if let Value::Str(p) = v {
                    out.push_str(p);
                }
            }
            Ok(Value::str(out))
        }
        _ => Err(bad(alloc::format!("str has no method '{}'", name))),
    }
}

// `Fault` needs a Debug-ish escape hatch for `Result::is_err` in tests without
// requiring Debug on Value.
impl core::fmt::Debug for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Fault::Msg(m) => write!(f, "Fault({})", m),
            Fault::Abort => write!(f, "Abort"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::NullHost;

    #[test]
    fn range_length_matches_python() {
        assert_eq!(range_len(0, 5, 1).unwrap(), 5);
        assert_eq!(range_len(0, 5, 2).unwrap(), 3);
        assert_eq!(range_len(5, 0, -1).unwrap(), 5);
        assert_eq!(range_len(0, 0, 1).unwrap(), 0);
        assert_eq!(range_len(5, 0, 1).unwrap(), 0);
        assert!(range_len(0, 1, 0).is_err());
    }

    #[test]
    fn range_span_does_not_overflow() {
        // stop - start overflows i32 here; the limit check must still fire
        // rather than the subtraction wrapping into a small positive count.
        assert!(range_len(i32::MIN, i32::MAX, 1).is_err());
    }

    #[test]
    fn range_is_capped() {
        assert!(range_len(0, 1_000_000, 1).is_err());
    }

    #[test]
    fn min_max_over_a_list_and_over_args() {
        let mut h = NullHost::default();
        let l = Value::list(alloc::vec![Value::Int(3), Value::Int(1), Value::Int(2)]);
        let got = call(Builtin::Min, &[l], &mut h).ok().unwrap();
        assert!(got.eq(&Value::Int(1)));
        let got = call(Builtin::Max, &[Value::Int(3), Value::Int(9)], &mut h).ok().unwrap();
        assert!(got.eq(&Value::Int(9)));
    }

    #[test]
    fn extend_with_itself_does_not_panic() {
        let l = Value::list(alloc::vec![Value::Int(1)]);
        let Value::List(inner) = &l else { panic!() };
        method(&l, "extend", &[Value::List(inner.clone())]).ok().unwrap();
        assert_eq!(inner.borrow().len(), 2);
    }

    #[test]
    fn str_find_returns_character_indices() {
        let s = Value::str("aébc");
        let got = method(&s, "find", &[Value::str("b")]).ok().unwrap();
        assert!(got.eq(&Value::Int(2)), "expected a char index, got {}", got.to_display());
    }

    #[test]
    fn int_parses_prefixes_and_signs() {
        let mut h = NullHost::default();
        let v = call(Builtin::Int, &[Value::str("0x1f"), Value::Int(16)], &mut h).ok().unwrap();
        assert!(v.eq(&Value::Int(31)));
        let v = call(Builtin::Int, &[Value::str(" -42 ")], &mut h).ok().unwrap();
        assert!(v.eq(&Value::Int(-42)));
        assert!(call(Builtin::Int, &[Value::str("zz")], &mut h).is_err());
    }
}
