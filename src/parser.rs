//! Hand-written recursive descent. Items and statements are plain RD;
//! expressions go through a Pratt loop so each operator tier is one
//! binding-power entry. Statements end at newlines, `;`, or `}`.
//!
//! Recovery: a statement- or item-level error skips to the next newline
//! (or closing brace) and parsing continues, so one typo reports once.

use crate::ast::{
    Ast, BinOp, Effect, Expr, ExprId, FnKind, Item, ItemId, Name, Param, ScheduleDirective, Stmt,
    StmtId, UnOp,
};
use crate::lexer::{Span, Token, TokenKind};

/// Which keyword introduced a function-shaped item.
#[derive(Clone, Copy)]
enum FnFlavor {
    Fn,
    Spec,
    Impl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

pub fn parse(src: &str, tokens: &[Token]) -> (Ast, Vec<ParseError>) {
    let mut parser = Parser {
        src,
        tokens,
        pos: 0,
        prev_end: 0,
        ast: Ast::default(),
        errors: Vec::new(),
    };
    parser.parse_file();
    (parser.ast, parser.errors)
}

struct Parser<'a> {
    src: &'a str,
    tokens: &'a [Token],
    pos: usize,
    /// End offset of the last consumed token; closes spans.
    prev_end: usize,
    ast: Ast,
    errors: Vec<ParseError>,
}

/// Binding powers, loosest to tightest. Comparisons sit below the bitwise
/// tier, as in Rust (not C). Postfix (`()`, `[]`, `.`, `?`) binds tightest.
fn infix_bp(kind: TokenKind) -> Option<(u8, u8)> {
    use TokenKind::*;
    let bp = match kind {
        DotDot => (1, 2),
        EqEq | BangEq | Lt | Le | Gt | Ge => (3, 4),
        Pipe => (5, 6),
        Caret => (7, 8),
        Amp => (9, 10),
        Shl | Shr => (11, 12),
        Plus | Minus => (13, 14),
        Star | Slash | Percent => (15, 16),
        _ => return None,
    };
    Some(bp)
}

const PREFIX_BP: u8 = 17;
const POSTFIX_BP: u8 = 19;

/// Minimum binding power for type positions (`: ty`). Excludes comparison
/// and range operators so the `<` of a following effect list (`: bits[1]
/// <converges>`) is never eaten as less-than. Bracket application and
/// arithmetic (`bits[N+1]`) still parse.
const TYPE_MIN_BP: u8 = 5;

fn binop_of(kind: TokenKind) -> BinOp {
    use TokenKind::*;
    match kind {
        DotDot => BinOp::Range,
        EqEq => BinOp::Eq,
        BangEq => BinOp::Ne,
        Lt => BinOp::Lt,
        Le => BinOp::Le,
        Gt => BinOp::Gt,
        Ge => BinOp::Ge,
        Pipe => BinOp::BitOr,
        Caret => BinOp::BitXor,
        Amp => BinOp::BitAnd,
        Shl => BinOp::Shl,
        Shr => BinOp::Shr,
        Plus => BinOp::Add,
        Minus => BinOp::Sub,
        Star => BinOp::Mul,
        Slash => BinOp::Div,
        Percent => BinOp::Rem,
        _ => unreachable!("not a binary operator: {kind:?}"),
    }
}

impl<'a> Parser<'a> {
    // --- token plumbing ---

    fn peek(&self) -> Option<TokenKind> {
        self.tokens.get(self.pos).map(|t| t.kind)
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.peek() == Some(kind)
    }

