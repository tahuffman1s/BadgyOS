//! The evaluator: a tree-walker over the [`Ast`] arena.
//!
//! # Why a tree-walker and not a bytecode VM
//!
//! A bytecode VM would run maybe two to three times faster, but it needs a
//! compiler, a constant pool, a stack machine and a disassembler to debug any
//! of it -- call it three times the code for a device whose bottleneck is a
//! 2 MHz SPI panel refresh, not the interpreter. Walking the arena keeps the
//! whole thing auditable and small, and "fast enough to animate 128x128" is a
//! low bar.
//!
//! # Staying out of the firmware's way
//!
//! Two counters do all the work of keeping a bad script from taking the badge
//! down with it. [`Interp::steps`] triggers a [`Host::tick`] every
//! `TICK_INTERVAL` statements, which is where USB gets serviced and where the
//! user's "stop" gesture is noticed. [`Interp::depth`] caps recursion, because
//! `def f(): f()` is three characters of infinite stack growth.

use alloc::string::String;
use alloc::vec::Vec;

use crate::Error;
use crate::ast::*;
use crate::builtins::{self, Builtin, Fault};
use crate::host::{Abort, Host};
use crate::value::{Value, resolve_index};

/// Maximum nesting of user function calls.
pub const MAX_CALL_DEPTH: u32 = 16;

/// Maximum nesting of the recursive evaluator, counting both expression levels
/// and block levels. Bounds stack use regardless of how the nesting is spread
/// between deep expressions and deep call chains.
pub const MAX_EVAL_DEPTH: u32 = 96;

/// How a script ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Ran off the end of the program, or hit a top-level `return`.
    Finished,
    /// The host asked it to stop.
    Aborted,
}

/// Non-local exits. Kept separate from [`Flow`] because these unwind all the
/// way out, whereas `break`/`continue`/`return` are caught by a loop or a call.
enum Trap {
    Err(Error),
    Abort,
}

impl From<Abort> for Trap {
    fn from(_: Abort) -> Self { Trap::Abort }
}

/// What a statement did to control flow.
enum Flow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

type Eval = Result<Value, Trap>;
type Exec = Result<Flow, Trap>;

struct Frame {
    /// Function locals. A linear scan beats a map here: functions have a
    /// handful of names, and a `Vec` costs one allocation instead of many.
    locals: Vec<(NameId, Value)>,
    /// Names this function declared `global`.
    global_decls: Vec<NameId>,
}

pub struct Interp<'a> {
    ast: &'a Ast,
    /// Indexed by `NameId`, so a global lookup is an array index.
    globals: Vec<Option<Value>>,
    /// Precomputed `NameId -> Builtin`, so calling `print` costs no string
    /// comparison at runtime.
    builtin_of: Vec<Option<Builtin>>,
    /// Precomputed `NameId -> constant value` for `WIDTH`, `KEY_UP` and friends.
    const_of: Vec<Option<Value>>,
    frames: Vec<Frame>,
    steps: u32,
    tick_interval: u32,
    depth: u32,
    /// Line of the statement being executed, for error messages.
    line: u32,
}

impl<'a> Interp<'a> {
    pub fn new(ast: &'a Ast) -> Self {
        let n = ast.name_count();
        let mut builtin_of = Vec::with_capacity(n);
        let mut const_of = Vec::with_capacity(n);
        for i in 0..n {
            let name = ast.name(i as NameId);
            builtin_of.push(Builtin::from_name(name));
            const_of.push(builtins::constant(name));
        }
        Interp {
            ast,
            globals: alloc::vec![None; n],
            builtin_of,
            const_of,
            frames: Vec::new(),
            steps: 0,
            // Replaced by the host's own cadence in `run`.
            tick_interval: 2048,
            depth: 0,
            line: 0,
        }
    }

