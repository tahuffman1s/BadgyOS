//! Recursive-descent parser: tokens -> [`Ast`].
//!
//! Precedence is expressed the usual way, as one function per level, from
//! [`Parser::or_test`] down to [`Parser::atom`]. The chain matches Python's:
//!
//! ```text
//!   or  and  not  comparison  |  ^  &  << >>  + -  * / // %  unary  **  call/index
//! ```
//!
//! Both the expression levels and block nesting run through [`Parser::deep`],
//! which enforces [`MAX_PARSE_DEPTH`]. The badge stack is small and a file
//! dropped onto a USB drive is untrusted input, so "how deep can this recurse"
//! has to have an answer that is not "until it crashes".

use alloc::string::String;
use alloc::vec::Vec;

use crate::Error;
use crate::ast::*;
use crate::lexer::{AugOp, Spanned, Tok};

/// Maximum nesting of expressions and blocks.
///
/// Each level is worth a few hundred bytes of parser frame and a similar amount
/// of evaluator frame later, so 32 keeps the worst case comfortably inside a
/// 16 KiB stack while being far more nesting than any real script uses.
pub const MAX_PARSE_DEPTH: u32 = 32;

pub fn parse(tokens: &[Spanned]) -> Result<Ast, Error> {
    let mut p = Parser { toks: tokens, pos: 0, ast: Ast::default(), depth: 0 };
    let top = p.parse_block_body(&[Tok::Eof])?;
    p.ast.top = top;
    Ok(p.ast)
}

struct Parser<'a> {
    toks: &'a [Spanned],
    pos: usize,
    ast: Ast,
    depth: u32,
}

impl<'a> Parser<'a> {
    // ------------------------------------------------------------- token access

    fn peek(&self) -> &Tok { &self.toks[self.pos.min(self.toks.len() - 1)].tok }

