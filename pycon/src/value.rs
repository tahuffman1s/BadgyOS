//! Runtime values.
//!
//! Five types, chosen to be what a badge script actually needs and nothing
//! more. `Value` is `Clone` and cheap to clone: strings and lists are
//! reference-counted, so passing one around copies a pointer.
//!
//! There are no floats. The core has no FPU, and soft-float would cost more
//! code than the whole evaluator; scripts get 32-bit wrapping integers, and
//! `/` means floor division.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::ast::FuncId;

/// A list, shared by reference the way Python shares them: `b = a` aliases.
pub type ListRef = Rc<RefCell<Vec<Value>>>;

#[derive(Clone)]
pub enum Value {
    None,
    Bool(bool),
    Int(i32),
    Str(Rc<str>),
    List(ListRef),
    /// A user-defined function, by index into `Ast::funcs`.
    Func(FuncId),
}

/// How deep the recursive value operations (compare, format) will follow nested
/// lists before giving up.
///
/// This is not a stylistic limit -- `a = []; a.append(a)` builds a cycle that
/// reference counting cannot see, and without a cap `print(a)` would recurse
/// forever and take the stack with it.
const MAX_VALUE_DEPTH: u32 = 16;

/// How many values one compare or format may visit.
///
/// A depth cap alone is not enough, because the cost is exponential in the
/// *fan-out*, not the depth: `a = []; a.append(a); a.append(a)` is two elements
/// deep-one, but walking it to depth 16 visits 2^16 nodes, and a third append
/// makes it 43 million. Depth bounds the stack; this bounds the time, which is
/// what stops a two-line script from freezing the badge inside a single
/// `print()` where no interpreter step is counted and no tick can fire.
const MAX_VALUE_VISITS: u32 = 4096;

/// Longest string this interpreter will construct.
///
/// `s = s + s` in a loop doubles, so twenty iterations is a megabyte and the
/// heap is a quarter of that. Every operation that can grow a string checks
/// against this, and the limit is deliberately generous next to a screen that
/// holds 210 characters.
pub const MAX_STR_LEN: usize = 16 * 1024;

/// Longest text `str()` and `print()` will produce from a container.
const MAX_REPR_LEN: usize = 4096;

// `eq`, `cmp` and `len` shadow names clippy associates with `PartialEq`, `Ord`
// and the `is_empty` convention. None of the three can be the trait version:
// equality and ordering carry a visit budget and a depth (a script can build a
// self-referential list, and the recursive versions would either loop or blow
// the stack), ordering is partial by design because Python refuses to compare a
// string with an int, and `len` returns `Option` because most values have no
// length at all -- which is also why `is_empty` would be meaningless.
#[allow(
    clippy::should_implement_trait,
    clippy::len_without_is_empty,
    reason = "Python semantics, not Rust trait semantics -- see above"
)]
impl Value {
    pub fn list(items: Vec<Value>) -> Value { Value::List(Rc::new(RefCell::new(items))) }