    /// Run the program.
    pub fn run(&mut self, host: &mut dyn Host) -> Result<Completion, Error> {
        self.tick_interval = host.tick_interval().max(1);

        // Hoist every `def` before running, so a script can call a helper that
        // is defined further down the file -- which is how people write them.
        let top = self.ast.top;
        for &sid in self.ast.block(top) {
            if let Stmt::Def(fid) = self.ast.stmt(sid) {
                let name = self.ast.funcs[*fid as usize].name;
                self.globals[name as usize] = Some(Value::Func(*fid));
            }
        }

        let outcome = match self.exec_block(top, host) {
            Ok(_) => Ok(Completion::Finished),
            Err(Trap::Abort) => Ok(Completion::Aborted),
            Err(Trap::Err(e)) => Err(e),
        };
        self.teardown();
        outcome
    }

    /// Release every value the script left behind, without recursing.
    ///
    /// `a = [a]` in a loop builds a list nested as deep as the heap allows, and
    /// dropping that normally is a recursion one frame deep per level -- on a
    /// device with no guard page, which turns "the script used a lot of memory"
    /// into silent corruption of whatever is below the stack. Emptying each
    /// list into an explicit worklist first makes every individual drop
    /// shallow, and moves the depth onto the heap where it is merely finite.
    fn teardown(&mut self) {
        let mut work: Vec<Value> = Vec::new();
        for slot in self.globals.iter_mut() {
            if let Some(v) = slot.take() {
                work.push(v);
            }
        }
        for frame in self.frames.drain(..) {
            work.extend(frame.locals.into_iter().map(|(_, v)| v));
        }
        while let Some(v) = work.pop() {
            if let Value::List(l) = v {
                if let Ok(mut items) = l.try_borrow_mut() {
                    // Moves the children out, so the list this `l` points at is
                    // empty by the time its last reference goes away.
                    work.append(&mut items);
                }
            }
        }
    }

    /// Override the tick cadence. The firmware uses this to poll USB more often
    /// while a drive is mounted.
    pub fn set_tick_interval(&mut self, steps: u32) { self.tick_interval = steps.max(1); }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, Trap> { Err(Trap::Err(Error::new(self.line, msg))) }

    /// Charge one step, and hand control back to the host every so often.
    fn step(&mut self, host: &mut dyn Host) -> Result<(), Trap> {
        self.steps += 1;
        if self.steps >= self.tick_interval {
            self.steps = 0;
            host.tick()?;
            if host.heap_pressure() {
                // Stop while there is still enough heap left to build the error
                // message and unwind. Waiting for the allocator to fail would
                // mean panicking, and a panic here is unrecoverable.
                return self.err("out of memory");
            }
        }
        Ok(())
    }