    fn line(&self) -> u32 { self.toks[self.pos.min(self.toks.len() - 1)].line }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<(), Error> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err(alloc::format!("expected {}, found {}", what, describe(self.peek()))))
        }
    }

    fn err(&self, msg: impl Into<String>) -> Error { Error::new(self.line(), msg) }

    /// Run `f` one level deeper, refusing to go past [`MAX_PARSE_DEPTH`].
    fn deep<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(self.err("nested too deeply"));
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    // ---------------------------------------------------------------- statements

    /// Parse statements until one of `terminators` is next, and return them as
    /// a block. Does not consume the terminator.
    fn parse_block_body(&mut self, terminators: &[Tok]) -> Result<Block, Error> {
        let mut ids: Vec<StmtId> = Vec::new();
        loop {
            while self.eat(&Tok::Newline) {}
            if terminators.contains(self.peek()) {
                break;
            }
            if *self.peek() == Tok::Eof {
                break;
            }
            let before = self.pos;
            self.statement(&mut ids)?;
            debug_assert!(self.pos > before, "parser made no progress");
        }
        Ok(self.commit_block(ids))
    }

    fn commit_block(&mut self, ids: Vec<StmtId>) -> Block {
        let start = self.ast.stmt_list.len() as u32;
        self.ast.stmt_list.extend_from_slice(&ids);
        Block { start, len: ids.len() as u32 }
    }

    /// A statement, appended to `out`. Compound statements append exactly one
    /// id; so do simple ones.
    fn statement(&mut self, out: &mut Vec<StmtId>) -> Result<(), Error> {
        let line = self.line();
        match self.peek().clone() {
            Tok::If => {
                self.bump();
                let id = self.if_stmt(line)?;
                out.push(id);
            }
            Tok::While => {
                self.bump();
                let cond = self.expr()?;
                let body = self.suite()?;
                out.push(self.ast.push_stmt(Stmt::While { cond, body }, line));
            }
            Tok::For => {
                self.bump();
                let var = match self.bump() {
                    Tok::Name(n) => self.ast.intern(&n),
                    other => {
                        return Err(Error::new(
                            line,
                            alloc::format!("expected a loop variable, found {}", describe(&other)),
                        ));
                    }
                };
                self.expect(&Tok::In, "'in'")?;
                let iter = self.expr()?;
                let body = self.suite()?;
                out.push(self.ast.push_stmt(Stmt::For { var, iter, body }, line));
            }
            Tok::Def => {
                self.bump();
                let id = self.func_def(line)?;
                out.push(id);
            }
            _ => {
                let id = self.simple_stmt()?;
                out.push(id);
                // A one-line suite (`if x: y = 1`) has already consumed its
                // newline by the time we get back here, so accept either.
                if !self.eat(&Tok::Newline) && !matches!(self.peek(), Tok::Eof | Tok::Dedent) {
                    return Err(
                        self.err(alloc::format!("unexpected {} after statement", describe(self.peek())))
                    );
                }
            }
        }
        Ok(())
    }

    fn if_stmt(&mut self, line: u32) -> Result<StmtId, Error> {
        // Collect (condition, block) pairs for `if` and every `elif`, then the
        // optional `else`. Storing them as one run keeps `Stmt` copyable.
        let mut arms: Vec<(ExprId, Block)> = Vec::new();
        let cond = self.expr()?;
        let body = self.suite()?;
        arms.push((cond, body));

        let mut orelse = None;
        loop {
            // `elif`/`else` belong to this `if` only if they are at the same
            // indentation, which the lexer has already expressed by *not*
            // emitting a Dedent before them.
            if self.eat(&Tok::Elif) {
                let c = self.expr()?;
                let b = self.suite()?;
                arms.push((c, b));
                continue;
            }
            if self.eat(&Tok::Else) {
                orelse = Some(self.suite()?);
            }
            break;
        }

        let start = self.ast.arms.len() as u32;
        let len = arms.len() as u32;
        self.ast.arms.extend(arms);
        Ok(self.ast.push_stmt(Stmt::If { arms: Slice { start, len }, orelse }, line))
    }

    fn func_def(&mut self, line: u32) -> Result<StmtId, Error> {
        let name = match self.bump() {
            Tok::Name(n) => self.ast.intern(&n),
            other => {
                return Err(Error::new(
                    line,
                    alloc::format!("expected a function name, found {}", describe(&other)),
                ));
            }
        };
        self.expect(&Tok::LParen, "'('")?;
        let mut params: Vec<NameId> = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                match self.bump() {
                    Tok::Name(n) => {
                        let id = self.ast.intern(&n);
                        if params.contains(&id) {
                            return Err(Error::new(line, "duplicate parameter name"));
                        }
                        params.push(id);
                    }
                    other => {
                        return Err(Error::new(
                            line,
                            alloc::format!("expected a parameter name, found {}", describe(&other)),
                        ));
                    }
                }
                if self.eat(&Tok::Comma) {
                    // trailing comma before ')' is fine
                    if self.eat(&Tok::RParen) {
                        break;
                    }
                    continue;
                }
                self.expect(&Tok::RParen, "')'")?;
                break;
            }
        }
        let body = self.suite()?;

        let pstart = self.ast.name_list.len() as u32;
        let plen = params.len() as u32;
        self.ast.name_list.extend(params);
        self.ast.funcs.push(Func { name, params: Slice { start: pstart, len: plen }, body });
        let fid = (self.ast.funcs.len() - 1) as FuncId;
        Ok(self.ast.push_stmt(Stmt::Def(fid), line))
    }

    /// `':' NEWLINE INDENT statements DEDENT`, or the one-line form
    /// `':' simple_stmt NEWLINE`.
    fn suite(&mut self) -> Result<Block, Error> {
        self.expect(&Tok::Colon, "':'")?;
        if self.eat(&Tok::Newline) {
            self.expect(&Tok::Indent, "an indented block")?;
            let b = self.deep(|p| p.parse_block_body(&[Tok::Dedent]))?;
            self.expect(&Tok::Dedent, "the end of the block")?;
            if b.len == 0 {
                return Err(self.err("empty block"));
            }
            Ok(b)
        } else {
            let line = self.line();
            let id = self.simple_stmt()?;
            if !self.eat(&Tok::Newline) && !matches!(self.peek(), Tok::Eof | Tok::Dedent) {
                return Err(Error::new(line, "expected end of line after a one-line block"));
            }
            Ok(self.commit_block(alloc::vec![id]))
        }
    }

    fn simple_stmt(&mut self) -> Result<StmtId, Error> {
        let line = self.line();
        match self.peek().clone() {
            Tok::Pass => {
                self.bump();
                Ok(self.ast.push_stmt(Stmt::Pass, line))
            }
            Tok::Break => {
                self.bump();
                Ok(self.ast.push_stmt(Stmt::Break, line))
            }
            Tok::Continue => {
                self.bump();
                Ok(self.ast.push_stmt(Stmt::Continue, line))
            }
            Tok::Return => {
                self.bump();
                let v = if matches!(self.peek(), Tok::Newline | Tok::Eof | Tok::Dedent) {
                    None
                } else {
                    Some(self.expr()?)
                };
                Ok(self.ast.push_stmt(Stmt::Return(v), line))
            }
            Tok::Global => {
                self.bump();
                let mut names = Vec::new();
                loop {
                    match self.bump() {
                        Tok::Name(n) => names.push(self.ast.intern(&n)),
                        other => {
                            return Err(Error::new(
                                line,
                                alloc::format!("expected a name after 'global', found {}", describe(&other)),
                            ));
                        }
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let start = self.ast.name_list.len() as u32;
                let len = names.len() as u32;
                self.ast.name_list.extend(names);
                Ok(self.ast.push_stmt(Stmt::Global(Slice { start, len }), line))
            }
            _ => self.expr_or_assign(line),
        }
    }

    /// An expression statement, or an assignment whose target is that
    /// expression. Python's grammar does the same thing: parse the left side as
    /// an expression first, then decide.
    fn expr_or_assign(&mut self, line: u32) -> Result<StmtId, Error> {
        let lhs = self.expr()?;

        if self.eat(&Tok::Assign) {
            let value = self.expr()?;
            return match self.ast.expr(lhs).clone() {
                Expr::Name(name) => Ok(self.ast.push_stmt(Stmt::Assign { name, value }, line)),
                Expr::Index(target, index) => {
                    Ok(self.ast.push_stmt(Stmt::SetIndex { target, index, value }, line))
                }
                _ => Err(Error::new(line, "cannot assign to this expression")),
            };
        }

        if let Tok::AugAssign(aug) = self.peek().clone() {
            self.bump();
            let value = self.expr()?;
            let op = aug_to_bin(aug);
            return match self.ast.expr(lhs).clone() {
                Expr::Name(name) => Ok(self.ast.push_stmt(Stmt::AugAssign { name, op, value }, line)),
                Expr::Index(target, index) => {
                    Ok(self.ast.push_stmt(Stmt::AugSetIndex { target, index, op, value }, line))
                }
                _ => Err(Error::new(line, "cannot assign to this expression")),
            };
        }

        Ok(self.ast.push_stmt(Stmt::Expr(lhs), line))
    }

    // --------------------------------------------------------------- expressions

    fn expr(&mut self) -> Result<ExprId, Error> { self.deep(|p| p.or_test()) }

    fn or_test(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.and_test()?;
        while self.eat(&Tok::Or) {
            let rhs = self.and_test()?;
            lhs = self.ast.push_expr(Expr::Or(lhs, rhs));
        }
        Ok(lhs)
    }

    fn and_test(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.not_test()?;
        while self.eat(&Tok::And) {
            let rhs = self.not_test()?;
            lhs = self.ast.push_expr(Expr::And(lhs, rhs));
        }
        Ok(lhs)
    }

    fn not_test(&mut self) -> Result<ExprId, Error> {
        if self.eat(&Tok::Not) {
            let e = self.deep(|p| p.not_test())?;
            return Ok(self.ast.push_expr(Expr::Unary(UnOp::Not, e)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<ExprId, Error> {
        let first = self.bit_or()?;
        let Some(op) = self.cmp_op() else {
            return Ok(first);
        };
        let mut left = first;
        let mut rhs = self.bit_or()?;
        let mut acc = self.ast.push_expr(Expr::Cmp(op, left, rhs));

        // Chained comparisons (`0 <= x < 10`) desugar to `and`, which means the
        // middle operand is evaluated once per comparison it takes part in.
        // Python evaluates it once. The difference is only observable when that
        // operand calls a function with side effects, which is a trade worth
        // making to keep the evaluator free of temporaries.
        while let Some(op2) = self.cmp_op() {
            left = rhs;
            rhs = self.bit_or()?;
            let next = self.ast.push_expr(Expr::Cmp(op2, left, rhs));
            acc = self.ast.push_expr(Expr::And(acc, next));
        }
        Ok(acc)
    }

    fn cmp_op(&mut self) -> Option<CmpOp> {
        let op = match self.peek() {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Gt => CmpOp::Gt,
            Tok::Le => CmpOp::Le,
            Tok::Ge => CmpOp::Ge,
            Tok::In => CmpOp::In,
            Tok::Not => {
                // `not in` -- only a comparison operator when the `in` follows.
                if matches!(self.toks.get(self.pos + 1).map(|s| &s.tok), Some(Tok::In)) {
                    self.bump();
                    self.bump();
                    return Some(CmpOp::NotIn);
                }
                return None;
            }
            _ => return None,
        };
        self.bump();
        Some(op)
    }

    fn bit_or(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.bit_xor()?;
        while self.eat(&Tok::Pipe) {
            let rhs = self.bit_xor()?;
            lhs = self.ast.push_expr(Expr::Bin(BinOp::BitOr, lhs, rhs));
        }
        Ok(lhs)
    }

    fn bit_xor(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.bit_and()?;
        while self.eat(&Tok::Caret) {
            let rhs = self.bit_and()?;
            lhs = self.ast.push_expr(Expr::Bin(BinOp::BitXor, lhs, rhs));
        }
        Ok(lhs)
    }

    fn bit_and(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.shift()?;
        while self.eat(&Tok::Amp) {
            let rhs = self.shift()?;
            lhs = self.ast.push_expr(Expr::Bin(BinOp::BitAnd, lhs, rhs));
        }
        Ok(lhs)
    }

    fn shift(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.arith()?;
        loop {
            let op = if self.eat(&Tok::Shl) {
                BinOp::Shl
            } else if self.eat(&Tok::Shr) {
                BinOp::Shr
            } else {
                break;
            };
            let rhs = self.arith()?;
            lhs = self.ast.push_expr(Expr::Bin(op, lhs, rhs));
        }
        Ok(lhs)
    }

    fn arith(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.term()?;
        loop {
            let op = if self.eat(&Tok::Plus) {
                BinOp::Add
            } else if self.eat(&Tok::Minus) {
                BinOp::Sub
            } else {
                break;
            };
            let rhs = self.term()?;
            lhs = self.ast.push_expr(Expr::Bin(op, lhs, rhs));
        }
        Ok(lhs)
    }

    fn term(&mut self) -> Result<ExprId, Error> {
        let mut lhs = self.factor()?;
        loop {
            let op = if self.eat(&Tok::Star) {
                BinOp::Mul
            } else if self.eat(&Tok::DblSlash) {
                BinOp::FloorDiv
            } else if self.eat(&Tok::Slash) {
                // There are no floats, so `/` is floor division. Scripts that
                // meant `//` get the same answer; scripts that wanted a float
                // never had one available.
                BinOp::Div
            } else if self.eat(&Tok::Percent) {
                BinOp::Mod
            } else {
                break;
            };
            let rhs = self.factor()?;
            lhs = self.ast.push_expr(Expr::Bin(op, lhs, rhs));
        }
        Ok(lhs)
    }

    fn factor(&mut self) -> Result<ExprId, Error> {
        let op = if self.eat(&Tok::Minus) {
            UnOp::Neg
        } else if self.eat(&Tok::Plus) {
            UnOp::Pos
        } else if self.eat(&Tok::Tilde) {
            UnOp::Invert
        } else {
            return self.power();
        };
        let e = self.deep(|p| p.factor())?;
        Ok(self.ast.push_expr(Expr::Unary(op, e)))
    }

    fn power(&mut self) -> Result<ExprId, Error> {
        let base = self.atom_expr()?;
        if self.eat(&Tok::DblStar) {
            // Right-associative, and its right operand may be unary -- `2**-1`
            // parses, even though it then fails at runtime as integer division.
            let exp = self.deep(|p| p.factor())?;
            return Ok(self.ast.push_expr(Expr::Bin(BinOp::Pow, base, exp)));
        }
        Ok(base)
    }

    /// An atom followed by any number of call / index / method trailers.
    fn atom_expr(&mut self) -> Result<ExprId, Error> {
        let (mut e, mut bare_name) = self.atom()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    let Some(name) = bare_name else {
                        return Err(self.err("only a plain function name can be called"));
                    };
                    self.bump();
                    let args = self.call_args()?;
                    e = self.ast.push_expr(Expr::Call(name, args));
                    bare_name = None;
                }
                Tok::LBracket => {
                    self.bump();
                    let idx = self.expr()?;
                    self.expect(&Tok::RBracket, "']'")?;
                    e = self.ast.push_expr(Expr::Index(e, idx));
                    bare_name = None;
                }
                Tok::Dot => {
                    self.bump();
                    let name = match self.bump() {
                        Tok::Name(n) => self.ast.intern(&n),
                        other => {
                            return Err(self
                                .err(alloc::format!("expected a method name, found {}", describe(&other))));
                        }
                    };
                    // Attributes exist only as method calls; there are no data
                    // attributes to read, so requiring the '(' here turns a
                    // typo into a syntax error rather than a runtime surprise.
                    self.expect(&Tok::LParen, "'(' -- attributes can only be called")?;
                    let args = self.call_args()?;
                    e = self.ast.push_expr(Expr::Method(e, name, args));
                    bare_name = None;
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// The argument list of a call, with the '(' already consumed.
    fn call_args(&mut self) -> Result<Slice, Error> {
        let mut args: Vec<ExprId> = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.expr()?);
                if self.eat(&Tok::Comma) {
                    if self.eat(&Tok::RParen) {
                        break;
                    }
                    continue;
                }
                self.expect(&Tok::RParen, "')'")?;
                break;
            }
        }
        let start = self.ast.expr_list.len() as u32;
        let len = args.len() as u32;
        self.ast.expr_list.extend(args);
        Ok(Slice { start, len })
    }

    /// Returns the expression and, when it is a bare identifier, its name id --
    /// which is what lets `atom_expr` tell `f(1)` from `(f)(1)`.
    fn atom(&mut self) -> Result<(ExprId, Option<NameId>), Error> {
        let line = self.line();
        match self.bump() {
            Tok::Int(v) => Ok((self.ast.push_expr(Expr::Int(v)), None)),
            Tok::Str(s) => {
                let id = self.ast.push_str(s);
                Ok((self.ast.push_expr(Expr::Str(id)), None))
            }
            Tok::True => Ok((self.ast.push_expr(Expr::Bool(true)), None)),
            Tok::False => Ok((self.ast.push_expr(Expr::Bool(false)), None)),
            Tok::None => Ok((self.ast.push_expr(Expr::None), None)),
            Tok::Name(n) => {
                let id = self.ast.intern(&n);
                Ok((self.ast.push_expr(Expr::Name(id)), Some(id)))
            }
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok((e, None))
            }
            Tok::LBracket => {
                let mut items: Vec<ExprId> = Vec::new();
                if !self.eat(&Tok::RBracket) {
                    loop {
                        items.push(self.expr()?);
                        if self.eat(&Tok::Comma) {
                            if self.eat(&Tok::RBracket) {
                                break;
                            }
                            continue;
                        }
                        self.expect(&Tok::RBracket, "']'")?;
                        break;
                    }
                }
                let start = self.ast.expr_list.len() as u32;
                let len = items.len() as u32;
                self.ast.expr_list.extend(items);
                Ok((self.ast.push_expr(Expr::List(Slice { start, len })), None))
            }
            other => Err(Error::new(line, alloc::format!("unexpected {} in expression", describe(&other)))),
        }
    }
}

fn aug_to_bin(a: AugOp) -> BinOp {
    match a {
        AugOp::Add => BinOp::Add,
        AugOp::Sub => BinOp::Sub,
        AugOp::Mul => BinOp::Mul,
        AugOp::Div => BinOp::Div,
        AugOp::FloorDiv => BinOp::FloorDiv,
        AugOp::Mod => BinOp::Mod,
        AugOp::And => BinOp::BitAnd,
        AugOp::Or => BinOp::BitOr,
        AugOp::Xor => BinOp::BitXor,
        AugOp::Shl => BinOp::Shl,
        AugOp::Shr => BinOp::Shr,
    }
}

/// A human-readable name for a token, for error messages.
fn describe(t: &Tok) -> String {
    let s: &str = match t {
        Tok::Int(_) => "a number",
        Tok::Str(_) => "a string",
        Tok::Name(n) => return alloc::format!("'{}'", n),
        Tok::Newline => "end of line",
        Tok::Indent => "an indented block",
        Tok::Dedent => "the end of a block",
        Tok::Eof => "end of file",
        Tok::If => "'if'",
        Tok::Elif => "'elif'",
        Tok::Else => "'else'",
        Tok::While => "'while'",
        Tok::For => "'for'",
        Tok::In => "'in'",
        Tok::Def => "'def'",
        Tok::Return => "'return'",
        Tok::Break => "'break'",
        Tok::Continue => "'continue'",
        Tok::Pass => "'pass'",
        Tok::Global => "'global'",
        Tok::And => "'and'",
        Tok::Or => "'or'",
        Tok::Not => "'not'",
        Tok::True => "'True'",
        Tok::False => "'False'",
        Tok::None => "'None'",
        Tok::LParen => "'('",
        Tok::RParen => "')'",
        Tok::LBracket => "'['",
        Tok::RBracket => "']'",
        Tok::Comma => "','",
        Tok::Colon => "':'",
        Tok::Dot => "'.'",
        Tok::Plus => "'+'",
        Tok::Minus => "'-'",
        Tok::Star => "'*'",
        Tok::Slash => "'/'",
        Tok::DblSlash => "'//'",
        Tok::Percent => "'%'",
        Tok::DblStar => "'**'",
        Tok::Amp => "'&'",
        Tok::Pipe => "'|'",
        Tok::Caret => "'^'",
        Tok::Tilde => "'~'",
        Tok::Shl => "'<<'",
        Tok::Shr => "'>>'",
        Tok::Eq => "'=='",
        Tok::Ne => "'!='",
        Tok::Lt => "'<'",
        Tok::Gt => "'>'",
        Tok::Le => "'<='",
        Tok::Ge => "'>='",
        Tok::Assign => "'='",
        Tok::AugAssign(_) => "an augmented assignment",
    };
    String::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn ast(src: &str) -> Ast { parse(&tokenize(src).unwrap()).unwrap() }

    fn fails(src: &str) -> Error {
        let t = match tokenize(src) {
            Ok(t) => t,
            Err(e) => return e,
        };
        parse(&t).expect_err("expected a parse error")
    }

    #[test]
    fn precedence_binds_as_python_does() {
        let a = ast("x = 1 + 2 * 3\n");
        // The top of the assignment must be the '+', with '*' underneath.
        let Stmt::Assign { value, .. } = a.stmt(0).clone() else { panic!() };
        let Expr::Bin(op, _, rhs) = a.expr(value).clone() else { panic!("{:?}", a.expr(value)) };
        assert_eq!(op, BinOp::Add);
        assert!(matches!(a.expr(rhs), Expr::Bin(BinOp::Mul, _, _)));
    }

    #[test]
    fn unary_minus_binds_looser_than_power() {
        // -2**2 is -(2**2) in Python.
        let a = ast("x = -2 ** 2\n");
        let Stmt::Assign { value, .. } = a.stmt(0).clone() else { panic!() };
        assert!(matches!(a.expr(value), Expr::Unary(UnOp::Neg, _)));
    }

    #[test]
    fn comparison_chains_desugar_to_and() {
        let a = ast("x = 0 <= n < 10\n");
        let Stmt::Assign { value, .. } = a.stmt(0).clone() else { panic!() };
        assert!(matches!(a.expr(value), Expr::And(_, _)));
    }

    #[test]
    fn not_in_is_one_operator() {
        let a = ast("x = 1 not in y\n");
        let Stmt::Assign { value, .. } = a.stmt(0).clone() else { panic!() };
        assert!(matches!(a.expr(value), Expr::Cmp(CmpOp::NotIn, _, _)));
    }

    #[test]
    fn elif_chain_becomes_arms() {
        let a = ast("if a:\n  p\nelif b:\n  q\nelif c:\n  r\nelse:\n  s\n");
        let Stmt::If { arms, orelse } = a.stmt(a.block(a.top)[0]).clone() else { panic!() };
        assert_eq!(arms.len, 3);
        assert!(orelse.is_some());
    }

    #[test]
    fn one_line_suite() {
        let a = ast("if a: b = 1\n");
        let Stmt::If { arms, .. } = a.stmt(a.block(a.top)[0]).clone() else { panic!() };
        assert_eq!(a.arms_of(arms)[0].1.len, 1);
    }

    #[test]
    fn index_assignment_targets() {
        let a = ast("a[0] = 1\n");
        assert!(matches!(a.stmt(a.block(a.top)[0]), Stmt::SetIndex { .. }));
        let b = ast("a[0] += 1\n");
        assert!(matches!(b.stmt(b.block(b.top)[0]), Stmt::AugSetIndex { .. }));
    }

    #[test]
    fn method_calls_need_parens() {
        assert!(tokenize("a.b()\n").is_ok());
        let a = ast("a.append(1)\n");
        assert!(matches!(a.stmt(a.block(a.top)[0]), Stmt::Expr(_)));
        assert!(fails("x = a.b\n").msg.contains("attributes"));
    }

    #[test]
    fn assignment_to_a_literal_is_rejected() {
        assert!(fails("1 = 2\n").msg.contains("cannot assign"));
    }

    #[test]
    fn deep_nesting_is_refused_not_crashed() {
        let mut src = String::from("x = ");
        for _ in 0..200 {
            src.push('(');
        }
        src.push('1');
        for _ in 0..200 {
            src.push(')');
        }
        src.push('\n');
        // The lexer's bracket cap catches this first; either way it must be an
        // error and must not recurse to death.
        assert!(tokenize(&src).is_err() || parse(&tokenize(&src).unwrap()).is_err());
    }

    #[test]
    fn function_definition_records_params() {
        let a = ast("def f(x, y):\n  return x + y\n");
        assert_eq!(a.funcs.len(), 1);
        assert_eq!(a.names_of(a.funcs[0].params).len(), 2);
    }

    #[test]
    fn duplicate_parameters_are_rejected() {
        assert!(fails("def f(x, x):\n  pass\n").msg.contains("duplicate"));
    }

    #[test]
    fn empty_block_is_rejected() {
        // An `if` whose body dedents immediately cannot be represented.
        assert!(fails("if a:\npass\n").msg.contains("indented"));
    }
}