    pub fn str(s: impl AsRef<str>) -> Value { Value::Str(Rc::from(s.as_ref())) }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::None => "None",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Str(_) => "str",
            Value::List(_) => "list",
            Value::Func(_) => "function",
        }
    }

    /// Python's truthiness: zero, empty and `None` are false.
    pub fn truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Str(s) => !s.is_empty(),
            Value::List(l) => !l.borrow().is_empty(),
            Value::Func(_) => true,
        }
    }

    /// The `str()` form: what `print` shows.
    pub fn to_display(&self) -> String {
        let mut out = String::new();
        let mut visits = MAX_VALUE_VISITS;
        self.fmt_into(&mut out, false, 0, &mut visits);
        out
    }

    /// The `repr()` form, which quotes strings. Used inside containers so that
    /// `print(['a'])` reads as a list of one string rather than one letter.
    pub fn to_repr(&self) -> String {
        let mut out = String::new();
        let mut visits = MAX_VALUE_VISITS;
        self.fmt_into(&mut out, true, 0, &mut visits);
        out
    }

    /// Append this value's text to `out`, stopping at the depth, visit and
    /// length limits. Writing into one buffer rather than returning a `String`
    /// per node is also what keeps a nested list from allocating quadratically.
    fn fmt_into(&self, out: &mut String, quoted: bool, depth: u32, visits: &mut u32) {
        if depth > MAX_VALUE_DEPTH || *visits == 0 || out.len() >= MAX_REPR_LEN {
            out.push_str("...");
            return;
        }
        *visits -= 1;

        match self {
            Value::None => out.push_str("None"),
            Value::Bool(true) => out.push_str("True"),
            Value::Bool(false) => out.push_str("False"),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Str(s) => {
                // Truncate rather than refuse: a long string is a legitimate
                // thing to hold, it just is not a useful thing to print in full
                // to a 21-column screen or a 1 Mbaud console.
                let room = MAX_REPR_LEN.saturating_sub(out.len());
                let end = s.char_indices().map(|(i, _)| i).take_while(|i| *i < room).last().unwrap_or(0);
                let body = if s.len() <= room { &**s } else { &s[..end] };
                if quoted {
                    out.push('\'');
                    out.push_str(body);
                    out.push('\'');
                } else {
                    out.push_str(body);
                }
                if body.len() < s.len() {
                    out.push_str("...");
                }
            }
            Value::List(l) => {
                // A list being formatted may also be a list being iterated, so
                // borrow() rather than borrow_mut() -- and if some other borrow
                // is live, say so instead of panicking.
                let Ok(items) = l.try_borrow() else {
                    out.push_str("[...]");
                    return;
                };
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if out.len() >= MAX_REPR_LEN || *visits == 0 {
                        out.push_str("...");
                        break;
                    }
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.fmt_into(out, true, depth + 1, visits);
                }
                out.push(']');
            }
            Value::Func(_) => out.push_str("<function>"),
        }
    }

    /// Structural equality. Different types are simply unequal, as in Python --
    /// except that `True == 1`, which Python also agrees with.
    pub fn eq(&self, other: &Value) -> bool {
        let mut visits = MAX_VALUE_VISITS;
        self.eq_at(other, 0, &mut visits)
    }

    fn eq_at(&self, other: &Value, depth: u32, visits: &mut u32) -> bool {
        if depth > MAX_VALUE_DEPTH || *visits == 0 {
            // Out of budget: report "not equal" rather than guess. Two values
            // this tangled are not something a script should be comparing.
            return false;
        }
        *visits -= 1;
        match (self, other) {
            (Value::None, Value::None) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Bool(a), Value::Int(b)) | (Value::Int(b), Value::Bool(a)) => (*a as i32) == *b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                // Identity first: it is both the fast path and the only way a
                // self-referential list can compare equal to itself.
                if Rc::ptr_eq(a, b) {
                    return true;
                }
                let (Ok(a), Ok(b)) = (a.try_borrow(), b.try_borrow()) else {
                    return false;
                };
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_at(y, depth + 1, visits))
            }
            (Value::Func(a), Value::Func(b)) => a == b,
            _ => false,
        }
    }

    /// Ordering for `<`, `>`, `<=`, `>=`. `None` means the two are not
    /// comparable, which the caller turns into a runtime error.
    pub fn cmp(&self, other: &Value) -> Option<core::cmp::Ordering> {
        let mut visits = MAX_VALUE_VISITS;
        self.cmp_at(other, 0, &mut visits)
    }

    fn cmp_at(&self, other: &Value, depth: u32, visits: &mut u32) -> Option<core::cmp::Ordering> {
        use core::cmp::Ordering;
        if depth > MAX_VALUE_DEPTH || *visits == 0 {
            return None;
        }
        *visits -= 1;
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Int(b)) => Some((*a as i32).cmp(b)),
            (Value::Int(a), Value::Bool(b)) => Some(a.cmp(&(*b as i32))),
            (Value::Str(a), Value::Str(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
            (Value::List(a), Value::List(b)) => {
                // Identity first, as in `eq`: it is the fast path, and it is
                // the only way a self-referential list can be ordered against
                // itself without exhausting the visit budget. Sorting a list
                // that holds the same list twice hits this.
                if Rc::ptr_eq(a, b) {
                    return Some(Ordering::Equal);
                }
                let (Ok(a), Ok(b)) = (a.try_borrow(), b.try_borrow()) else {
                    return None;
                };
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.cmp_at(y, depth + 1, visits)? {
                        Ordering::Equal => continue,
                        ord => return Some(ord),
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            _ => None,
        }
    }

    /// `x in y`.
    pub fn contains(&self, needle: &Value) -> Option<bool> {
        match self {
            Value::List(l) => {
                let items = l.try_borrow().ok()?;
                Some(items.iter().any(|v| v.eq(needle)))
            }
            Value::Str(s) => match needle {
                Value::Str(n) => Some(s.contains(&**n)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Length, for `len()` and for iteration.
    pub fn len(&self) -> Option<usize> {
        match self {
            Value::Str(s) => Some(s.chars().count()),
            Value::List(l) => Some(l.borrow().len()),
            _ => None,
        }
    }
}

/// Resolve a possibly-negative Python index against a length.
///
/// Returns `None` when it is out of range, which the caller reports as an
/// "index out of range" error rather than clamping -- silently reading the
/// wrong element is a much harder bug to find on a screen with no debugger.
pub fn resolve_index(idx: i32, len: usize) -> Option<usize> {
    let len_i = len as i64;
    let i = idx as i64;
    let i = if i < 0 { i + len_i } else { i };
    if i < 0 || i >= len_i { None } else { Some(i as usize) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_matches_python() {
        assert!(!Value::None.truthy());
        assert!(!Value::Int(0).truthy());
        assert!(Value::Int(-1).truthy());
        assert!(!Value::str("").truthy());
        assert!(Value::str("x").truthy());
        assert!(!Value::list(alloc::vec![]).truthy());
    }

    #[test]
    fn display_quotes_strings_only_inside_containers() {
        assert_eq!(Value::str("hi").to_display(), "hi");
        assert_eq!(Value::list(alloc::vec![Value::str("hi")]).to_display(), "['hi']");
    }

    #[test]
    fn bools_and_ints_compare_equal() {
        assert!(Value::Bool(true).eq(&Value::Int(1)));
        assert!(Value::Int(0).eq(&Value::Bool(false)));
    }

    #[test]
    fn self_referential_list_does_not_hang() {
        let l = Rc::new(RefCell::new(Vec::new()));
        l.borrow_mut().push(Value::List(l.clone()));
        let v = Value::List(l.clone());
        // Both of these used to be infinite recursion.
        let _ = v.to_display();
        assert!(v.eq(&v));
    }

    #[test]
    fn negative_indices_resolve_from_the_end() {
        assert_eq!(resolve_index(-1, 3), Some(2));
        assert_eq!(resolve_index(0, 3), Some(0));
        assert_eq!(resolve_index(3, 3), None);
        assert_eq!(resolve_index(-4, 3), None);
        assert_eq!(resolve_index(0, 0), None);
    }

    #[test]
    fn lists_compare_lexicographically() {
        let a = Value::list(alloc::vec![Value::Int(1), Value::Int(2)]);
        let b = Value::list(alloc::vec![Value::Int(1), Value::Int(3)]);
        assert_eq!(a.cmp(&b), Some(core::cmp::Ordering::Less));
    }
}