    fn enter(&mut self) -> Result<(), Trap> {
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return self.err("expression or call nested too deeply");
        }
        Ok(())
    }

    fn leave(&mut self) { self.depth -= 1; }

    // ---------------------------------------------------------------- statements

    fn exec_block(&mut self, block: Block, host: &mut dyn Host) -> Exec {
        self.enter()?;
        let r = (|| {
            // Copy the ids out: `self.ast` is borrowed immutably for 'a, so this
            // is only to keep the borrow checker happy about `&mut self` below.
            for i in block.range() {
                let sid = self.ast.stmt_list[i];
                match self.exec_stmt(sid, host)? {
                    Flow::Normal => (),
                    other => return Ok(other),
                }
            }
            Ok(Flow::Normal)
        })();
        self.leave();
        r
    }

    fn exec_stmt(&mut self, sid: StmtId, host: &mut dyn Host) -> Exec {
        self.step(host)?;
        self.line = self.ast.stmt_line(sid);

        match self.ast.stmt(sid).clone() {
            Stmt::Pass => Ok(Flow::Normal),
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),

            Stmt::Expr(e) => {
                self.eval(e, host)?;
                Ok(Flow::Normal)
            }

            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e, host)?,
                    None => Value::None,
                };
                Ok(Flow::Return(v))
            }

            Stmt::Global(names) => {
                if let Some(frame) = self.frames.last_mut() {
                    for &n in &self.ast.name_list[names.range()] {
                        if !frame.global_decls.contains(&n) {
                            frame.global_decls.push(n);
                        }
                        // A name declared global must not also be a local, or
                        // the local would shadow the very binding we just
                        // promised to write through.
                        frame.locals.retain(|(k, _)| *k != n);
                    }
                }
                Ok(Flow::Normal)
            }

            Stmt::Def(fid) => {
                let name = self.ast.funcs[fid as usize].name;
                self.bind(name, Value::Func(fid));
                Ok(Flow::Normal)
            }

            Stmt::Assign { name, value } => {
                let v = self.eval(value, host)?;
                self.bind(name, v);
                Ok(Flow::Normal)
            }

            Stmt::AugAssign { name, op, value } => {
                let cur = self.load(name)?;
                let rhs = self.eval(value, host)?;
                let v = self.binop(op, &cur, &rhs)?;
                self.bind(name, v);
                Ok(Flow::Normal)
            }

            Stmt::SetIndex { target, index, value } => {
                let t = self.eval(target, host)?;
                let i = self.eval(index, host)?;
                let v = self.eval(value, host)?;
                self.store_index(&t, &i, v)
            }

            Stmt::AugSetIndex { target, index, op, value } => {
                let t = self.eval(target, host)?;
                let i = self.eval(index, host)?;
                let cur = self.load_index(&t, &i)?;
                let rhs = self.eval(value, host)?;
                let v = self.binop(op, &cur, &rhs)?;
                self.store_index(&t, &i, v)
            }

            Stmt::If { arms, orelse } => {
                for k in arms.range() {
                    let (cond, body) = self.ast.arms[k];
                    if self.eval(cond, host)?.truthy() {
                        return self.exec_block(body, host);
                    }
                }
                match orelse {
                    Some(b) => self.exec_block(b, host),
                    None => Ok(Flow::Normal),
                }
            }

            Stmt::While { cond, body } => {
                loop {
                    self.step(host)?;
                    if !self.eval(cond, host)?.truthy() {
                        break;
                    }
                    match self.exec_block(body, host)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => (),
                    }
                }
                Ok(Flow::Normal)
            }

            Stmt::For { var, iter, body } => self.exec_for(var, iter, body, host),
        }
    }

    /// `for` has a fast path worth having: iterating `range(...)` directly
    /// instead of materializing the list. `for i in range(4096)` would otherwise
    /// allocate 4096 `Value`s before the first iteration -- and anything larger
    /// would be refused outright by the list cap, even though the loop itself
    /// needs no memory at all.
    fn exec_for(&mut self, var: NameId, iter: ExprId, body: Block, host: &mut dyn Host) -> Exec {
        if let Expr::Call(callee, args) = *self.ast.expr(iter) {
            if self.is_plain_builtin(callee, Builtin::Range) {
                let a = self.ast.exprs_of(args).to_vec();
                if a.is_empty() || a.len() > 3 {
                    return self.err("range() takes 1 to 3 arguments");
                }
                let mut n = [0i32; 3];
                for (i, e) in a.iter().enumerate() {
                    n[i] = match self.eval(*e, host)? {
                        Value::Int(v) => v,
                        Value::Bool(b) => b as i32,
                        other => {
                            return self.err(alloc::format!("range() needs ints, got {}", other.type_name()));
                        }
                    };
                }
                let (start, stop, step) = match a.len() {
                    1 => (0, n[0], 1),
                    2 => (n[0], n[1], 1),
                    _ => (n[0], n[1], n[2]),
                };
                if step == 0 {
                    return self.err("range() step must not be zero");
                }
                let mut cur = start as i64;
                loop {
                    let done = if step > 0 { cur >= stop as i64 } else { cur <= stop as i64 };
                    if done {
                        break;
                    }
                    self.step(host)?;
                    self.bind(var, Value::Int(cur as i32));
                    match self.exec_block(body, host)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => (),
                    }
                    cur += step as i64;
                }
                return Ok(Flow::Normal);
            }
        }

        let seq = self.eval(iter, host)?;
        match seq {
            Value::List(l) => {
                // Snapshot the length each pass rather than holding a borrow, so
                // the body may legally append to or shrink the list it is
                // walking. Mutating mid-iteration is a foot-gun in any language;
                // here it at least cannot panic.
                let mut i = 0usize;
                loop {
                    let item = {
                        let items = l.borrow();
                        if i >= items.len() {
                            break;
                        }
                        items[i].clone()
                    };
                    self.step(host)?;
                    self.bind(var, item);
                    match self.exec_block(body, host)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => (),
                    }
                    i += 1;
                }
                Ok(Flow::Normal)
            }
            Value::Str(s) => {
                for c in s.chars() {
                    self.step(host)?;
                    let mut buf = [0u8; 4];
                    self.bind(var, Value::str(&*c.encode_utf8(&mut buf)));
                    match self.exec_block(body, host)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => (),
                    }
                }
                Ok(Flow::Normal)
            }
            other => self.err(alloc::format!("cannot iterate over {}", other.type_name())),
        }
    }

    // ------------------------------------------------------------------- scoping

    /// Is `name` unbound as a variable and equal to the given builtin? Used for
    /// the `range()` fast path, so that a script which defines its own `range`
    /// still gets its own.
    fn is_plain_builtin(&self, name: NameId, want: Builtin) -> bool {
        if self.lookup(name).is_some() {
            return false;
        }
        self.builtin_of[name as usize] == Some(want)
    }

    /// Read a variable without consulting builtins or constants.
    fn lookup(&self, name: NameId) -> Option<Value> {
        if let Some(frame) = self.frames.last() {
            if !frame.global_decls.contains(&name) {
                if let Some((_, v)) = frame.locals.iter().find(|(k, _)| *k == name) {
                    return Some(v.clone());
                }
                // Fall through to globals: a function can *read* a global it
                // never assigns, which is how helper functions see module-level
                // configuration.
            }
        }
        self.globals[name as usize].clone()
    }

    /// Full name resolution, in Python's order: locals, globals, constants.
    fn load(&self, name: NameId) -> Eval {
        if let Some(v) = self.lookup(name) {
            return Ok(v);
        }
        if let Some(c) = &self.const_of[name as usize] {
            return Ok(c.clone());
        }
        if self.builtin_of[name as usize].is_some() {
            return Err(Trap::Err(Error::new(
                self.line,
                alloc::format!(
                    "'{}' is a builtin function; call it as {}(...)",
                    self.ast.name(name),
                    self.ast.name(name)
                ),
            )));
        }
        Err(Trap::Err(Error::new(self.line, alloc::format!("name '{}' is not defined", self.ast.name(name)))))
    }

    /// Assign, honouring `global`. Inside a function, an assignment creates a
    /// local unless the name was declared global.
    fn bind(&mut self, name: NameId, v: Value) {
        if let Some(frame) = self.frames.last_mut() {
            if !frame.global_decls.contains(&name) {
                if let Some(slot) = frame.locals.iter_mut().find(|(k, _)| *k == name) {
                    slot.1 = v;
                } else {
                    frame.locals.push((name, v));
                }
                return;
            }
        }
        self.globals[name as usize] = Some(v);
    }

    // ----------------------------------------------------------------- expressions

    fn eval(&mut self, id: ExprId, host: &mut dyn Host) -> Eval {
        self.enter()?;
        let r = self.eval_inner(id, host);
        self.leave();
        r
    }

    fn eval_inner(&mut self, id: ExprId, host: &mut dyn Host) -> Eval {
        match self.ast.expr(id).clone() {
            Expr::None => Ok(Value::None),
            Expr::Bool(b) => Ok(Value::Bool(b)),
            Expr::Int(i) => Ok(Value::Int(i)),
            Expr::Str(s) => Ok(Value::str(self.ast.string(s))),
            Expr::Name(n) => self.load(n),

            Expr::List(items) => {
                let ids = self.ast.exprs_of(items).to_vec();
                if ids.len() > builtins::MAX_LIST_LEN {
                    return self.err("list literal is too long");
                }
                let mut out = Vec::with_capacity(ids.len());
                for e in ids {
                    out.push(self.eval(e, host)?);
                }
                Ok(Value::list(out))
            }

            Expr::And(a, b) => {
                let lhs = self.eval(a, host)?;
                if lhs.truthy() { self.eval(b, host) } else { Ok(lhs) }
            }
            Expr::Or(a, b) => {
                let lhs = self.eval(a, host)?;
                if lhs.truthy() { Ok(lhs) } else { self.eval(b, host) }
            }

            Expr::Unary(op, e) => {
                let v = self.eval(e, host)?;
                match op {
                    UnOp::Not => Ok(Value::Bool(!v.truthy())),
                    UnOp::Pos => match v {
                        Value::Int(i) => Ok(Value::Int(i)),
                        Value::Bool(b) => Ok(Value::Int(b as i32)),
                        other => self.err(alloc::format!("cannot apply '+' to {}", other.type_name())),
                    },
                    UnOp::Neg => match v {
                        Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
                        Value::Bool(b) => Ok(Value::Int(-(b as i32))),
                        other => self.err(alloc::format!("cannot negate {}", other.type_name())),
                    },
                    UnOp::Invert => match v {
                        Value::Int(i) => Ok(Value::Int(!i)),
                        Value::Bool(b) => Ok(Value::Int(!(b as i32))),
                        other => self.err(alloc::format!("cannot invert {}", other.type_name())),
                    },
                }
            }

            Expr::Bin(op, a, b) => {
                let lhs = self.eval(a, host)?;
                let rhs = self.eval(b, host)?;
                self.binop(op, &lhs, &rhs)
            }

            Expr::Cmp(op, a, b) => {
                let lhs = self.eval(a, host)?;
                let rhs = self.eval(b, host)?;
                self.compare(op, &lhs, &rhs)
            }

            Expr::Index(target, index) => {
                let t = self.eval(target, host)?;
                let i = self.eval(index, host)?;
                self.load_index(&t, &i)
            }

            Expr::Method(recv, name, args) => {
                let r = self.eval(recv, host)?;
                let a = self.eval_args(args, host)?;
                builtins::method(&r, self.ast.name(name), &a).map_err(|f| self.fault(f))
            }

            Expr::Call(name, args) => self.call(name, args, host),
        }
    }

    fn eval_args(&mut self, args: Slice, host: &mut dyn Host) -> Result<Vec<Value>, Trap> {
        let ids = self.ast.exprs_of(args).to_vec();
        let mut out = Vec::with_capacity(ids.len());
        for e in ids {
            out.push(self.eval(e, host)?);
        }
        Ok(out)
    }

    fn call(&mut self, name: NameId, args: Slice, host: &mut dyn Host) -> Eval {
        // A binding shadows a builtin, so `def print(x)` really does replace it.
        let bound = self.lookup(name);
        let argv = self.eval_args(args, host)?;

        match bound {
            Some(Value::Func(fid)) => self.call_user(fid, argv, host),
            Some(other) => self.err(alloc::format!(
                "'{}' is a {}, not a function",
                self.ast.name(name),
                other.type_name()
            )),
            None => match self.builtin_of[name as usize] {
                Some(b) => builtins::call(b, &argv, host).map_err(|f| self.fault(f)),
                None => self.err(alloc::format!("name '{}' is not defined", self.ast.name(name))),
            },
        }
    }

    fn call_user(&mut self, fid: FuncId, argv: Vec<Value>, host: &mut dyn Host) -> Eval {
        let f = &self.ast.funcs[fid as usize];
        let params = self.ast.names_of(f.params).to_vec();
        let body = f.body;
        let fname = self.ast.name(f.name);

        if argv.len() != params.len() {
            return self.err(alloc::format!(
                "{}() takes {} argument(s), got {}",
                fname,
                params.len(),
                argv.len()
            ));
        }
        if self.frames.len() as u32 >= MAX_CALL_DEPTH {
            return self.err("too much recursion");
        }

        let locals = params.into_iter().zip(argv).collect();
        self.frames.push(Frame { locals, global_decls: Vec::new() });
        self.enter()?;
        // The caller's line is restored on the way out so a later error in the
        // caller does not report the callee's last line.
        let saved_line = self.line;
        let r = self.exec_block(body, host);
        self.leave();
        self.frames.pop();
        self.line = saved_line;

        match r? {
            Flow::Return(v) => Ok(v),
            // Falling off the end of a function returns None, as in Python. A
            // stray break/continue cannot reach here: the parser only produces
            // them inside a statement list, and any enclosing loop catches them
            // before the function body finishes.
            _ => Ok(Value::None),
        }
    }

    fn fault(&self, f: Fault) -> Trap {
        match f {
            Fault::Abort => Trap::Abort,
            Fault::Msg(m) => Trap::Err(Error::new(self.line, m)),
        }
    }

    // -------------------------------------------------------------------- indexing

    fn load_index(&self, target: &Value, index: &Value) -> Eval {
        let idx = match index {
            Value::Int(i) => *i,
            Value::Bool(b) => *b as i32,
            other => {
                return self.err(alloc::format!("index must be an int, got {}", other.type_name()));
            }
        };
        match target {
            Value::List(l) => {
                let items = l.borrow();
                match resolve_index(idx, items.len()) {
                    Some(i) => Ok(items[i].clone()),
                    None => self.err("list index out of range"),
                }
            }
            Value::Str(s) => {
                let n = s.chars().count();
                match resolve_index(idx, n) {
                    Some(i) => {
                        let c = s.chars().nth(i).expect("index checked against the char count");
                        let mut buf = [0u8; 4];
                        Ok(Value::str(&*c.encode_utf8(&mut buf)))
                    }
                    None => self.err("string index out of range"),
                }
            }
            other => self.err(alloc::format!("{} is not indexable", other.type_name())),
        }
    }

    fn store_index(&self, target: &Value, index: &Value, v: Value) -> Exec {
        let idx = match index {
            Value::Int(i) => *i,
            Value::Bool(b) => *b as i32,
            other => {
                return self.err(alloc::format!("index must be an int, got {}", other.type_name()));
            }
        };
        match target {
            Value::List(l) => {
                let mut items = l.borrow_mut();
                match resolve_index(idx, items.len()) {
                    Some(i) => {
                        items[i] = v;
                        Ok(Flow::Normal)
                    }
                    None => self.err("list index out of range"),
                }
            }
            Value::Str(_) => self.err("strings cannot be modified in place"),
            other => self.err(alloc::format!("{} does not support item assignment", other.type_name())),
        }
    }

    // ------------------------------------------------------------------ operators

    fn compare(&self, op: CmpOp, a: &Value, b: &Value) -> Eval {
        use core::cmp::Ordering::*;
        let r = match op {
            CmpOp::Eq => a.eq(b),
            CmpOp::Ne => !a.eq(b),
            CmpOp::In | CmpOp::NotIn => {
                let found = b.contains(a).ok_or_else(|| {
                    Trap::Err(Error::new(
                        self.line,
                        alloc::format!("'in' does not apply to {}", b.type_name()),
                    ))
                })?;
                if op == CmpOp::In { found } else { !found }
            }
            _ => {
                let ord = a.cmp(b).ok_or_else(|| {
                    Trap::Err(Error::new(
                        self.line,
                        alloc::format!("cannot order {} against {}", a.type_name(), b.type_name()),
                    ))
                })?;
                match op {
                    CmpOp::Lt => ord == Less,
                    CmpOp::Gt => ord == Greater,
                    CmpOp::Le => ord != Greater,
                    CmpOp::Ge => ord != Less,
                    _ => unreachable!("handled above"),
                }
            }
        };
        Ok(Value::Bool(r))
    }

    fn binop(&self, op: BinOp, a: &Value, b: &Value) -> Eval {
        // Bools act as 0/1 in arithmetic, as in Python.
        let as_int = |v: &Value| match v {
            Value::Int(i) => Some(*i),
            Value::Bool(x) => Some(*x as i32),
            _ => None,
        };

        // Sequence operations first: `+` concatenates, `*` repeats.
        match (op, a, b) {
            (BinOp::Add, Value::Str(x), Value::Str(y)) => {
                // `s = s + s` in a loop doubles; twenty iterations is a
                // megabyte against a 256 KiB heap.
                let len = x.len().saturating_add(y.len());
                if len > crate::value::MAX_STR_LEN {
                    return self.err("string would be too long");
                }
                let mut s = String::with_capacity(len);
                s.push_str(x);
                s.push_str(y);
                return Ok(Value::str(s));
            }
            (BinOp::Add, Value::List(x), Value::List(y)) => {
                let mut out = x.borrow().clone();
                // Clone before extending in case x and y are the same list.
                let tail = y.borrow().clone();
                if out.len() + tail.len() > builtins::MAX_LIST_LEN {
                    return self.err("list would exceed the element limit");
                }
                out.extend(tail);
                return Ok(Value::list(out));
            }
            (BinOp::Mul, Value::Str(s), _) | (BinOp::Mul, _, Value::Str(s))
                if repeat_count(a, b).is_some() =>
            {
                let n = repeat_count(a, b).unwrap();
                // Check the count as well as the product. `'' * 2147483647` has
                // a product of zero, so a size-only check waves it through and
                // the repeat then runs two billion times.
                if n > crate::value::MAX_STR_LEN || s.len().saturating_mul(n) > crate::value::MAX_STR_LEN {
                    return self.err("string would be too long");
                }
                return Ok(Value::str(s.repeat(n)));
            }
            (BinOp::Mul, Value::List(l), _) | (BinOp::Mul, _, Value::List(l))
                if repeat_count(a, b).is_some() =>
            {
                let n = repeat_count(a, b).unwrap();
                let items = l.borrow().clone();
                // Same trap: `[] * 2147483647` multiplies out to zero elements
                // but would still spin the loop below 2^31 times, and nothing in
                // that loop calls `tick`, so the badge would be unrecoverable.
                if n > builtins::MAX_LIST_LEN || items.len().saturating_mul(n) > builtins::MAX_LIST_LEN {
                    return self.err("list would exceed the element limit");
                }
                let mut out = Vec::with_capacity(items.len() * n);
                for _ in 0..n {
                    out.extend(items.iter().cloned());
                }
                return Ok(Value::list(out));
            }
            _ => (),
        }

        let (Some(x), Some(y)) = (as_int(a), as_int(b)) else {
            return self.err(alloc::format!(
                "cannot apply '{}' to {} and {}",
                binop_symbol(op),
                a.type_name(),
                b.type_name()
            ));
        };

        let v = match op {
            BinOp::Add => x.wrapping_add(y),
            BinOp::Sub => x.wrapping_sub(y),
            BinOp::Mul => x.wrapping_mul(y),
            BinOp::Div | BinOp::FloorDiv => {
                if y == 0 {
                    return self.err("division by zero");
                }
                floor_div(x, y)
            }
            BinOp::Mod => {
                if y == 0 {
                    return self.err("modulo by zero");
                }
                floor_mod(x, y)
            }
            BinOp::Pow => {
                if y < 0 {
                    // A negative power is a fraction, and there are no fractions
                    // here. Better to say so than to hand back 0.
                    return self.err("a negative exponent needs floats, which are not supported");
                }
                x.wrapping_pow(y as u32)
            }
            BinOp::BitAnd => x & y,
            BinOp::BitOr => x | y,
            BinOp::BitXor => x ^ y,
            BinOp::Shl => {
                if y < 0 {
                    return self.err("negative shift count");
                }
                // Rust's `<<` panics past the word size in debug and is UB-ish
                // in spirit; Python just gives up all the bits.
                if y >= 32 { 0 } else { x.wrapping_shl(y as u32) }
            }
            BinOp::Shr => {
                if y < 0 {
                    return self.err("negative shift count");
                }
                // Arithmetic shift, so a negative number keeps its sign all the
                // way down to -1 -- which is what Python's infinite-precision
                // right shift converges to as well.
                if y >= 32 { if x < 0 { -1 } else { 0 } } else { x.wrapping_shr(y as u32) }
            }
        };
        Ok(Value::Int(v))
    }
}

