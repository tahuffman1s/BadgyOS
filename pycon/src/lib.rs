//! `pycon` -- a small Python-subset interpreter sized for a badge.
//!
//! # Why a subset, and which one
//!
//! The badge is an RV32IMAC core with no FPU, a few hundred KiB of heap and a
//! 16 KiB-ish stack. A real Python is out of the question, so this implements
//! the part of the language people actually reach for when they write a
//! twenty-line animation or a button toy:
//!
//! * `int` (32-bit, wrapping), `str`, `bool`, `None`, `list`
//! * `if` / `elif` / `else`, `while`, `for x in ...`, `break`, `continue`
//! * `def` with positional parameters, `return`, `global`
//! * the usual arithmetic, comparison, boolean and bitwise operators
//! * indexing and slicing-free list/string access, `len()`, `range()`
//! * a handful of list and string methods (`append`, `pop`, `upper`, ...)
//!
//! Deliberately absent: classes, imports, exceptions, closures, lambdas,
//! generators, decorators, dicts, tuples, comprehensions, floats. Each of those
//! costs more code than it would earn on a 128x128 screen, and floats in
//! particular would drag in soft-float routines that dwarf the interpreter.
//! Scripts that use them get a clean syntax error rather than silent weirdness.
//!
//! # Shape of the implementation
//!
//! [`lexer`] turns source into tokens, resolving significant indentation into
//! explicit `Indent`/`Dedent`. [`parser`] builds an [`ast::Ast`], which is an
//! *arena*: every node lives in a flat `Vec` and children are `u32` indices.
//! That matters here for two reasons -- it avoids a heap allocation per node,
//! and it means dropping a program is a handful of `Vec` frees rather than a
//! recursive walk that could overflow the stack on a deeply nested expression.
//! [`interp`] walks the arena.
//!
//! # Staying interruptible
//!
//! The firmware has no scheduler: the interpreter runs inside the same polling
//! loop that services the screen, the keys and USB. So the evaluator counts
//! steps, and every [`Host::TICK_INTERVAL`] steps it calls [`Host::tick`]. The
//! firmware uses that to pump USB and to check whether the user is holding the
//! button down to kill a runaway script. Recursion is bounded too -- see
//! [`interp::MAX_CALL_DEPTH`] and [`parser::MAX_PARSE_DEPTH`] -- so neither a
//! deeply nested literal nor an infinite recursion can walk off the stack.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod ast;
pub mod builtins;
pub mod host;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod value;

use alloc::string::String;

pub use crate::ast::Ast;
pub use crate::host::{Abort, Host};
pub use crate::interp::{Completion, Interp};
pub use crate::value::Value;

/// Where something went wrong, in source terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// 1-based line number, or 0 if the failure is not tied to a line.
    pub line: u32,
    pub msg: String,
}

impl Error {
    pub fn new(line: u32, msg: impl Into<String>) -> Self { Error { line, msg: msg.into() } }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.line > 0 { write!(f, "line {}: {}", self.line, self.msg) } else { write!(f, "{}", self.msg) }
    }
}

/// A parsed, ready-to-run program.
pub struct Script {
    pub ast: Ast,
}

impl Script {
    /// Tokenize and parse `src`. Nothing runs yet, so this only reports syntax
    /// errors -- an undefined name is a runtime error, as in Python.
    pub fn compile(src: &str) -> Result<Script, Error> {
        let tokens = lexer::tokenize(src)?;
        let ast = parser::parse(&tokens)?;
        Ok(Script { ast })
    }

    /// Run to completion, or until `host` asks to stop.
    pub fn run(&self, host: &mut dyn Host) -> Result<Completion, Error> {
        let mut interp = Interp::new(&self.ast);
        interp.run(host)
    }
}