    fn cur_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|t| t.span.clone())
            .unwrap_or(self.src.len()..self.src.len())
    }

    fn bump(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if let Some(tok) = &tok {
            self.prev_end = tok.span.end;
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<Span, ()> {
        if self.at(kind) {
            Ok(self.bump().unwrap().span)
        } else {
            self.error_here(format!("expected {what}"));
            Err(())
        }
    }

    fn text(&self, span: &Span) -> &'a str {
        &self.src[span.clone()]
    }

    fn skip_newlines(&mut self) {
        while self.eat(TokenKind::Newline) {}
    }

    fn error_here(&mut self, message: String) {
        self.errors.push(ParseError {
            span: self.cur_span(),
            message,
        });
    }

    /// Statement/item recovery: skip to the next newline or `}`.
    fn sync(&mut self) {
        while let Some(kind) = self.peek() {
            match kind {
                TokenKind::Newline | TokenKind::RBrace => break,
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// A statement or single-line item must end the line.
    fn expect_terminator(&mut self) {
        match self.peek() {
            Some(TokenKind::Newline) | Some(TokenKind::Semi) => {
                self.bump();
            }
            Some(TokenKind::RBrace) | None => {}
            _ => {
                self.error_here("expected end of statement".to_string());
                self.sync();
            }
        }
    }

    // --- items ---

    fn parse_file(&mut self) {
        loop {
            self.skip_newlines();
            if self.peek().is_none() {
                break;
            }
            let before = self.pos;
            if let Some(item) = self.parse_item() {
                self.ast.roots.push(item);
            }
            // Recovery must always make progress: `sync` stops before `}`,
            // which nothing consumes at top level. Never loop on one token.
            if self.pos == before {
                self.bump();
            }
        }
    }

    fn parse_item(&mut self) -> Option<ItemId> {
        use TokenKind::*;
        match self.peek() {
            Some(Module) => self.parse_module(),
            Some(Reg) => self.parse_state_decl(Reg),
            Some(Mem) => self.parse_state_decl(Mem),
            Some(Fifo) => self.parse_state_decl(Fifo),
            Some(Input) => self.parse_state_decl(Input),
            Some(Output) => self.parse_state_decl(Output),
            Some(Inst) => self.parse_state_decl(Inst),
            Some(Rule) => self.parse_rule(),
            Some(Ident) => self.parse_fn(FnFlavor::Fn),
            Some(Spec) => {
                self.bump();
                self.parse_fn(FnFlavor::Spec)
            }
            Some(Impl) => {
                self.bump();
                self.parse_fn(FnFlavor::Impl)
            }
            Some(Schedule) => self.parse_schedule(),
            _ => {
                self.error_here(
                    "expected an item (module, reg, mem, fifo, input, output, rule, schedule, or a function)"
                        .to_string(),
                );
                self.sync();
                None
            }
        }
    }

    fn parse_module(&mut self) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // module
        let name = self.expect_ident("module name")?;
        self.expect(TokenKind::LBrace, "`{` after module name")
            .ok()?;
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Some(TokenKind::RBrace) => {
                    self.bump();
                    break;
                }
                None => {
                    self.error_here("unclosed module body".to_string());
                    break;
                }
                _ => {
                    let before = self.pos;
                    if let Some(item) = self.parse_item() {
                        items.push(item);
                    }
                    if self.pos == before {
                        self.bump();
                    }
                }
            }
        }
        Some(
            self.ast
                .push_item(Item::Module { name, items }, lo..self.prev_end),
        )
    }

    /// `reg name : ty (= init)?` / `mem name : ty` / `fifo name : ty` /
    /// `input name : ty` / `output name : ty (= init)?` /
    /// `inst name : Module`
    fn parse_state_decl(&mut self, keyword: TokenKind) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // keyword
        let name = self.expect_ident("declaration name")?;
        self.expect(TokenKind::Colon, "`:` before type").ok()?;
        let ty = self.parse_expr(TYPE_MIN_BP)?;
        let item = match keyword {
            TokenKind::Reg => {
                let init = if self.eat(TokenKind::Eq) {
                    Some(self.parse_expr(0)?)
                } else {
                    None
                };
                Item::Reg { name, ty, init }
            }
            TokenKind::Mem => Item::Mem { name, ty },
            TokenKind::Fifo => Item::Fifo { name, ty },
            TokenKind::Input => Item::Input { name, ty },
            TokenKind::Output => {
                let init = if self.eat(TokenKind::Eq) {
                    Some(self.parse_expr(0)?)
                } else {
                    None
                };
                Item::Output { name, ty, init }
            }
            TokenKind::Inst => Item::Inst { name, module: ty },
            _ => unreachable!(),
        };
        self.expect_terminator();
        Some(self.ast.push_item(item, lo..self.prev_end))
    }

    /// `rule name <effects>? { body }`
    fn parse_rule(&mut self) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // rule
        let name = self.expect_ident("rule name")?;
        let effects = self.parse_effects()?;
        self.skip_newlines();
        let body = self.parse_block()?;
        Some(self.ast.push_item(
            Item::Rule {
                name,
                effects,
                body,
            },
            lo..self.prev_end,
        ))
    }

    /// `Name(params) (: ret)? <effects>? (refines Spec)? { body }`
    ///
    /// Handles `fn`, `spec`, and `impl` alike; the `spec`/`impl` keyword is
    /// already consumed. Newlines may split the signature before `refines`
    /// and before the body brace, as in DESIGN.md's RoundRobin example.
    fn parse_fn(&mut self, flavor: FnFlavor) -> Option<ItemId> {
        let lo = self.cur_span().start;
        let name = self.expect_ident("function name")?;
        self.expect(TokenKind::LParen, "`(` after function name")
            .ok()?;
        let mut params = Vec::new();
        self.skip_newlines();
        while !self.at(TokenKind::RParen) {
            let pname = self.expect_ident("parameter name")?;
            self.expect(TokenKind::Colon, "`:` before parameter type")
                .ok()?;
            let ty = self.parse_expr(TYPE_MIN_BP)?;
            params.push(Param { name: pname, ty });
            self.skip_newlines();
            if !self.eat(TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        self.expect(TokenKind::RParen, "`)` after parameters")
            .ok()?;
        let ret = if self.eat(TokenKind::Colon) {
            Some(self.parse_expr(TYPE_MIN_BP)?)
        } else {
            None
        };
        let effects = self.parse_effects()?;
        let kind = match flavor {
            FnFlavor::Fn => FnKind::Fn,
            FnFlavor::Spec => FnKind::Spec,
            FnFlavor::Impl => {
                self.skip_newlines();
                self.expect(TokenKind::Refines, "`refines` after impl signature")
                    .ok()?;
                FnKind::Impl {
                    refines: self.expect_ident("spec name after `refines`")?,
                }
            }
        };
        self.skip_newlines();
        let body = self.parse_block()?;
        Some(self.ast.push_item(
            Item::Fn {
                name,
                kind,
                params,
                ret,
                effects,
                body,
            },
            lo..self.prev_end,
        ))
    }

    /// `schedule { urgency a > b \n conflict_free { a, b } }`
    /// Directive names are contextual identifiers, not keywords.
    fn parse_schedule(&mut self) -> Option<ItemId> {
        let lo = self.cur_span().start;
        self.bump(); // schedule
        self.skip_newlines();
        self.expect(TokenKind::LBrace, "`{` after `schedule`")
            .ok()?;
        let mut directives = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Some(TokenKind::RBrace) => {
                    self.bump();
                    break;
                }
                None => {
                    self.error_here("unclosed schedule block".to_string());
                    break;
                }
                _ => {
                    let before = self.pos;
                    if let Some(directive) = self.parse_schedule_directive() {
                        directives.push(directive);
                    }
                    if self.pos == before {
                        self.bump();
                    }
                }
            }
        }
        Some(
            self.ast
                .push_item(Item::Schedule { directives }, lo..self.prev_end),
        )
    }

    fn parse_schedule_directive(&mut self) -> Option<ScheduleDirective> {
        let name = self.expect_ident("`urgency` or `conflict_free`")?;
        let directive = match name.text.as_str() {
            "urgency" => {
                // `urgency a > b > c` — at least two names.
                let mut names = vec![self.expect_ident("rule name")?];
                while self.eat(TokenKind::Gt) {
                    names.push(self.expect_ident("rule name after `>`")?);
                }
                if names.len() < 2 {
                    self.error_here("`urgency` needs at least two rules (`a > b`)".to_string());
                }
                ScheduleDirective::Urgency(names)
            }
            "conflict_free" => {
                self.expect(TokenKind::LBrace, "`{` after `conflict_free`")
                    .ok()?;
                let mut names = vec![self.expect_ident("rule name")?];
                while self.eat(TokenKind::Comma) {
                    names.push(self.expect_ident("rule name")?);
                }
                self.expect(TokenKind::RBrace, "`}` closing `conflict_free`")
                    .ok()?;
                ScheduleDirective::ConflictFree(names)
            }
            _ => {
                self.error_here(format!(
                    "unknown schedule directive `{name}` (expected `urgency` or `conflict_free`)"
                ));
                self.sync();
                return None;
            }
        };
        self.expect_terminator();
        Some(directive)
    }

    /// `<name (args)?, ...>` — e.g. `<suspends, reads {pc, mem}>`.
    /// Effect names are contextual identifiers, not keywords.
    fn parse_effects(&mut self) -> Option<Vec<Effect>> {
        let mut effects = Vec::new();
        if !self.eat(TokenKind::Lt) {
            return Some(effects);
        }
        loop {
            let name = self.expect_ident("effect name")?;
            let mut args = Vec::new();
            if self.eat(TokenKind::LBrace) {
                loop {
                    args.push(self.expect_ident("state name")?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RBrace, "`}` closing effect arguments")
                    .ok()?;
            }
            effects.push(Effect { name, args });
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::Gt, "`>` closing effect list").ok()?;
        Some(effects)
    }

    /// Name positions accept `reg`/`mem`/`fifo`/`input`/`output` too: they
    /// are keywords only at item-declaration position. DESIGN.md itself
    /// writes `reads {mem}`.
    fn at_name(&self) -> bool {
        matches!(
            self.peek(),
            Some(TokenKind::Ident)
                | Some(TokenKind::Reg)
                | Some(TokenKind::Mem)
                | Some(TokenKind::Fifo)
                | Some(TokenKind::Input)
                | Some(TokenKind::Output)
                | Some(TokenKind::Inst)
        )
    }

    fn expect_ident(&mut self, what: &str) -> Option<Name> {
        if self.at_name() {
            let span = self.bump().unwrap().span;
            Some(Name {
                text: self.text(&span).to_string(),
                span,
            })
        } else {
            self.error_here(format!("expected {what}"));
            self.sync();
            None
        }
    }

    // --- statements ---

    fn parse_block(&mut self) -> Option<Vec<StmtId>> {
        self.expect(TokenKind::LBrace, "`{` opening a block").ok()?;
        let mut body = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Some(TokenKind::RBrace) => {
                    self.bump();
                    break;
                }
                None => {
                    self.error_here("unclosed block".to_string());
                    break;
                }
                _ => {
                    let before = self.pos;
                    if let Some(stmt) = self.parse_stmt() {
                        body.push(stmt);
                    }
                    if self.pos == before {
                        self.bump();
                    }
                }
            }
        }
        Some(body)
    }

    fn parse_stmt(&mut self) -> Option<StmtId> {
        use TokenKind::*;
        let lo = self.cur_span().start;
        let stmt = match self.peek() {
            Some(Tick) => {
                self.bump();
                self.expect_terminator();
                Stmt::Tick
            }
            Some(Let) => {
                self.bump();
                let name = self.expect_ident("binding name")?;
                self.expect(Eq, "`=` after `let` name").ok().or_else(|| {
                    self.sync();
                    None
                })?;
                let init = self.parse_expr(0)?;
                self.expect_terminator();
                Stmt::Let { name, init }
            }
            Some(Return) => {
                self.bump();
                let value = match self.peek() {
                    Some(Newline) | Some(Semi) | Some(RBrace) | None => None,
                    _ => Some(self.parse_expr(0)?),
                };
                self.expect_terminator();
                Stmt::Return(value)
            }
            Some(If) => self.parse_if()?,
            Some(While) => {
                self.bump();
                let cond = self.parse_expr(0)?;
                let body = self.parse_block()?;
                self.expect_terminator();
                Stmt::While { cond, body }
            }
            _ => {
                let lhs = self.parse_expr(0)?;
                if self.eat(ColonEq) {
                    let rhs = self.parse_expr(0)?;
                    self.expect_terminator();
                    Stmt::Assign { lhs, rhs }
                } else {
                    self.expect_terminator();
                    Stmt::Expr(lhs)
                }
            }
        };
        Some(self.ast.push_stmt(stmt, lo..self.prev_end))
    }

    fn parse_if(&mut self) -> Option<Stmt> {
        self.bump(); // if
        let cond = self.parse_expr(0)?;
        let then_body = self.parse_block()?;
        let else_body = if self.eat(TokenKind::Else) {
            if self.at(TokenKind::If) {
                // `else if`: wrap the nested if as a one-statement block.
                let lo = self.cur_span().start;
                let nested = self.parse_if()?;
                let id = self.ast.push_stmt(nested, lo..self.prev_end);
                Some(vec![id])
            } else {
                let block = self.parse_block()?;
                self.expect_terminator();
                Some(block)
            }
        } else {
            self.expect_terminator();
            None
        };
        Some(Stmt::If {
            cond,
            then_body,
            else_body,
        })
    }

    // --- expressions (Pratt) ---

    fn parse_expr(&mut self, min_bp: u8) -> Option<ExprId> {
        use TokenKind::*;
        let lo = self.cur_span().start;

        let mut lhs = match self.peek() {
            Some(Ident) => {
                let span = self.bump().unwrap().span;
                let name = self.text(&span).to_string();
                self.ast.push_expr(Expr::Ident(name), span)
            }
            // `sync`/`race` are keywords but appear in call position, and
            // `reg`/`mem`/`fifo`/`input`/`output` are keywords only at
            // declaration position (`mem[addr]` is an ordinary read). All
            // become plain idents.
            Some(Sync) | Some(Race) | Some(Reg) | Some(Mem) | Some(Fifo) | Some(Input)
            | Some(Output) => {
                let tok = self.bump().unwrap();
                let name = self.text(&tok.span).to_string();
                self.ast.push_expr(Expr::Ident(name), tok.span)
            }
            Some(Underscore) => {
                let span = self.bump().unwrap().span;
                self.ast.push_expr(Expr::Wildcard, span)
            }
            Some(Int) => {
                let span = self.bump().unwrap().span;
                let value = parse_int(self.text(&span));
                let value = match value {
                    Some(v) => v,
                    None => {
                        self.errors.push(ParseError {
                            span: span.clone(),
                            message: "integer literal too large".to_string(),
                        });
                        0
                    }
                };
                self.ast.push_expr(Expr::Int(value), span)
            }
            Some(LParen) => {
                self.bump();
                self.skip_newlines();
                let inner = self.parse_expr(0)?;
                self.skip_newlines();
                self.expect(RParen, "`)`").ok()?;
                inner
            }
            Some(Spawn) => {
                self.bump();
                let inner = self.parse_expr(PREFIX_BP)?;
                self.ast.push_expr(Expr::Spawn(inner), lo..self.prev_end)
            }
            Some(Minus) => self.parse_prefix(UnOp::Neg)?,
            Some(Bang) => self.parse_prefix(UnOp::Not)?,
            Some(Tilde) => self.parse_prefix(UnOp::BitNot)?,
            _ => {
                self.error_here("expected an expression".to_string());
                self.sync();
                return None;
            }
        };

        loop {
            let Some(kind) = self.peek() else { break };

            // Postfix operators bind tightest.
            if POSTFIX_BP >= min_bp {
                match kind {
                    LParen => {
                        self.bump();
                        let args = self.parse_args(RParen)?;
                        lhs = self
                            .ast
                            .push_expr(Expr::Call { callee: lhs, args }, lo..self.prev_end);
                        continue;
                    }
                    LBracket => {
                        self.bump();
                        let args = self.parse_args(RBracket)?;
                        lhs = self
                            .ast
                            .push_expr(Expr::Bracket { callee: lhs, args }, lo..self.prev_end);
                        continue;
                    }
                    Dot => {
                        self.bump();
                        let name = self.expect_ident("field name")?;
                        lhs = self.ast.push_expr(
                            Expr::Field {
                                base: lhs,
                                name: name.text,
                            },
                            lo..self.prev_end,
                        );
                        continue;
                    }
                    Question => {
                        self.bump();
                        lhs = self.ast.push_expr(Expr::Guard(lhs), lo..self.prev_end);
                        continue;
                    }
                    _ => {}
                }
            }

            let Some((l_bp, r_bp)) = infix_bp(kind) else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            self.bump();
            let rhs = self.parse_expr(r_bp)?;
            lhs = self.ast.push_expr(
                Expr::Binary {
                    op: binop_of(kind),
                    lhs,
                    rhs,
                },
                lo..self.prev_end,
            );
        }

        Some(lhs)
    }

    fn parse_prefix(&mut self, op: UnOp) -> Option<ExprId> {
        let lo = self.cur_span().start;
        self.bump();
        let operand = self.parse_expr(PREFIX_BP)?;
        Some(
            self.ast
                .push_expr(Expr::Unary { op, operand }, lo..self.prev_end),
        )
    }

    /// Argument list after an already-consumed `(` or `[`. Newlines are
    /// allowed around arguments; the closing delimiter ends the list.
    fn parse_args(&mut self, close: TokenKind) -> Option<Vec<ExprId>> {
        let mut args = Vec::new();
        self.skip_newlines();
        while !self.at(close) {
            args.push(self.parse_expr(0)?);
            self.skip_newlines();
            if !self.eat(TokenKind::Comma) {
                break;
            }
            self.skip_newlines();
        }
        let what = if close == TokenKind::RParen {
            "`)`"
        } else {
            "`]`"
        };
        self.expect(close, what).ok()?;
        Some(args)
    }
}

fn parse_int(text: &str) -> Option<u64> {
    let text = text.replace('_', "");
    if let Some(hex) = text.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = text.strip_prefix("0b") {
        u64::from_str_radix(bin, 2).ok()
    } else {
        text.parse().ok()
    }
}