/// The repeat count of `seq * n` or `n * seq`, whichever way round it is
/// written. Python accepts both, and only rejecting one is the kind of gap that
/// makes a language feel unfinished.
fn repeat_count(a: &Value, b: &Value) -> Option<usize> {
    let n = match (a, b) {
        (Value::Str(_) | Value::List(_), Value::Int(n)) => *n,
        (Value::Str(_) | Value::List(_), Value::Bool(n)) => *n as i32,
        (Value::Int(n), Value::Str(_) | Value::List(_)) => *n,
        (Value::Bool(n), Value::Str(_) | Value::List(_)) => *n as i32,
        _ => return None,
    };
    Some(n.max(0) as usize)
}

/// Division that rounds towards negative infinity, as Python's does.
/// Rust's `/` truncates towards zero, so `-7 / 2` differs: -4 here, -3 there.
///
/// Every operation here is a wrapping one, including the remainder used to
/// decide whether to round down. `i32::MIN % -1` is an arithmetic overflow --
/// the quotient does not fit -- and a plain `%` panics on it in any build with
/// overflow checks on.
fn floor_div(x: i32, y: i32) -> i32 {
    let q = x.wrapping_div(y);
    let r = x.wrapping_rem(y);
    if r != 0 && ((x < 0) != (y < 0)) { q.wrapping_sub(1) } else { q }
}

