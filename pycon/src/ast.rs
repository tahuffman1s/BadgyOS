//! The arena the parser fills and the interpreter walks.
//!
//! Every node type lives in its own flat `Vec` and refers to its children by
//! index. Two things fall out of that, both of which matter on a badge:
//!
//! * a program costs a handful of allocations rather than one per node, and
//! * dropping it is a few `Vec` frees. A `Box`-linked tree would drop recursively, and `a = [[[[...]]]]`
//!   nested a few hundred deep would blow the 16 KiB stack *while freeing memory* -- a crash with no stack
//!   trace and no obvious cause.
//!
//! Names and string literals are interned so the interpreter can compare and
//! look them up by `u32` instead of by string.

use alloc::string::String;
use alloc::vec::Vec;

pub type ExprId = u32;
pub type StmtId = u32;
pub type NameId = u32;
pub type StrId = u32;
pub type FuncId = u32;

/// A contiguous run inside one of the side tables (`expr_list`, `stmt_list`,
/// `name_list`, `arms`). Cheaper than a `Vec` per node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Slice {
    pub start: u32,
    pub len: u32,
}

impl Slice {
    pub fn range(self) -> core::ops::Range<usize> { self.start as usize..(self.start + self.len) as usize }
}

/// A statement block: a run of statement ids in `stmt_list`.
pub type Block = Slice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    In,
    NotIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Pos,
    Invert,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    None,
    Bool(bool),
    Int(i32),
    Str(StrId),
    Name(NameId),
    /// `[a, b, c]`
    List(Slice),
    Unary(UnOp, ExprId),
    Bin(BinOp, ExprId, ExprId),
    Cmp(CmpOp, ExprId, ExprId),
    /// Short-circuiting; kept separate from `Bin` because they do not evaluate
    /// both sides.
    And(ExprId, ExprId),
    Or(ExprId, ExprId),
    /// `f(args)` where `f` is a bare name -- the only callable form, since
    /// there are no first-class functions to speak of.
    Call(NameId, Slice),
    /// `obj.method(args)`
    Method(ExprId, NameId, Slice),
    /// `obj[index]`
    Index(ExprId, ExprId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Expr(ExprId),
    Assign { name: NameId, value: ExprId },
    SetIndex { target: ExprId, index: ExprId, value: ExprId },
    AugAssign { name: NameId, op: BinOp, value: ExprId },
    AugSetIndex { target: ExprId, index: ExprId, op: BinOp, value: ExprId },
    If { arms: Slice, orelse: Option<Block> },
    While { cond: ExprId, body: Block },
    For { var: NameId, iter: ExprId, body: Block },
    Def(FuncId),
    Return(Option<ExprId>),
    Break,
    Continue,
    Pass,
    Global(Slice),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Func {
    pub name: NameId,
    /// Parameter names, a run in `name_list`.
    pub params: Slice,
    pub body: Block,
}

#[derive(Debug, Default)]
pub struct Ast {
    pub exprs: Vec<Expr>,
    pub stmts: Vec<Stmt>,
    pub funcs: Vec<Func>,

    /// Side tables the `Slice`s point into.
    pub expr_list: Vec<ExprId>,
    pub stmt_list: Vec<StmtId>,
    pub name_list: Vec<NameId>,
    pub arms: Vec<(ExprId, Block)>,

    /// Interned identifiers and string literals.
    pub names: Vec<String>,
    pub strs: Vec<String>,

    /// Line number per statement, parallel to `stmts`. Used for runtime errors.
    pub stmt_lines: Vec<u32>,

    /// The program body.
    pub top: Block,
}

impl Ast {
    pub fn name(&self, id: NameId) -> &str { &self.names[id as usize] }

    pub fn string(&self, id: StrId) -> &str { &self.strs[id as usize] }

    pub fn expr(&self, id: ExprId) -> &Expr { &self.exprs[id as usize] }

    pub fn stmt(&self, id: StmtId) -> &Stmt { &self.stmts[id as usize] }

    pub fn stmt_line(&self, id: StmtId) -> u32 { self.stmt_lines[id as usize] }

    pub fn block(&self, b: Block) -> &[StmtId] { &self.stmt_list[b.range()] }

    pub fn exprs_of(&self, s: Slice) -> &[ExprId] { &self.expr_list[s.range()] }

    pub fn names_of(&self, s: Slice) -> &[NameId] { &self.name_list[s.range()] }

    pub fn arms_of(&self, s: Slice) -> &[(ExprId, Block)] { &self.arms[s.range()] }

    /// How many distinct identifiers the program mentions. The interpreter
    /// sizes its globals table from this, which is what makes a global lookup
    /// an array index instead of a search.
    pub fn name_count(&self) -> usize { self.names.len() }

    pub(crate) fn push_expr(&mut self, e: Expr) -> ExprId {
        self.exprs.push(e);
        (self.exprs.len() - 1) as ExprId
    }

    pub(crate) fn push_stmt(&mut self, s: Stmt, line: u32) -> StmtId {
        self.stmts.push(s);
        self.stmt_lines.push(line);
        (self.stmts.len() - 1) as StmtId
    }

    /// Intern an identifier, returning its id.
    pub(crate) fn intern(&mut self, s: &str) -> NameId {
        if let Some(i) = self.names.iter().position(|n| n == s) {
            return i as NameId;
        }
        self.names.push(String::from(s));
        (self.names.len() - 1) as NameId
    }

    pub(crate) fn push_str(&mut self, s: String) -> StrId {
        self.strs.push(s);
        (self.strs.len() - 1) as StrId
    }
}