/// The remainder that pairs with [`floor_div`]: its sign follows the divisor,
/// which is what makes `-1 % 8 == 7` and therefore what makes wrapping a
/// coordinate around the screen work without a special case.
fn floor_mod(x: i32, y: i32) -> i32 {
    let r = x.wrapping_rem(y);
    if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r }
}

fn binop_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::FloorDiv => "//",
        BinOp::Mod => "%",
        BinOp::Pow => "**",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_division_matches_python() {
        assert_eq!(floor_div(7, 2), 3);
        assert_eq!(floor_div(-7, 2), -4);
        assert_eq!(floor_div(7, -2), -4);
        assert_eq!(floor_div(-7, -2), 3);
    }

    #[test]
    fn modulo_sign_follows_the_divisor() {
        assert_eq!(floor_mod(-1, 8), 7);
        assert_eq!(floor_mod(7, 3), 1);
        assert_eq!(floor_mod(-7, 3), 2);
        assert_eq!(floor_mod(7, -3), -2);
    }

    #[test]
    fn division_identity_holds() {
        for x in [-9i32, -1, 0, 5, 13] {
            for y in [-4i32, -1, 2, 7] {
                assert_eq!(floor_div(x, y) * y + floor_mod(x, y), x, "x={} y={}", x, y);
            }
        }
    }
}
